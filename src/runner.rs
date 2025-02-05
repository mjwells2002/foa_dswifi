use core::cmp::PartialEq;
use core::future::Future;
use core::intrinsics::{black_box, unreachable};
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, Ordering};
use defmt::{debug, info, warn};
use embassy_futures::join::join;
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{DynamicReceiver, DynamicSender};
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};
use foa::esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, TxParameters, WiFiRate};
use foa::interface::InterfaceRunner;
use foa::lmac::{LMacError, LMacInterfaceControl, LMacTransmitEndpoint, OffChannelRequest};
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
use crate::{DsWiFiClient, DsWiFiClientEvent, DsWiFiClientManager, DsWiFiClientState, DsWiFiControlEvent, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWiFiSharedResources, DsWifiAidClientMaskBits, DsWifiClientMaskMath, Responder, MAX_CLIENTS};
use crate::DsWiFiControlEvent::FrameRequired;
use crate::DsWiFiInterfaceControlEventResponse::{Failed, Success};
use crate::packets::{BeaconType, DSWiFiBeaconTag, HostToClientDataFrame, HostToClientFlags, HostToClientFooter};
use crate::pictochat_packets::{PictochatBeacon, PictochatChatroom};

pub struct PendingDataFrame {
    pub data: [u8; 300],
    pub size: u16,
    pub flags: HostToClientFlags,
}
pub struct DsWiFiRunner<'res> {
    pub(crate) transmit_endpoint: LMacTransmitEndpoint<'res>,
    pub(crate) interface_control: &'res LMacInterfaceControl<'res>,
    pub(crate) mac_address: [u8; 6],
    pub(crate) bg_rx_queue: DynamicReceiver<'res, BorrowedBuffer<'res, 'res>>,
    pub(crate) client_manager: &'res Mutex<NoopRawMutex, DsWiFiClientManager>,
    pub(crate) ack_rx_queue: DynamicReceiver<'res, (MACAddress, Instant)>,
    pub(crate) start_time: Instant,
    pub(crate) data_tx_mutex: &'res Mutex<NoopRawMutex, PendingDataFrame>,
    pub(crate) data_tx_signal: &'res Signal<NoopRawMutex, DsWiFiControlEvent>,
    pub(crate) data_tx_signal_2: &'res Signal<NoopRawMutex, DsWiFiControlEvent>,
    pub(crate) control_responder: Responder<'res, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse>,
    pub(crate) beacons_enabled: Mutex<NoopRawMutex, bool>,
    pub(crate) event_tx: DynamicSender<'res,DsWiFiClientEvent>,
    pub(crate) data_seq: AtomicU16
}

/* ChatGPT wrote these 2 functions, it may be wrong */
/// Calculates the air duration (in microseconds) of a Wi-Fi frame.
/// `frame_size` is in bytes (total payload size, including MAC overhead).
pub fn calculate_air_duration(rate: WiFiRate, frame_size: usize) -> u16 {
    match rate {
        WiFiRate::PhyRate1ML => {
            // 1 Mbps long preamble timings:
            // Long preamble = 192 microseconds, Header = 48 bits (48μs at 1 Mbps)
            let preamble = 192; // 192 μs
            let header = 48; // 48 μs
            let payload_time = calculate_payload_time(1, frame_size); // rate = 1 Mbps
            preamble + header + payload_time
        }
        WiFiRate::PhyRate2ML => {
            // 2 Mbps long preamble timings:
            // Long preamble = 192 microseconds, Header = 24 bits (12μs at 2 Mbps)
            let preamble = 192; // 192 μs
            let header = 24; // 24 μs
            let payload_time = calculate_payload_time(2, frame_size); // rate = 2 Mbps
            preamble + header + payload_time
        }
        WiFiRate::PhyRate2MS => {
            // 2 Mbps short preamble timings:
            // Short preamble = 96 microseconds, Header = 24 bits (12μs at 2 Mbps)
            let preamble = 96; // 96 μs
            let header = 24; // 24 μs
            let payload_time = calculate_payload_time(2, frame_size); // rate = 2 Mbps
            preamble + header + payload_time
        }
        _ => {
            todo!("unsupported rate")
        }
    }
}

fn calculate_payload_time(rate_mbps: u16, frame_size: usize) -> u16 {
    // Convert frame size from bytes to bits (1 byte = 8 bits)
    let frame_bits = frame_size as u16 * 8;
    // Divide frame bits by rate (Mbps) to get time in microseconds
    (frame_bits + (rate_mbps - 1)) / rate_mbps // Ceiling division
}
/* I hope it's not though */

fn ds_tx_params_for_dataframe(rate: WiFiRate, frame_size: usize, tx_error_behaviour: TxErrorBehaviour) -> TxParameters {
    let air_duration = calculate_air_duration(rate, frame_size);
    TxParameters {
        rate,
        interface_zero: false,
        interface_one: false,
        wait_for_ack: false,
        duration: air_duration,
        override_seq_num: true,
        tx_error_behaviour,
    }
}
impl DsWiFiRunner<'_> {
    async fn handle_auth_frame(&self, auth: AuthenticationFrame<'_>) {
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

    async fn handle_assoc_req_frame(&self, assoc: AssociationRequestFrame<'_>) {
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
        self.event_tx.send(DsWiFiClientEvent::Connected(*assoc.header.transmitter_address)).await;

        Timer::after_micros(500).await;
    }

    async fn handle_deauth(&self, deauth: DeauthenticationFrame<'_>) {
        let mut client_manager = self.client_manager.lock().await;

        let aid = client_manager.get_client(deauth.header.transmitter_address).unwrap().association_id;
        self.event_tx.send(DsWiFiClientEvent::Disconnected(*deauth.header.transmitter_address)).await;

        info!("disconnecting client with aid {} due to deauth frame", aid.aid());

        client_manager.remove_client(aid);
    }
    async fn update_client_rx_time(&self, mac: MACAddress) {
        let mut client_manager = self.client_manager.lock().await;
        if let Some(client) = client_manager.get_client_mut(mac) {
            client.last_heard_from = Instant::now();
        };
    }
    async fn handle_bg_rx(
        &self,
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
        {
            let enabled = self.beacons_enabled.lock().await;
            if !*enabled {
                return;
            }
        }
        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        //todo: move all this stuff to api
        let beacon = {
            let client_mananger = self.client_manager.lock().await;

            DSWiFiBeaconTag {
                oui_type: 0,
                stepping_offset: [0x0a, 0x00],
                lcd_video_sync: [0x00, 0x00],
                fixed_id: [0x00, 0x00, 0x00, 0x0a],
                game_id: [0x00, 0x00, 0x00, 0x00],
                beacon_type: BeaconType::MULTICART,
                cmd_data_size: 0x00c0,
                reply_data_size: 0x00c0,
                stream_code: 0x0f0f, //todo: increment this like a real ds
                payload: Some(PictochatBeacon {
                    chatroom: PictochatChatroom::B,
                    client_count: client_mananger.all_clients_mask.num_clients() + 1,
                    ..Default::default()
                }),
            }
        };

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
                    VendorSpecificElement::new_prefixed(&[0x00u8,0x09,0xbf],beacon)
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
    async fn send_ack(&self) {
        let tx = Instant::now();

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

        //info!("ack2 delay: {}", (Instant::now() - tx).as_micros());
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
    async fn handle_timeouts(&self, ticker: &mut Ticker) {
        ticker.next().await;
        let mut timed_clients: [Option<AssociationID>; MAX_CLIENTS] = [None; MAX_CLIENTS];
        let mut i = 0;
        let mut client_manager = self.client_manager.lock().await;
        for client in &client_manager.clients {
            if let Some(client) = client {
                if client.last_heard_from.elapsed() > Duration::from_secs(1) {
                    info!("client {:?} timed out", client.associated_mac_address);
                    self.send_deauth(&client.associated_mac_address).await;
                    self.event_tx.send(DsWiFiClientEvent::Disconnected(client.associated_mac_address)).await;

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

            if !client_manager.last_mask.is_empty() {
                client_manager.last_mask
            } else {
                self.data_tx_signal.signal(FrameRequired);

                self.data_tx_signal_2.wait().await;
                self.data_tx_signal_2.reset();
                client_manager.all_clients_mask
            }
        };


        let payload = self.data_tx_mutex.lock().await;

        //info!("sending data frame with payload size {}", payload.size);

        let max_client_ack_wait_micros = 998;

        let frame = DataFrame {
            header: DataFrameHeader {
                subtype: DataFrameSubtype::DataCFPoll,
                fcf_flags: FCFFlags::new().with_from_ds(true),
                duration: 0,
                address_1: MACAddress::from([0x03,0x09,0xbf,0x00,0x00,0x00]),
                address_2: MACAddress::from(self.mac_address),
                address_3: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new(),
                address_4: None,
                qos: None,
                ht_control: None,
            },
            payload: Some(HostToClientDataFrame::<&[u8]> {
                us_per_client_reply: max_client_ack_wait_micros,
                client_target_mask: mask,
                flags: payload.flags,
                payload: if payload.size != 0 { Some(&payload.data[..payload.size as usize]) } else { None },
                footer: Some(HostToClientFooter {
                    data_seq: self.data_seq.fetch_add(1,Ordering::Relaxed),
                    client_target_mask: mask,
                }),
            }),
            _phantom: Default::default(),
        };

        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let written  = buffer.pwrite_with(frame, 0, false).unwrap();

        while self.ack_rx_queue.try_receive().is_ok() {
            warn!("ack received after timeout");
        }

        let tx_pre = Instant::now();
        let res = self.transmit_endpoint.transmit(
            &mut buffer[..written],
            &ds_tx_params_for_dataframe(WiFiRate::PhyRate2MS, written, TxErrorBehaviour::RetryUntil(4)),
        ).await;
        let tx = Instant::now();

        debug!("tx took {} micros", (tx - tx_pre).as_micros());

        if res.is_err() {
            warn!("tx failed");
            return;
        }

        //TODO: this still isnt right, but it works most of the time
        //180us is the average observed queue latency from rx to processing
        let mut timeout = Timer::after_micros(((max_client_ack_wait_micros * 5) * (mask.num_clients() as u16)) as u64);

        while !mask.is_empty() {
            match select(&mut timeout,self.ack_rx_queue.receive()).await {
                Either::First(_) => { warn!("ack timeout"); break; }
                Either::Second((ack_from,ack_enqueue_time)) => {
                    let mut client_manager = self.client_manager.lock().await;
                    if let Some(client) = client_manager.get_client_mut(ack_from) {
                        let ack = Instant::now();
                        debug!("ack latency: {} / {}", (ack - tx).as_micros(), (ack - ack_enqueue_time).as_micros());
                        let aapl = (ack - tx).as_micros() - (ack - ack_enqueue_time).as_micros() - 216;
                        debug!("adjusted ack processing latency {}",aapl);
                        Timer::after_micros(450).await;
                        self.send_ack().await;
                        client.last_heard_from = Instant::now();
                        mask.mask_subtract(client.association_id.get_mask_bits());
                    }
                }
            }
        }
    }
    async fn handle_control(&self) {
        let request = self.control_responder.wait_for_request().await;
        match request {
            DsWiFiInterfaceControlEvent::SetChannel(channel) => {
                if let Err(_) = self.interface_control.set_and_lock_channel(channel).await {
                    self.control_responder.send_response(Failed);
                } else {
                    self.control_responder.send_response(Success);
                }
            },
            DsWiFiInterfaceControlEvent::SetBeaconsEnabled(new_enabled) => {
                let mut enabled = self.beacons_enabled.lock().await;
                *enabled = new_enabled;
                self.control_responder.send_response(Success);
            }
        }
    }

    async fn tick(&self, beacon_ticker: &mut Ticker, data_rate_limit: &mut Ticker, timeout_check_rate: &mut Ticker) {
        let _ = select4(
            self.send_data_tick(data_rate_limit),
            self.send_beacon(beacon_ticker),
            self.handle_timeouts(timeout_check_rate),
            self.handle_control()
        ).await;
    }


}
impl InterfaceRunner for DsWiFiRunner<'_> {


    async fn run(&mut self) -> ! {
        info!("Runner Says Hi");

        let mut beacon_ticker =  Ticker::every(Duration::from_millis(100));
        let mut timeout_check_rate = Ticker::every(Duration::from_secs(2));
        let mut data_rate_limit = Ticker::every(Duration::from_millis(33)); //very slow rate limit for now

        loop {
            match select3(
                self.interface_control.wait_for_off_channel_request(),
                self.bg_rx_queue.receive(),
                self.tick(&mut beacon_ticker,
                          &mut data_rate_limit,
                          &mut timeout_check_rate),
            ).await {
                Either3::First(off_channel_request) => {
                    off_channel_request.reject();
                },
                Either3::Second(buffer) => {self.handle_bg_rx(buffer).await;},
                _ => {}
            }
        }
    }
}