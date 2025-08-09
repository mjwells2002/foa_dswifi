use alloc::vec;
use alloc::vec::Vec;
use core::cmp::PartialEq;
use core::slice::SlicePattern;
use defmt::{error, info, warn, Format};
use embassy_futures::join::join3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{Channel, DynamicReceiver, DynamicSender};
use embassy_sync::mutex::Mutex;
use ieee80211::mac_parser::MACAddress;
use ieee80211::scroll::{Pread, Pwrite};
use ieee80211::scroll::ctx::MeasureWith;
use crate::{DsWiFiClientEvent, DsWiFiControl, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWifiClientMask};
use crate::DsWiFiControlEvent::FrameGenerated;
use crate::packets::HostToClientFlags;
use crate::pictochat_application::PictochatHandshakePhase::{Connected, Handshake};
use crate::pictochat_packets::{ConsoleIdPayload, MessagePayload, PictochatHeader, PictochatType1, PictochatType2, PictochatType45};
use crate::runner::PendingDataFrame;

const MAX_TRANSFER_SIZE: u16 = 20480;
const MESSAGE_CHUNK_SIZE: u16 = 180; // 180 for DS Lite, 250 for 2DS with TWiLight Menu, 250+ renders on 2DS but its very broken

#[derive(PartialEq,Debug,Format,Clone)]
pub enum PictochatHandshakePhase {
    Handshake,
    Connected,
}

#[derive(Clone)]
pub struct PictochatUser {
    pub mac: MACAddress,
    pub mask: DsWifiClientMask,
    pub phase: PictochatHandshakePhase,
    pub console_id: Option<ConsoleIdPayload>,
    pub id: u8,
}

pub struct PictoChatUserManager {
    pub users: [Option<PictochatUser>; 15]
}


#[derive(Debug, Eq, PartialEq)]
pub enum PictoChatState {
    Idle,
    NewClientPending,
    IdentConsole((MACAddress,u8,u8)),
    IdentConsoleInternalStage13(u8),
    IdentConsoleInternalStage24((MACAddress,[u8; 2])),
    RequestIdent(u8),
    EchoTransfer(Vec<u8>),
    SendMessage(i32)
}
impl PictoChatUserManager {
    pub fn add_user(&mut self, user: PictochatUser) {
        let first_empty = self.users.iter().position(|x| x.is_none() || x.as_ref().unwrap().mac == user.mac);
        if let Some(index) = first_empty {
            self.users[index] = Some(user);
        } else {
            panic!("Too many users");
        }
    }

    pub fn remove_user(&mut self, mac: MACAddress) {
        for i in 0..15 {
            if let Some(user) = &self.users[i] {
                if user.mac == mac {
                    self.users[i] = None;
                }
            }
        }
    }

    pub fn get_user(&self, mac: MACAddress) -> Option<&PictochatUser> {
        for i in 0..15 {
            if let Some(user) = &self.users[i] {
                if user.mac == mac {
                    return Some(user);
                }
            }
        }
        None
    }

    pub fn update_user(&mut self, newuser: PictochatUser) {
        for i in 0..15 {
            if let Some(user) = &self.users[i] {
                if user.mac == newuser.mac {
                    self.users[i] = Option::from(newuser.clone());
                }
            }
        }
    }
}

pub enum PictochatInterfaceEvent {
    ClientConnected(ConsoleIdPayload),
    ClientDisconnected(ConsoleIdPayload)
}
pub struct PictochatSharedData {
    message_queue_inbound: Channel<NoopRawMutex, MessagePayload, 1>,
    message_queue_outbound: Channel<NoopRawMutex, MessagePayload, 1>,
    event_queue: Channel<NoopRawMutex, PictochatInterfaceEvent, 5>,
}
impl Default for PictochatSharedData {
    fn default() -> Self {
        Self {
            message_queue_inbound: Channel::new(),
            message_queue_outbound: Channel::new(),
            event_queue: Channel::new(),
        }
    }
}
pub struct PictochatInterface<'res> {
    pub inbound_queue: DynamicReceiver<'res, MessagePayload>,
    pub outbound_queue: DynamicSender<'res, MessagePayload>,
    pub event_queue: DynamicReceiver<'res, PictochatInterfaceEvent>
}
pub struct PictochatExternalInterface<'res> {
    inbound_queue: DynamicSender<'res, MessagePayload>,
    outbound_queue: DynamicReceiver<'res, MessagePayload>,
    event_queue: DynamicSender<'res, PictochatInterfaceEvent>
}

struct PictochatInflightDataTransfer {
    inflight_data: Option<Vec<u8>>,
    inflight_data_tx: Option<Vec<u8>>,
}

pub struct PictoChatApplication<'res> {
    ds_wifi_control: DsWiFiControl<'res>,
    user_state_manager: Mutex<NoopRawMutex, PictoChatUserManager>,
    state_queue: Channel<NoopRawMutex, PictoChatState, 20>,
    pictochat_external_interface: PictochatExternalInterface<'res>,
    inflight_data: Mutex<NoopRawMutex, PictochatInflightDataTransfer>,
}

impl<'res> PictoChatApplication<'res> {
    pub async fn new(ds_wi_fi_control: DsWiFiControl<'res>, pictochat_shared_data: &'res mut PictochatSharedData) -> (Self,PictochatInterface<'res>) {

        let pictochat_app = PictoChatApplication {
            ds_wifi_control: ds_wi_fi_control,
            user_state_manager: Mutex::new(PictoChatUserManager {
                users: [const { None };15],
            }),
            state_queue: Channel::new(),
            pictochat_external_interface: PictochatExternalInterface {
                inbound_queue: pictochat_shared_data.message_queue_inbound.dyn_sender(),
                outbound_queue: pictochat_shared_data.message_queue_outbound.dyn_receiver(),
                event_queue: pictochat_shared_data.event_queue.dyn_sender(),
            },
            inflight_data: Mutex::from(PictochatInflightDataTransfer {
                inflight_data: None,
                inflight_data_tx: None,
            }),
        };

        let interface = {
            PictochatInterface {
                inbound_queue: pictochat_shared_data.message_queue_inbound.dyn_receiver(),
                outbound_queue: pictochat_shared_data.message_queue_outbound.dyn_sender(),
                event_queue: pictochat_shared_data.event_queue.dyn_receiver(),
            }
        };

        (pictochat_app,interface)
    }

    async fn generate_idle_frame(&self, frame: &mut PendingDataFrame, id: u16) {
        let mut idle = PictochatType45 {
            header: PictochatHeader {
                type_id: id,
                size_with_header: 104,
            },
            ..Default::default()
        };
        idle.members[0] = MACAddress::from(self.ds_wifi_control.mac_address);
        let user_manager = self.user_state_manager.lock().await;
        for i in 0..15 {
            if let Some(user) = &user_manager.users[i] {
                idle.members[i+1] = user.mac;
            }
        }
        let written = frame.data.pwrite(idle, 0).unwrap();
        //info!("Idle frame written: {:?}", written);
        frame.flags = HostToClientFlags::from_bits(28).unwrap();

        frame.size = written as u16;
    }

    async fn get_state(&self) -> PictoChatState {
        let pending = self.state_queue.try_receive();
        if let Ok(state) = pending {
            state
        } else {
            match self.pictochat_external_interface.outbound_queue.try_receive() {
                Ok(data) => {
                    let mut inflight = self.inflight_data.lock().await;
                    let len = data.measure_with(&());
                    let mut p_inflight = vec![0u8; len];
                    p_inflight.pwrite(data, 0).expect("TODO: panic message");
                    inflight.inflight_data_tx = Option::from(p_inflight);
                    info!("about to send");
                    PictoChatState::SendMessage(-1)
                }
                Err(_) => {
                    PictoChatState::Idle
                }
            }
        }
    }
    async fn tx_wait_loop(&self) -> ! {
        loop {
            self.ds_wifi_control.data_tx_signal.wait().await;
            self.ds_wifi_control.data_tx_signal.reset();
            let mut tx_out = self.ds_wifi_control.data_tx_mutex.lock().await;
            match self.get_state().await {
                PictoChatState::Idle => {
                    self.generate_idle_frame(&mut tx_out, 5).await;
                }
                PictoChatState::NewClientPending => {
                    tx_out.flags = HostToClientFlags::from_bits(28).unwrap();
                    self.generate_idle_frame(&mut tx_out, 4).await;
                }
                PictoChatState::EchoTransfer(echo) => {
                    tx_out.flags = HostToClientFlags::from_bits(if echo.len() > 25  {158} else {29}).unwrap();
                    tx_out.data[..echo.len()].copy_from_slice(echo.as_slice());
                    tx_out.size = echo.len() as u16;
                    //info!("echo len: {}",echo.len())
                }
                PictoChatState::IdentConsole((mac,_ident_type,_other_ident_type)) => {
                    if self.state_queue.free_capacity() > 4 {
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13(0)).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((mac,[0x03,0x00]))).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13(0)).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((mac,[0x03,0x01]))).expect("Failed to send state");
                        self.generate_idle_frame(&mut tx_out, 5).await;
                    } else {
                        panic!("Not enough space in queue");
                    }
                }
                PictoChatState::RequestIdent(id) => {
                    tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                    let ident = PictochatType1 {
                        sender_id: id,
                        data_size: 84,
                        ..Default::default()
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                }
                PictoChatState::IdentConsoleInternalStage13(data_type) => {
                    tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                    let ident = PictochatType1 {
                        sender_id: 0,
                        data_type,
                        data_size: 84,
                        ..Default::default()
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                }
                PictoChatState::IdentConsoleInternalStage24((mac,data)) => {
                    tx_out.flags = HostToClientFlags::from_bits(30).unwrap();
                    let mut payload_bytes = [0u8;84];
                    let mut payload = ConsoleIdPayload {
                        magic: data,
                        to: mac,
                        ..Default::default()
                    };
                    payload.write_name("host");
                    payload_bytes.pwrite(payload, 0).unwrap();

                    let ident = PictochatType2 {
                        header: PictochatHeader {
                            type_id: 2,
                            size_with_header: 96,
                        },
                        sending_console_id: 0,
                        payload_type: 5,
                        transfer_flags: 1,
                        write_offset: 0,
                        magic: [0,0],
                        payload: payload_bytes.to_vec(),
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                },
                PictoChatState::SendMessage(offset) => {
                    let mut inflight = self.inflight_data.lock().await;
                    let tx_buf = inflight.inflight_data_tx.take().unwrap();
                    if offset < 0 {
                        tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                        let ident = PictochatType1 {
                            sender_id: 0,
                            data_size: tx_buf.len() as u16,
                            magic_2: [0x00, 0x00,
                                0x58, 0x2b, 0x00, 0x03,
                                0xdb, 0xa2, 0xfa, 0xea],
                            ..Default::default()
                        };
                        let written = tx_out.data.pwrite(ident, 0).unwrap();
                        tx_out.size = written as u16;
                        inflight.inflight_data_tx = Some(tx_buf);
                        self.state_queue.try_send(PictoChatState::SendMessage(0)).expect("TODO: panic message");
                    } else {
                        let data_size = if tx_buf.len() as u16 - (offset as u16) > MESSAGE_CHUNK_SIZE { MESSAGE_CHUNK_SIZE  } else { tx_buf.len() as u16 - (offset as u16)  } as u16;
                        let tx_buf_subslice = &tx_buf[offset as usize..][..data_size as usize];
                        let is_last_fragment = offset as usize + data_size as usize >= tx_buf.len();
                        let data_fragment = PictochatType2 {
                            header: PictochatHeader {
                                type_id: 2,
                                size_with_header: 12 + data_size,
                            },
                            sending_console_id: 0,
                            payload_type: if is_last_fragment {
                                0x04
                            } else {
                                if offset == 0 {
                                    0xff
                                } else {
                                    0x97
                                }
                            },
                            transfer_flags: if is_last_fragment { 0x01 } else { 0x00 },
                            write_offset: offset as u16,
                            magic: [0x00,0x00],
                            payload: tx_buf_subslice.to_vec()
                        };
                        tx_out.flags = if is_last_fragment {
                            HostToClientFlags::from_bits(158).unwrap()
                        } else {
                            HostToClientFlags::from_bits(30).unwrap()
                        };
                        let written = tx_out.data.pwrite(data_fragment, 0).unwrap();
                        tx_out.size = written as u16;
                        if is_last_fragment {
                            self.state_queue.try_send(PictoChatState::Idle).expect("TODO: panic message");
                            info!("sent message");
                        } else {
                            self.state_queue.try_send(PictoChatState::SendMessage(offset + data_size as i32)).expect("TODO: panic message");
                        }
                        if !is_last_fragment {
                            inflight.inflight_data_tx = Some(tx_buf);
                        }
                    }

                }
            }

            self.ds_wifi_control.data_tx_signal_2.signal(FrameGenerated);
        }
    }
    async fn rx_wait_loop(&self) -> ! {
        loop {
            let (data_raw,id,mac,_) = self.ds_wifi_control.data_rx.receive().await;
            //info!("Received data: {}", data_raw[0]);
            let header: PictochatHeader = data_raw.pread(0).unwrap();
            //info!("Header: {:?}", header.type_id);
            if header.type_id == 6 {
                let mut user_state_manager = self.user_state_manager.lock().await;
                user_state_manager.add_user(PictochatUser {
                    mac,
                    mask: id,
                    phase: PictochatHandshakePhase::Handshake,
                    console_id: None,
                    id: id.trailing_zeros() as u8,
                });

                self.state_queue.try_send(PictoChatState::NewClientPending).expect("Failed to send state");

            } else if header.type_id == 0 {
                let mut veccy_mc_vec_face = vec![0u8; header.size_with_header as usize];
                veccy_mc_vec_face.copy_from_slice(data_raw[..header.size_with_header as usize].as_slice());
                veccy_mc_vec_face[0] = 1;
                let parsed: PictochatType1 = veccy_mc_vec_face.as_slice().pread(0).unwrap();
                if parsed.data_size < MAX_TRANSFER_SIZE {
                    let mut inflight = self.inflight_data.lock().await;
                    if inflight.inflight_data.is_none() {
                        info!("transfer started, allocating buffer");
                        inflight.inflight_data = Some(vec![0u8; parsed.data_size as usize]);
                    } else {
                        warn!("transfer started when inflight data present, reallocating buffer");
                        let mut buf = inflight.inflight_data.take().unwrap();
                        buf.resize(parsed.data_size as usize, 0);
                        inflight.inflight_data = Some(buf);
                    }
                }
                self.state_queue.try_send(PictoChatState::EchoTransfer(veccy_mc_vec_face)).unwrap()
            } else if header.type_id == 2 {
                let parsed: PictochatType2 = data_raw.pread(0).unwrap();
                //info!("data fragment, size {}, offset {}, {}", parsed.payload.len(), parsed.write_offset, header.size_with_header);
                let mut veccy_mc_vec_face = vec![0u8; header.size_with_header as usize];
                veccy_mc_vec_face.copy_from_slice(data_raw[..header.size_with_header as usize].as_slice());
                self.state_queue.try_send(PictoChatState::EchoTransfer(veccy_mc_vec_face)).unwrap();
                {
                    let mut inflight = self.inflight_data.lock().await;
                    if inflight.inflight_data.is_some() {
                        let mut inflight_buf = inflight.inflight_data.take().unwrap();
                        if parsed.write_offset as usize + parsed.payload.len() <= inflight_buf.len(){
                            let start = parsed.write_offset as usize;
                            let end = start + parsed.payload.len();
                            inflight_buf.as_mut_slice()[start..end].copy_from_slice(parsed.payload.as_slice());
                        } else {
                            warn!("ignoring data chunk for inflight transfer as it would overflow buffer, buffer_len: {}, write_offset: {} chunk_size: {}",inflight_buf.len(),parsed.write_offset, parsed.payload.len());
                        }
                        inflight.inflight_data = Some(inflight_buf);
                    } else {
                        error!("no buffer in place for transfer, {} {} {}", header.size_with_header, parsed.write_offset, parsed.payload.len())
                    }
                }

                if parsed.transfer_flags == 1 {
                    let mut inflight = self.inflight_data.lock().await;
                    if inflight.inflight_data.is_some() {
                        let buf = inflight.inflight_data.take().unwrap();
                        if buf[1] == 1 ||  buf[1] == 0 {
                            let consoleid: ConsoleIdPayload = buf.as_slice().pread(0).unwrap();
                            let mut user_state_manager = self.user_state_manager.lock().await;
                            let mut user = user_state_manager.get_user(mac).unwrap().clone();
                            if user.phase == Handshake {
                                user.phase = Connected;
                                user.console_id = Some(consoleid.clone());
                                user_state_manager.update_user(user);
                                info!("client now connected name: {}",consoleid.name);

                                match self.pictochat_external_interface.event_queue.try_send(PictochatInterfaceEvent::ClientConnected(consoleid.clone())) {
                                    Ok(_) => {}
                                    Err(_) => {
                                        warn!("failed to send event to external interface")
                                    }
                                }

                                self.state_queue.try_send(PictoChatState::Idle).unwrap();
                                self.state_queue.try_send(PictoChatState::IdentConsole((MACAddress::from(self.ds_wifi_control.mac_address),0xD0,0))).unwrap();
                                self.state_queue.try_send(PictoChatState::Idle).unwrap();
                                self.state_queue.try_send(PictoChatState::RequestIdent(id.trailing_zeros() as u8)).unwrap();
                            }
                        } else {
                            let message: MessagePayload = buf.as_slice().pread(0).unwrap();
                            match self.pictochat_external_interface.inbound_queue.try_send(message) {
                                Ok(_) => {}
                                Err(_) => {
                                    warn!("failed to send message to external interface")
                                }
                            }
                        }
                    } else {
                        warn!("finished transfer with no buffer?")
                    }


                }
            }
        }
    }
    async fn event_wait_loop(&self) -> ! {
        loop {
            let client_event = self.ds_wifi_control.event_rx.receive().await;
            match client_event {
                DsWiFiClientEvent::Connected(_mac) => {
                    //info!("Client Connected: {:?}", mac);


                },
                DsWiFiClientEvent::Disconnected(mac) => {
                    //info!("Client Disconnected: {:?}", mac);
                    let mut user_state_manager = self.user_state_manager.lock().await;
                    match user_state_manager.get_user(MACAddress::from(mac)) {
                        None => {}
                        Some(user) => {
                            let id = user.console_id.clone();
                            match id {
                                None => {}
                                Some(console_id) => {
                                    match self.pictochat_external_interface.event_queue.try_send(PictochatInterfaceEvent::ClientDisconnected(console_id)) {
                                        Ok(_) => {}
                                        Err(_) => {
                                            warn!("failed to send event to external interface")
                                        }
                                    }
                                }
                            }
                        }
                    }
                    user_state_manager.remove_user(MACAddress::from(mac));

                }
            }
        }
    }
    pub async fn run(&mut self) -> ! {
        match self.ds_wifi_control.control_requester.send_request_and_wait(DsWiFiInterfaceControlEvent::SetChannel(7)).await {
            DsWiFiInterfaceControlEventResponse::Success => {
                info!("Set Channel to 7");
            },
            DsWiFiInterfaceControlEventResponse::Failed => {
                error!("Failed to set channel");
            }
        };

        match self.ds_wifi_control.control_requester.send_request_and_wait(DsWiFiInterfaceControlEvent::SetBeaconsEnabled(true)).await {
            DsWiFiInterfaceControlEventResponse::Success => {
                info!("Set Beacons enabled");
            },
            DsWiFiInterfaceControlEventResponse::Failed => {
                error!("Failed to set beacons enabled");
            }
        };

        join3(self.tx_wait_loop(), self.rx_wait_loop(), self.event_wait_loop()).await;
        unreachable!()
    }


}