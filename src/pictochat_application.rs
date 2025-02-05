use alloc::vec;
use alloc::vec::Vec;
use core::slice::SlicePattern;
use defmt::{error, info};
use embassy_futures::join::join3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Ticker};
use ieee80211::mac_parser::MACAddress;
use ieee80211::scroll::{Endian, Pread, Pwrite};
use crate::{DsWiFiClientEvent, DsWiFiControl, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWifiClientMask, DsWifiClientMaskMath};
use crate::DsWiFiControlEvent::FrameGenerated;
use crate::packets::HostToClientFlags;
use crate::pictochat_packets::{ConsoleIdPayload, PictochatHeader, PictochatType1, PictochatType2, PictochatType45};
use crate::runner::PendingDataFrame;

pub struct PictochatUser {
    pub mac: MACAddress,
    pub mask: DsWifiClientMask,
    pub id: u8,
}

pub struct PictoChatUserManager {
    pub users: [Option<PictochatUser>; 15]
}


#[derive(Debug, Eq, PartialEq)]
pub enum PictoChatState {
    Idle,
    NewClientPending,
    IdentConsole((MACAddress)),
    IdentConsoleInternalStage13,
    IdentConsoleInternalStage24((MACAddress,[u8; 2])),
    RequestIdent(u16),
    EchoTransfer(Vec<u8>),
    TxTransfer,
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
}
pub struct PictoChatApplication<'res> {
    pub ds_wifi_control: DsWiFiControl<'res>,
    pub user_state_manager: Mutex<NoopRawMutex, PictoChatUserManager>,
    pub state_queue: Channel<NoopRawMutex, PictoChatState, 20>
}

impl<'res> PictoChatApplication<'res> {
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
            PictoChatState::Idle
        }
    }
    async fn tx_wait_loop(&self) {
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
                    tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                    tx_out.data[..echo.len()].copy_from_slice(echo.as_slice());
                }
                PictoChatState::TxTransfer => {}
                PictoChatState::IdentConsole((mac)) => {
                    if self.state_queue.free_capacity() > 4 {
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((mac,[0x03,0x00]))).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13).expect("Failed to send state");
                        self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((mac,[0x03,0x01]))).expect("Failed to send state");
                        self.generate_idle_frame(&mut tx_out, 5).await;
                    } else {
                        panic!("Not enough space in queue");
                    }
                }
                PictoChatState::RequestIdent((id)) => {
                    tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                    let ident = PictochatType1 {
                        console_id: id,
                        data_size: 84,
                        ..Default::default()
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                }
                PictoChatState::IdentConsoleInternalStage13 => {
                    tx_out.flags = HostToClientFlags::from_bits(29).unwrap();
                    let ident = PictochatType1 {
                        console_id: 0,
                        data_size: 84,
                        ..Default::default()
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                }
                PictoChatState::IdentConsoleInternalStage24((mac,data)) => {
                    tx_out.flags = HostToClientFlags::from_bits(30).unwrap();
                    let mut payload_bytes = [0u8;84];
                    let payload = ConsoleIdPayload {
                        magic: data,
                        to: mac,
                        ..Default::default()
                    };
                    payload_bytes.pwrite(payload, 0).unwrap();

                    let ident = PictochatType2 {
                        header: PictochatHeader {
                            type_id: 2,
                            size_with_header: 84,
                        },
                        sending_console_id: 0,
                        payload_type: 5,
                        transfer_flags: 1,
                        write_offset: 0,
                        payload: payload_bytes.to_vec(),
                    };
                    let written = tx_out.data.pwrite(ident, 0).unwrap();
                    tx_out.size = written as u16;
                }
            }

            self.ds_wifi_control.data_tx_signal_2.signal(FrameGenerated);
        }
    }
    async fn rx_wait_loop(&self) {
        loop {
            let (data_raw,mac,size) = self.ds_wifi_control.data_rx.receive().await;
            info!("Received data: {}", data_raw[0]);
            let header: PictochatHeader = data_raw.pread(0).unwrap();
            info!("Header: {:?}", header.type_id);
            if header.type_id == 6 {
                let mut user_state_manager = self.user_state_manager.lock().await;
                user_state_manager.add_user(PictochatUser {
                    mac,
                    mask: 1,
                    id: 1,
                });
                self.state_queue.try_send(PictoChatState::NewClientPending).expect("Failed to send state");
                /*
                self.state_queue.try_send(PictoChatState::NewClientPending).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::Idle).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::RequestIdent(1)).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::Idle).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((MACAddress::from(self.ds_wifi_control.mac_address), [0x03,0x00]))).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage13).expect("Failed to send state");
                self.state_queue.try_send(PictoChatState::IdentConsoleInternalStage24((MACAddress::from(self.ds_wifi_control.mac_address),[0x03,0x01]))).expect("Failed to send state");
                */
            } else if header.type_id == 0 {
                let mut veccy_mc_vec_face = vec![0u8; header.size_with_header as usize];
                veccy_mc_vec_face.copy_from_slice(data_raw[..header.size_with_header as usize].as_slice());
                veccy_mc_vec_face[0] = 1;
                self.state_queue.try_send(PictoChatState::EchoTransfer(veccy_mc_vec_face)).unwrap()
            }
        }
    }
    async fn event_wait_loop(&self) {
        loop {
            let client_event = self.ds_wifi_control.event_rx.receive().await;
            match client_event {
                DsWiFiClientEvent::Connected(mac) => {
                    info!("Client Connected: {:?}", mac);

                },
                DsWiFiClientEvent::Disconnected(mac) => {
                    info!("Client Disconnected: {:?}", mac);
                    let mut user_state_manager = self.user_state_manager.lock().await;
                    user_state_manager.remove_user(MACAddress::from(mac));
                }
            }
        }
    }
    pub async fn run(&mut self) {
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
    }
}