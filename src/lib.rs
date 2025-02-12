#![no_std]
#![feature(core_intrinsics)]
#![feature(trivial_bounds)]
#![feature(slice_pattern)]
#![feature(future_join)]
extern crate alloc;

pub mod runner;
mod packets;
mod pictochat_packets;
pub mod pictochat_application;

use core::ffi::c_void;
use core::future::Future;
use core::marker::PhantomData;
use core::ops::{BitAndAssign, BitOrAssign};
use core::sync::atomic::{AtomicU16, AtomicUsize};
use defmt::{error, info, warn, Format};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{Channel, DynamicReceiver, DynamicSender};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};
use foa::{VirtualInterface};
use foa::esp_wifi_hal::BorrowedBuffer;
use foa::esp_wifi_hal::RxFilterBank::{ReceiverAddress, BSSID};
use foa::lmac::{LMacInterfaceControl};
use hex_literal::hex;
use ieee80211::common::{AssociationID, CapabilitiesInformation, DataFrameSubtype, FCFFlags, FrameType, ManagementFrameSubtype, SequenceControl};
use ieee80211::{element_chain, match_frames, scroll, supported_rates, GenericFrame};
use ieee80211::data_frame::{DataFrame, DataFrameReadPayload};
use ieee80211::data_frame::header::DataFrameHeader;
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{BeaconFrame, ManagementFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::BeaconBody;
use ieee80211::scroll::ctx::{MeasureWith, TryFromCtx, TryIntoCtx};
use ieee80211::scroll::Pwrite;
use crate::packets::{ClientToHostDataFrame};
use crate::runner::{DsWiFiRunner, PendingDataFrame};

pub struct DsWiFiInterface;

const MAX_CLIENTS: usize = 15;

pub struct RequestResponseSignal<Request, Response> {
    request_signal: Signal<NoopRawMutex, Request>,
    response_signal: Signal<NoopRawMutex, Response>,
}

pub struct Requester<'a, Request, Response> {
    request_signal: &'a Signal<NoopRawMutex, Request>,
    response_signal: &'a Signal<NoopRawMutex, Response>,
}

pub struct Responder<'a, Request, Response> {
    request_signal: &'a Signal<NoopRawMutex, Request>,
    response_signal: &'a Signal<NoopRawMutex, Response>,
}

impl<'a, Request, Response> Requester<'a, Request, Response> {
    pub async fn send_request_and_wait(&self, request: Request) -> Response {
        if self.request_signal.signaled() {
            panic!("Request already sent");
        }
        self.request_signal.signal(request);
        let res = self.response_signal.wait().await;
        self.response_signal.reset();
        res
    }

}

impl<'a, Request, Response> Responder<'a, Request, Response> {
    pub async fn wait_for_request(&self) -> Request {
        self.request_signal.wait().await
    }
    pub fn send_response(&self, response: Response) {
        self.response_signal.signal(response);
    }
}
impl<Request, Response> RequestResponseSignal<Request, Response> {
    pub fn new() -> Self {
        Self {
            request_signal: Signal::new(),
            response_signal: Signal::new(),
        }
    }

    pub fn get_requester(&self) -> Requester<Request,Response> {
        Requester {
            request_signal: &self.request_signal,
            response_signal: &self.response_signal,
        }
    }

    pub fn get_responder(&self) -> Responder<Request,Response> {
        Responder {
            request_signal: &self.request_signal,
            response_signal: &self.response_signal,
        }
    }
}
extern "C" {
    pub fn chip_v7_set_chan_nomac(channel: u8, idk: u8);
    fn phy_set_most_tpw(max_txpwr: i8) -> c_void;
}
pub struct DsWiFiClientManager {
    pub clients: [Option<DsWiFiClient>; MAX_CLIENTS],
    pub all_clients_mask: DsWifiClientMask,
    pub current_mask: DsWifiClientMask,
}
impl DsWiFiClientManager {
    pub fn get_next_client_aid(&self) -> Option<AssociationID> {
        for i in 0..MAX_CLIENTS {
            if self.clients[i].is_none() {
                return Some(AssociationID::from((i + 1) as u16));
            }
        }
        None
    }

    pub fn has_client(&self, macaddress: MACAddress) -> bool {
        self.clients
            .iter()
            .flatten()
            .any(|client| macaddress.eq(&MACAddress::from(client.associated_mac_address)))
    }

    pub fn get_client(&self, macaddress: MACAddress) -> Option<&DsWiFiClient> {
        self.clients
            .iter()
            .flatten()
            .find(|client| macaddress.eq(&MACAddress::from(client.associated_mac_address)))
    }

    pub fn get_client_mut(&mut self, macaddress: MACAddress) -> Option<&mut DsWiFiClient> {
        self.clients
            .iter_mut()
            .flatten()
            .find(|client| macaddress.eq(&MACAddress::from(client.associated_mac_address)))
    }

    pub fn add_client(&mut self, client: DsWiFiClient) {
        let aid = client.association_id;
        self.clients[(aid.aid() - 1) as usize] = Some(client);
        if client.state == DsWiFiClientState::Connected {
            self.all_clients_mask.mask_add(aid.get_mask_bits());
        }
    }

    pub fn update_client_state(&mut self, addr: MACAddress, state: DsWiFiClientState) {
        {
            let client = self.get_client_mut(addr).unwrap();
            client.state = state;
            client.last_heard_from = Instant::now();
        }
        let client = self.get_client(addr).unwrap();
        //todo: sanity check new state is possible given previous state
        if state == DsWiFiClientState::Connected {
            self.all_clients_mask.mask_add(client.association_id.get_mask_bits());
        } else {
            self.all_clients_mask.mask_subtract(client.association_id.get_mask_bits());
        }

    }

    pub fn remove_client(&mut self, aid: AssociationID) {
        self.clients[(aid.aid() - 1) as usize] = None;
        self.all_clients_mask.mask_subtract(aid.get_mask_bits());
    }
}
#[derive(Copy)]
#[derive(Clone)]
pub struct DsWiFiClient {
    state: DsWiFiClientState,
    associated_mac_address: [u8; 6],
    association_id: AssociationID,
    last_heard_from: Instant,
}
impl DsWiFiClient {
    pub fn log_client_info(&self) {
        info!("Client: aid: {}, mac: {:?}, state {:?}",self.association_id.aid(),self.associated_mac_address, self.state);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Format)]
pub enum DsWiFiClientState {
    Associating,
    Connected,
}
pub type DsWifiClientMask = u16;
pub trait DsWifiClientMaskMath {
    fn mask_add(&mut self, other: DsWifiClientMask) -> DsWifiClientMask;
    fn mask_subtract(&mut self, other: DsWifiClientMask) -> DsWifiClientMask;
    fn num_clients(&self) -> u8;
    fn is_empty(&self) -> bool;
}
impl DsWifiClientMaskMath for DsWifiClientMask {
    fn mask_add(&mut self, other: DsWifiClientMask) -> DsWifiClientMask {
        self.bitor_assign(other);
        *self
    }

    fn mask_subtract(&mut self, other: DsWifiClientMask) -> DsWifiClientMask {
        self.bitand_assign(!other);
        *self
    }

    fn num_clients(&self) -> u8 {
        self.count_ones() as u8
    }

    fn is_empty(&self) -> bool {
        *self == 0x00u16
    }
}
pub trait DsWifiAidClientMaskBits {
    fn get_mask_bits(&self) -> DsWifiClientMask;
}
impl DsWifiAidClientMaskBits for AssociationID {
    fn get_mask_bits(&self) -> DsWifiClientMask {
        0x0001 << (self.aid() as u8)
    }

}

pub enum DsWiFiInterfaceControlEvent {
    SetChannel(u8),
    SetBeaconsEnabled(bool),
    
}

pub enum DsWiFiInterfaceControlEventResponse {
    Failed,
    Success,
}

pub enum DsWiFiClientEvent {
    Disconnected([u8; 6]),
    Connected([u8; 6]),
}

pub struct DsWiFiSharedResources<'res> {
    client_manager: Mutex<NoopRawMutex, DsWiFiClientManager>,

    interface_control: Option<LMacInterfaceControl<'res>>,

    bg_rx_queue: Channel<NoopRawMutex, BorrowedBuffer<'res>, 4>,
    ack_rx_queue: Channel<NoopRawMutex, (MACAddress, Instant), 4>,

    data_tx_mutex: Mutex<NoopRawMutex,PendingDataFrame>,
    data_queue: Channel<NoopRawMutex, ([u8;300], MACAddress, u16), 4>,
    data_tx_signal: Signal<NoopRawMutex, DsWiFiControlEvent>,
    data_tx_signal_2: Signal<NoopRawMutex, DsWiFiControlEvent>,
    control_channel: RequestResponseSignal<DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse>,
    client_queue: Channel<NoopRawMutex, DsWiFiClientEvent, 4>,
}

impl Default for DsWiFiSharedResources<'_> {
    fn default() -> Self {
        Self {
            client_manager: Mutex::from(DsWiFiClientManager {
                clients: [None; MAX_CLIENTS],
                all_clients_mask: 0x0000,
                current_mask: 0,

            }),
            bg_rx_queue: Channel::new(),
            ack_rx_queue: Channel::new(),
            data_tx_mutex: Mutex::from(PendingDataFrame {
                data: [0;300],
                flags: Default::default(),
                size: 0,
            }),
            data_queue: Channel::new(),
            data_tx_signal: Signal::new(),
            interface_control: None,
            data_tx_signal_2: Signal::new(),
            control_channel: RequestResponseSignal::new(),
            client_queue: Channel::new(),
        }
    }
}

pub enum DsWiFiControlEvent {
    FrameRequired,
    FrameGenerated,
}
pub struct DsWiFiControl<'res> {
    pub data_rx: DynamicReceiver<'res,([u8;300], MACAddress, u16)>,
    pub data_tx_mutex: &'res Mutex<NoopRawMutex, PendingDataFrame>,
    pub data_tx_signal: &'res Signal<NoopRawMutex, DsWiFiControlEvent>,
    pub data_tx_signal_2: &'res Signal<NoopRawMutex, DsWiFiControlEvent>,
    pub control_requester: Requester<'res, DsWiFiInterfaceControlEvent,DsWiFiInterfaceControlEventResponse>,
    pub client_manager: &'res Mutex<NoopRawMutex, DsWiFiClientManager>,
    pub event_rx: DynamicReceiver<'res,DsWiFiClientEvent>,
    pub mac_address: [u8; 6],

}

/*
pub struct DsWiFiInput<'res> {
    mac_address: [u8; 6],
    bg_rx_queue: DynamicSender<'res, BorrowedBuffer<'res>>,
    ack_rx_queue: DynamicSender<'res, (MACAddress, Instant)>,
    data_rx_queue: DynamicSender<'res,([u8;300], MACAddress, u16)>,
}

impl<'res> DsWiFiInput<'res, > {
    async fn interface_input(&mut self, borrowed_buffer: BorrowedBuffer<'res>) {
        //info!("InterfaceInput: {:X?}", borrowed_buffer.mpdu_buffer());
        let rx = Instant::now();
        let Ok(generic_frame) = GenericFrame::new(borrowed_buffer.mpdu_buffer(), false) else {
            return;
        };
        match generic_frame.frame_control_field().frame_type() {
            FrameType::Management(_) => {
                if let Err(_) = self.bg_rx_queue.try_send(borrowed_buffer) {
                    // it will probably retry, this should be non-fatal
                    error!("Failed to send to bg_rx_queue");
                };
            }
            FrameType::Data(data) => {
                match data {
                    DataFrameSubtype::DataCFAck => {
                        //TODO: parse frame and extract the app payload (if present) and forward it to control layer
                        let frame = generic_frame.parse_to_typed::<DataFrame>().unwrap().unwrap();
                        match frame.payload.unwrap() {
                            DataFrameReadPayload::Single(data) => {
                                let (c2h_frame,size) = ClientToHostDataFrame::try_from_ctx(data, ()).unwrap();
                                let rx_ack = Instant::now();
                                //info!("ack delay: {}", (rx_ack - rx).as_micros());
                                if c2h_frame.payload_size > 0 {
                                    let (frame,size) = c2h_frame.payload.unwrap();
                                    self.data_rx_queue.try_send((frame,generic_frame.address_2().unwrap(),size)).expect("todo")
                                }
                            }
                            DataFrameReadPayload::AMSDU(_) => {}
                        }
                        if let Err(_) = self.ack_rx_queue.try_send((generic_frame.address_2().unwrap(),Instant::now())) {
                            error!("Failed to send ack to runner");
                        }
                    }
                    DataFrameSubtype::CFAck => {

                        if let Err(_) = self.ack_rx_queue.try_send((generic_frame.address_2().unwrap(),Instant::now())) {
                            error!("Failed to send ack to runner");
                        }

                    }

                    _ => {
                        warn!("Unhandled data Frame");
                    }
                }
            }
            _ => {
                warn!("Unknown Frame");
            }
        }
    }
}
*/

pub struct DsWiFiInitInfo {}

impl Default for DsWiFiInitInfo {
    fn default() -> Self {
        Self {}
    }
}

pub fn new_ds_wifi_interface<'vif, 'foa>(
    virtual_interface: &'vif mut VirtualInterface<'foa>,
    shared_resources: &'vif mut DsWiFiSharedResources<'foa>) -> (
    DsWiFiControl<'vif>,
    DsWiFiRunner<'vif, 'foa>,
    )
{
    let (interface_control, interface_rx_queue) = virtual_interface.split();
    let mac_address = interface_control.get_factory_mac_for_interface();

    interface_control.lock_channel(7).expect("TODO: panic message");
    unsafe {
        //workaround for power cycling
        phy_set_most_tpw(20);

        //workaround for channel setting
        chip_v7_set_chan_nomac(7,0);
    }
    interface_control.set_filter_parameters(BSSID,mac_address,None);
    interface_control.set_filter_parameters(ReceiverAddress,mac_address,Some([0x00;6]));

    interface_control.set_filter_status(BSSID,true);
    interface_control.set_filter_status(ReceiverAddress,true);

    (
        DsWiFiControl {
            data_rx: shared_resources.data_queue.dyn_receiver(),
            data_tx_mutex: &shared_resources.data_tx_mutex,
            data_tx_signal: &shared_resources.data_tx_signal,
            data_tx_signal_2: &shared_resources.data_tx_signal_2,
            control_requester: shared_resources.control_channel.get_requester(),
            client_manager: &shared_resources.client_manager,
            event_rx: shared_resources.client_queue.dyn_receiver(),
            mac_address
        },
        DsWiFiRunner {
            interface_control,
            client_manager: &shared_resources.client_manager,
            mac_address,
            bg_rx_queue: shared_resources.bg_rx_queue.dyn_receiver(),
            start_time: Instant::now(),
            ack_rx_queue: shared_resources.ack_rx_queue.dyn_receiver(),
            data_tx_mutex: &shared_resources.data_tx_mutex,
            data_tx_signal: &shared_resources.data_tx_signal,
            data_tx_signal_2: &shared_resources.data_tx_signal_2,
            control_responder: shared_resources.control_channel.get_responder(),
            beacons_enabled: Mutex::from(false),
            event_tx: shared_resources.client_queue.dyn_sender(),
            data_seq: AtomicU16::new(0),
            interface_rx_queue,
            bg_rx_queue_sender: shared_resources.bg_rx_queue.dyn_sender(),
            ack_rx_queue_sender: shared_resources.ack_rx_queue.dyn_sender(),
            data_rx_queue_sender: shared_resources.data_queue.dyn_sender(),
        }
    )
}
/*
impl Interface for DsWiFiInterface {
    const NAME: &str = "DS-WiFi";
    type SharedResourcesType<'res> = DsWiFiSharedResources<'res>;
    type ControlType<'res> = DsWiFiControl<'res>;
    type RunnerType<'res> = DsWiFiRunner<'res>;
    type InputType<'res> = DsWiFiInput<'res>;
    type InitInfo = DsWiFiInitInfo;


    async fn new<'res>(
        shared_resources: &'res mut Self::SharedResourcesType<'res>,
        init_info: Self::InitInfo,
        transmit_endpoint: LMacTransmitEndpoint<'res>,
        interface_control: LMacInterfaceControl<'res>,
        mac_address: [u8; 6]
    ) -> (
        Self::ControlType<'res>,
        Self::RunnerType<'res>,
        Self::InputType<'res>,
    ) {
        //interface_control.set_and_lock_channel(7).await.expect("TODO: panic message");

        interface_control.set_filter_parameters(BSSID,mac_address,None);
        interface_control.set_filter_parameters(ReceiverAddress,mac_address,Some([0x00;6]));

        interface_control.set_filter_status(BSSID,true);
        interface_control.set_filter_status(ReceiverAddress,true);

        shared_resources.bg_rx_queue = Channel::new();
        shared_resources.interface_control = Some(interface_control);
        let interface_control = shared_resources.interface_control.as_ref().unwrap();

        (
            DsWiFiControl {
                data_rx: shared_resources.data_queue.dyn_receiver(),
                data_tx_mutex: &shared_resources.data_tx_mutex,
                data_tx_signal: &shared_resources.data_tx_signal,
                data_tx_signal_2: &shared_resources.data_tx_signal_2,
                control_requester: shared_resources.control_channel.get_requester(),
                client_manager: &shared_resources.client_manager,
                event_rx: shared_resources.client_queue.dyn_receiver(),
                mac_address
            },
            DsWiFiRunner {
                transmit_endpoint,
                interface_control,
                client_manager: &shared_resources.client_manager,
                mac_address,
                bg_rx_queue: shared_resources.bg_rx_queue.dyn_receiver(),
                start_time: Instant::now(),
                ack_rx_queue: shared_resources.ack_rx_queue.dyn_receiver(),
                data_tx_mutex: &shared_resources.data_tx_mutex,
                data_tx_signal: &shared_resources.data_tx_signal,
                data_tx_signal_2: &shared_resources.data_tx_signal_2,
                control_responder: shared_resources.control_channel.get_responder(),
                beacons_enabled: Mutex::from(false),
                event_tx: shared_resources.client_queue.dyn_sender(),
                data_seq: AtomicU16::new(0),
            },
            DsWiFiInput {
                transmit_endpoint,
                mac_address,
                bg_rx_queue: shared_resources.bg_rx_queue.dyn_sender(),
                ack_rx_queue: shared_resources.ack_rx_queue.dyn_sender(),
                data_rx_queue: shared_resources.data_queue.dyn_sender(),
            }
        )
    }
}
*/
