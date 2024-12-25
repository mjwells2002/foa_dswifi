use alloc::vec;
use core::cmp::PartialEq;
use core::future::Future;
use core::intrinsics::black_box;
use core::marker::PhantomData;
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::DynamicReceiver;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, TxParameters, WiFiRate};
use esp32_wifi_hal_rs::TxErrorBehaviour::Drop;
use foa::interface::InterfaceRunner;
use foa::lmac::{LMacInterfaceControl, LMacTransmitEndpoint, OffChannelRequest};
use hex_literal::hex;
use ieee80211::common::{AssociationID, CapabilitiesInformation, DataFrameCF, DataFrameSubtype, FCFFlags, IEEE80211AuthenticationAlgorithmNumber, IEEE80211StatusCode, SequenceControl};
use ieee80211::{element_chain, match_frames, supported_rates};
use ieee80211::data_frame::builder::DataFrameBuilder;
use ieee80211::data_frame::DataFrame;
use ieee80211::data_frame::header::DataFrameHeader;
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::elements::rates::SupportedRatesElement;
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{AssociationRequestFrame, AssociationResponseFrame, AuthenticationFrame, BeaconFrame, DeauthenticationFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::{AssociationResponseBody, AuthenticationBody, BeaconBody};
use ieee80211::scroll::Pwrite;
use log::{info, trace, warn};
use crate::{DsWiFiClient, DsWiFiClientManager, DsWiFiClientState, DsWiFiSharedResources, DsWifiAidClientMaskBits, DsWifiClientMaskMath, MAX_CLIENTS};
use crate::packets::{BeaconType, DSWiFiBeaconTag};

pub struct DsWiFiRunner<'res> {
    pub(crate) transmit_endpoint: LMacTransmitEndpoint<'res>,
    pub(crate) interface_control: &'res LMacInterfaceControl<'res>,
    pub(crate) mac_address: [u8; 6],
    pub(crate) bg_rx_queue: DynamicReceiver<'res, BorrowedBuffer<'res, 'res>>,
    pub(crate) client_manager: &'res Mutex<NoopRawMutex, DsWiFiClientManager>,
    pub(crate) ack_rx_queue: DynamicReceiver<'res, (MACAddress, Instant)>,
    pub(crate) start_time: Instant,
}

impl DsWiFiRunner<'_> {
    async fn handle_auth_frame(&mut self, auth: AuthenticationFrame<'_>) {
        info!("Got Auth Frame");
        if auth.body.authentication_algorithm_number != IEEE80211AuthenticationAlgorithmNumber::OpenSystem {
            info!("Got Auth Frame but it was not OpenSystem");
            return;
        }


        let mut client_manager = self.client_manager.lock().await;

        let client_already_exists = client_manager.has_client(auth.header.transmitter_address);
        if client_already_exists {
            todo!("client already exists, need to drop old clients");
        }

        let next_aid = client_manager.get_next_client_aid();

        if next_aid.is_none() {
            panic!("All client slots filled, can't associate new client");
        }

        client_manager.add_client(DsWiFiClient {
            state: DsWiFiClientState::Associating,
            associated_mac_address: *auth.header.transmitter_address,
            association_id: next_aid.unwrap(),
            last_heard_from: Instant::now(),
        });


        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let frame = AuthenticationFrame {
            header: ManagementFrameHeader {
                receiver_address: auth.header.transmitter_address,
                transmitter_address: MACAddress::from(self.mac_address),
                bssid: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new(),
                ..Default::default()
            },
            body: AuthenticationBody {
                authentication_algorithm_number: IEEE80211AuthenticationAlgorithmNumber::OpenSystem,
                authentication_transaction_sequence_number: 2,
                status_code: IEEE80211StatusCode::Success,
                elements: element_chain!(),
                _phantom: Default::default()
            },
        };

        let written = buffer.pwrite_with(frame, 0, false).unwrap();

        let _ = self.transmit_endpoint.transmit(
            &mut buffer[..written],
            &TxParameters {
                rate: WiFiRate::PhyRate2MS,
                wait_for_ack: true,
                duration: 248,
                tx_error_behaviour: TxErrorBehaviour::RetryUntil(4),
                interface_one: false,
                interface_zero: false,
                override_seq_num: true
            },
        ).await;


    }

    async fn handle_assoc_req_frame(&mut self, assoc: AssociationRequestFrame<'_>) {
        info!("assoc request");
        let mut client_manager = self.client_manager.lock().await;

        let client = client_manager.get_client(assoc.header.transmitter_address).unwrap();

        let mut caps = CapabilitiesInformation::new();
        caps.set_is_ess(true);
        //
        caps.set_is_short_preamble_allowed(true);

        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let frame = AssociationResponseFrame {
            header: ManagementFrameHeader {
                fcf_flags: Default::default(),
                receiver_address: assoc.header.transmitter_address,
                transmitter_address: MACAddress::from(self.mac_address),
                bssid: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new(),
                duration: 162,
                ht_control: None,
            },
            body: AssociationResponseBody {
                capabilities_info: caps,
                status_code: IEEE80211StatusCode::Success,
                association_id: client.association_id,
                elements: element_chain! {
                        supported_rates![
                            1 B,
                            2 B
                        ]
                    },
                _phantom: Default::default(),
            }
        };

        let written = buffer.pwrite_with(frame, 0, false).unwrap();

        info!("assoc response header: {:X?}",&buffer[0..4]);

        let _ = self.transmit_endpoint.transmit(
            &mut buffer[..written],
            &TxParameters {
                rate: WiFiRate::PhyRate2MS,
                wait_for_ack: true,
                duration: 248,
                tx_error_behaviour: TxErrorBehaviour::RetryUntil(4),
                interface_one: false,
                interface_zero: false,
                override_seq_num: true
            },
        ).await;


        client_manager.update_client_state(assoc.header.transmitter_address,DsWiFiClientState::Connected);

        Timer::after_micros(500).await;
    }

    async fn handle_deauth(&mut self, deauth: DeauthenticationFrame<'_>) {
        info!("Got deauth frame");

        let mut client_manager = self.client_manager.lock().await;

        let aid = client_manager.get_client(deauth.header.transmitter_address).unwrap().association_id;
        info!("deauthing client with aid {}", aid.aid());

        client_manager.remove_client(aid);
    }
    async fn update_client_rx_time(&self, mac: MACAddress) {
        let mut client_manager = self.client_manager.lock().await;
        if let Some(client) = client_manager.get_client_mut(mac) {
            client.last_heard_from = Instant::now();
        };
    }
    async fn handle_bg_rx(
        &mut self,
        buffer: BorrowedBuffer<'_, '_>,
    ) {
        let _ = match_frames! {
            buffer.mpdu_buffer(),
            deauth = DeauthenticationFrame => {
                self.handle_deauth(deauth).await;
            }
            auth = AuthenticationFrame => {
                self.handle_auth_frame(auth).await;
            }
            assoc = AssociationRequestFrame => {
                self.handle_assoc_req_frame(assoc).await;
            }
        };
    }

    async fn send_beacon(&self,ticker: &mut Ticker) {
        ticker.next().await;
        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        //todo: move all this stuff to api
        let mut beacon = DSWiFiBeaconTag {
            game_id: [0x00, 0x00, 0x00, 0x00],
            beacon_type: BeaconType::MULTICART,
            cmd_data_size: 0x00c0,
            reply_data_size: 0x00c0,
            stream_code: 0x0f0f, //todo: increment this like a real ds
            ..Default::default()
        };
        let beacon_payload = Some(vec![0x48, 0x23, 0x11, 0x0a, 0x01, 0x01, 0x04, 0x00]);
        let beacon_vec = beacon.to_bytes(beacon_payload);

        let frame = BeaconFrame {
            header: ManagementFrameHeader {
                receiver_address: BROADCAST,
                transmitter_address: MACAddress::from(self.mac_address),
                bssid: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new(),
                ..Default::default()
            },
            body: BeaconBody {
                beacon_interval: 100,
                timestamp: self.start_time.elapsed().as_micros(),
                capabilities_info: CapabilitiesInformation::new().with_is_ess(true),
                elements: element_chain! {
                    supported_rates![
                            1 B,
                            2 B
                        ],
                    DSSSParameterSetElement {
                        current_channel: 7
                    },
                    RawIEEE80211Element {
                        tlv_type: 5,
                        slice: &[00u8,02,00,00],
                        _phantom: Default::default(),
                    },
                    VendorSpecificElement::new_prefixed(&[0x00u8,0x09,0xbf],beacon_vec.as_slice())
                },
                _phantom: PhantomData
            },
        };

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

    async fn send_deauth(&self, target: &[u8; 6]) {
        //todo: this
    }
    async fn handle_timeouts(&self, ticker: &mut Ticker) {
        ticker.next().await;
        self.handle_timeouts_inner().await;
    }

    async fn handle_timeouts_inner(&self) {
        let mut timed_clients: [Option<AssociationID>; MAX_CLIENTS] = [None; MAX_CLIENTS];
        let mut i = 0;
        let mut client_manager = self.client_manager.lock().await;
        for client in &client_manager.clients {
            if let Some(client) = client {
                if client.last_heard_from.elapsed() > Duration::from_secs(1) {
                    info!("client {:X?} timed out", client.associated_mac_address);
                    self.send_deauth(&client.associated_mac_address).await;
                    timed_clients[i] = Some(client.association_id);
                    i+=1;
                }
            }
        };
        for client in timed_clients {
            if let Some(client) = client {
                client_manager.remove_client(client);
            }
        }
    }

    async fn send_data_tick(&self, ticker: &mut Ticker) {
        ticker.next().await;

        let mut mask = {
            let client_manager = self.client_manager.lock().await;

            if client_manager.all_clients_mask.is_empty() {
                return;
            }

             client_manager.all_clients_mask
        };

        let mask_le_bytes = mask.to_le_bytes();
        let mut idle = hex!("E6 03 02 00 34 1C 05 00 68 00 75 85 DA 87 38 90 5A 0C FC 79 1E 00 DC A9 24 52 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 04 00 02 00");
        //let payload = [0xe6,0x03,mask[0],mask[1],0x00,0x00u8];
        idle[2] = mask_le_bytes[0];
        idle[3] = mask_le_bytes[1];

        let ack = hex!("a9000000");

        let frame = DataFrame {
            header: DataFrameHeader {
                subtype: DataFrameSubtype::CFPoll,
                fcf_flags: FCFFlags::new().with_from_ds(true),
                duration: 240,
                address_1: MACAddress::from([0x03,0x09,0xbf,0x00,0x00,0x00]),
                address_2: MACAddress::from(self.mac_address),
                address_3: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new(),
                address_4: None,
                qos: None,
                ht_control: None,
            },
            payload: Some(idle.as_slice()),
            _phantom: Default::default(),

        };

        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let written = buffer.pwrite_with(frame, 0, false).unwrap();

        while self.ack_rx_queue.try_receive().is_ok() {
            warn!("ack received after timeout");
        }

        let res = self.transmit_endpoint.transmit(
            &mut buffer[..written],
            &TxParameters {
                rate: WiFiRate::PhyRate2MS,
                wait_for_ack: false,
                duration: 780,
                tx_error_behaviour: TxErrorBehaviour::RetryUntil(4),
                interface_one: false,
                interface_zero: false,
                override_seq_num: true
            },
        ).await;
        let tx = Instant::now();
        if res.is_err() {
            warn!("tx failed");
            return;
        }

        //TODO: this probably isnt correct, correct value would be the determined using the beacon params for max frame tx time
        let mut timeout = Timer::after_micros(1200 * (mask.num_clients() as u64));

        while !mask.is_empty() {
            match select(&mut timeout,self.ack_rx_queue.receive()).await {
                Either::First(_) => { warn!("ack timeout"); break; }
                Either::Second((ack_from,ack_enqueue_time)) => {
                    let mut client_manager = self.client_manager.lock().await;
                    if let Some(client) = client_manager.get_client_mut(ack_from) {
                        let ack = Instant::now();
                        info!("ack latency: {} / {}", (ack - tx).as_micros(), (ack - ack_enqueue_time).as_micros());
                        client.last_heard_from = Instant::now();
                        mask.mask_subtract(client.association_id.get_mask_bits());
                    }
                }
            }
        }
    }
    async fn tick(&self, beacon_ticker: &mut Ticker, data_rate_limit: &mut Ticker, timeout_check_rate: &mut Ticker) {
        let _ = select3(
            self.send_data_tick(data_rate_limit),
            self.send_beacon(beacon_ticker),
            self.handle_timeouts(timeout_check_rate)
        ).await;
    }
    async fn debug_log(&self, rate_ticker: &mut Ticker) {
        rate_ticker.next().await;
        let client_manager = self.client_manager.lock().await;

        info!("=====================================");
        info!("Clients Connected: {}", client_manager.all_clients_mask.num_clients());
        info!("All Client Mask: {}", client_manager.all_clients_mask);
        info!("Client List:");
        for i in 0..MAX_CLIENTS {
            let client_opt = &client_manager.clients[i];
            if let Some(client) = client_opt {
                info!("Client: aid: {}, mac: {:X?}, state {:?}, index: {}",client.association_id.aid(),client.associated_mac_address, client.state, i);
            }
        }
        return;
    }


}
impl InterfaceRunner for DsWiFiRunner<'_> {


    async fn run(&mut self) -> ! {
        info!("Runner Says Hi");

        let mut beacon_ticker =  Ticker::every(Duration::from_millis(100));
        let mut data_rate_limit = Ticker::every(Duration::from_millis(33)); //very slow rate limit for now
        let mut timeout_check_rate = Ticker::every(Duration::from_secs(2));
        let mut log_ticker = Ticker::every(Duration::from_secs(1));

        loop {
            match select4(
                self.interface_control.wait_for_off_channel_request(),
                self.bg_rx_queue.receive(),
                self.debug_log(&mut log_ticker),
                self.tick(&mut beacon_ticker,
                          &mut data_rate_limit,
                          &mut timeout_check_rate),
                ).await {
                Either4::First(off_channel_request) => {
                    off_channel_request.reject();
                },
                Either4::Second(buffer) => {self.handle_bg_rx(buffer).await;},
                _ => {}
            }


        }
    }
}