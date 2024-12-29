#![no_std]
#![feature(core_intrinsics)]
extern crate alloc;

mod runner;
mod packets;
mod pictochat_packets;

use core::future::Future;
use core::marker::PhantomData;
use core::ops::{BitAndAssign, BitOrAssign};
use core::sync::atomic::AtomicUsize;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{Channel, DynamicReceiver, DynamicSender};
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use foa::esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, TxParameters, WiFiRate};
use foa::esp32_wifi_hal_rs::RxFilterBank::{ReceiverAddress, BSSID};
use foa::esp32_wifi_hal_rs::RxFilterInterface::Zero;
use foa::interface;
use foa::interface::{Interface, InterfaceInput, InterfaceRunner};
use foa::lmac::{LMacInterfaceControl, LMacTransmitEndpoint};
use hex_literal::hex;
use ieee80211::common::{AssociationID, CapabilitiesInformation, DataFrameSubtype, FCFFlags, FrameType, ManagementFrameSubtype, SequenceControl};
use ieee80211::{element_chain, match_frames, supported_rates, GenericFrame};
use ieee80211::data_frame::{DataFrame, DataFrameReadPayload};
use ieee80211::data_frame::header::DataFrameHeader;
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{BeaconFrame, ManagementFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::BeaconBody;
use ieee80211::scroll::ctx::{TryFromCtx, TryIntoCtx};
use ieee80211::scroll::Pwrite;
use log::{error, info, warn};
use crate::packets::{ClientToHostDataFrame};
use crate::runner::DsWiFiRunner;

pub struct DsWiFiInterface;

const MAX_CLIENTS: usize = 15;

pub struct DsWiFiClientManager {
    clients: [Option<DsWiFiClient>; MAX_CLIENTS],
    all_clients_mask: DsWifiClientMask,

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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



pub struct DsWiFiSharedResources<'res> {
    client_manager: Mutex<NoopRawMutex, DsWiFiClientManager>,

    interface_control: Option<LMacInterfaceControl<'res>>,

    bg_rx_queue: Channel<NoopRawMutex, BorrowedBuffer<'res, 'res>, 4>,
    ack_rx_queue: Channel<NoopRawMutex, (MACAddress, Instant), 4>,
}

impl Default for DsWiFiSharedResources<'_> {
    fn default() -> Self {
        Self {
            client_manager: Mutex::from(DsWiFiClientManager {
                clients: [None; MAX_CLIENTS],
                all_clients_mask: 0x0000,
            }),
            bg_rx_queue: Channel::new(),
            ack_rx_queue: Channel::new(),
            interface_control: None,
        }
    }
}

pub struct DsWiFiControl<> {}

pub struct DsWiFiInput<'res> {
    transmit_endpoint: LMacTransmitEndpoint<'res>,
    mac_address: [u8; 6],
    bg_rx_queue: DynamicSender<'res, BorrowedBuffer<'res, 'res>>,
    ack_rx_queue: DynamicSender<'res, (MACAddress, Instant)>,
}

impl DsWiFiInput<'_> {
    async fn send_ack(&mut self) {
        let ack = hex!("82000000");
        let frame = DataFrame {
            header: DataFrameHeader {
                subtype: DataFrameSubtype::DataCFAck,
                fcf_flags: FCFFlags::new().with_from_ds(true),
                duration: 0,
                address_1: MACAddress::from([0x03,0x09,0xbf,0x00,0x00,0x03]),
                address_2: MACAddress::from(self.mac_address),
                address_3: MACAddress::from(self.mac_address),
                sequence_control: Default::default(),
                address_4: None,
                qos: None,
                ht_control: None,
            },
            payload: Some(ack.as_slice()),
            _phantom: Default::default(),
        };
        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let written = buffer.pwrite_with(frame, 0, false).unwrap();

        let _ = self.transmit_endpoint.transmit(
            &mut buffer[..written],
            &TxParameters {
                rate: WiFiRate::PhyRate2MS,
                wait_for_ack: false,
                duration: 0,
                tx_error_behaviour: TxErrorBehaviour::Drop,
                interface_one: false,
                interface_zero: false,
                override_seq_num: true
            },
        ).await;
    }
}
impl<'res> InterfaceInput<'res> for DsWiFiInput<'res, > {
    async fn interface_input(&mut self, borrowed_buffer: BorrowedBuffer<'res, 'res>) {
        //info!("InterfaceInput: {:X?}", borrowed_buffer.mpdu_buffer());
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
                                self.send_ack().await;
                                info!("got c2h data frame: pay load size {}",c2h_frame.payload_size);
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
                        self.send_ack().await;
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

pub struct DsWiFiInitInfo {}

impl Default for DsWiFiInitInfo {
    fn default() -> Self {
        Self {}
    }
}

impl Interface for DsWiFiInterface {
    const NAME: &str = "DS-WiFi";
    type SharedResourcesType<'res> = DsWiFiSharedResources<'res>;
    type ControlType<'res> = DsWiFiControl<>;
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
        interface_control.set_and_lock_channel(7).await.expect("TODO: panic message");

        interface_control.set_filter_parameters(BSSID,mac_address,None);
        interface_control.set_filter_parameters(ReceiverAddress,mac_address,Some([0x00;6]));

        interface_control.set_filter_status(BSSID,true);
        interface_control.set_filter_status(ReceiverAddress,true);

        shared_resources.bg_rx_queue = Channel::new();
        shared_resources.interface_control = Some(interface_control);
        let interface_control = shared_resources.interface_control.as_ref().unwrap();


        (
            DsWiFiControl {

            },
            DsWiFiRunner {
                transmit_endpoint,
                interface_control,
                client_manager: &shared_resources.client_manager,
                mac_address,
                bg_rx_queue: shared_resources.bg_rx_queue.dyn_receiver(),
                start_time: Instant::now(),
                ack_rx_queue: shared_resources.ack_rx_queue.dyn_receiver(),
            },
            DsWiFiInput {
                transmit_endpoint,
                mac_address,
                bg_rx_queue: shared_resources.bg_rx_queue.dyn_sender(),
                ack_rx_queue: shared_resources.ack_rx_queue.dyn_sender(),
            }
        )
    }
}

