use alloc::vec;
use core::cmp::PartialEq;
use core::marker::PhantomData;
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_sync::channel::DynamicReceiver;
use embassy_time::{Duration, Instant, Ticker};
use esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, WiFiRate};
use foa::interface::InterfaceRunner;
use foa::lmac::{LMacInterfaceControl, LMacTransmitEndpoint, OffChannelRequest};
use ieee80211::common::{AssociationID, CapabilitiesInformation, IEEE80211AuthenticationAlgorithmNumber, IEEE80211StatusCode, SequenceControl};
use ieee80211::{element_chain, match_frames, supported_rates};
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::elements::rates::SupportedRatesElement;
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{AssociationRequestFrame, AssociationResponseFrame, AuthenticationFrame, BeaconFrame, DeauthenticationFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::{AssociationResponseBody, AuthenticationBody, BeaconBody};
use ieee80211::scroll::Pwrite;
use log::info;
use crate::{DsWiFiClient, DsWiFiClientManager, DsWiFiClientState, DsWiFiSharedResources, DsWifiClientMaskMath};
use crate::packets::{BeaconType, DSWiFiBeaconTag};

pub struct DsWiFiRunner<'res> {
    pub(crate) transmit_endpoint: LMacTransmitEndpoint<'res>,
    pub(crate) interface_control: &'res LMacInterfaceControl<'res>,
    pub(crate) mac_address: [u8; 6],
    pub(crate) bg_rx_queue: DynamicReceiver<'res, BorrowedBuffer<'res, 'res>>,
    pub(crate) client_manager: &'res mut DsWiFiClientManager,
}


impl DsWiFiRunner<'_> {
    async fn handle_auth_frame(&mut self, auth: AuthenticationFrame<'_>) {
        info!("Got Auth Frame");
        if auth.body.authentication_algorithm_number != IEEE80211AuthenticationAlgorithmNumber::OpenSystem {
            info!("Got Auth Frame but it was not OpenSystem");
            return;
        }

        let client_already_exists = self.client_manager.has_client(auth.header.transmitter_address);
        if client_already_exists {
            todo!("client already exists, need to drop old clients");
        }

        self.client_manager.add_client(DsWiFiClient {
            state: DsWiFiClientState::Associating,
            associated_mac_address: *auth.header.transmitter_address,
            association_id: Default::default(),
            last_heard_from: Instant::now(),
        });

        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let frame = AuthenticationFrame {
            header: ManagementFrameHeader {
                receiver_address: auth.header.transmitter_address,
                transmitter_address: MACAddress::from(self.mac_address),
                bssid: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new().with_sequence_number(self.interface_control.get_and_increase_sequence_number()),
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

        let written = buffer.pwrite_with(frame, 0, true).unwrap();

        let _ = self.transmit_endpoint.transmit(
            &buffer[..written],
            WiFiRate::PhyRate2MS,
            TxErrorBehaviour::Drop,
        ).await;
    }

    async fn handle_assoc_req_frame(&mut self, assoc: AssociationRequestFrame<'_>) {
        info!("assoc request");

        let next_aid = self.client_manager.get_next_client_aid();

        let client = self.client_manager.get_client_mut(assoc.header.transmitter_address).unwrap();

        if client.state != DsWiFiClientState::Associating {
            panic!("Client is not authenticating, but got assoc request");
        }

        if let Some(aid) = next_aid {
            client.association_id = aid;
        } else {
            panic!("All client slots filled, can't associate new client");
        }

        client.last_heard_from = Instant::now();
        client.state = DsWiFiClientState::Connected;

        let mut caps = CapabilitiesInformation::new();
        caps.set_is_ess(true);
        caps.set_is_short_preamble_allowed(true);

        let mut buffer = self.transmit_endpoint.alloc_tx_buf().await;

        let frame = AssociationResponseFrame {
            header: ManagementFrameHeader {
                receiver_address: assoc.header.transmitter_address,
                transmitter_address: MACAddress::from(self.mac_address),
                bssid: MACAddress::from(self.mac_address),
                sequence_control: SequenceControl::new().with_sequence_number(self.interface_control.get_and_increase_sequence_number()),
                ..Default::default()
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

        let written = buffer.pwrite_with(frame, 0, true).unwrap();

        let _ = self.transmit_endpoint.transmit(
            &buffer[..written],
            WiFiRate::PhyRate2MS,
            TxErrorBehaviour::Drop,
        ).await;
    }

    async fn handle_deauth(&mut self, deauth: DeauthenticationFrame<'_>) {
        info!("Got deauth frame");

        let aid = self.client_manager.get_client(deauth.header.transmitter_address).unwrap().association_id;

        self.client_manager.remove_client(aid);
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

    async fn send_beacon(&self, start_time: Instant, lmac_interface_control: &LMacInterfaceControl<'_>, lmac_transmit_endpoint: &LMacTransmitEndpoint<'_>, mac_address: MACAddress, rate_ticker: &mut Ticker) {
        rate_ticker.next().await;
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
                transmitter_address: mac_address,
                bssid: mac_address,
                sequence_control: SequenceControl::new().with_sequence_number(lmac_interface_control.get_and_increase_sequence_number()),
                ..Default::default()
            },
            body: BeaconBody {
                beacon_interval: 100,
                timestamp: start_time.elapsed().as_micros(),
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

        let written = buffer.pwrite_with(frame, 0, true).unwrap();

        let _ = lmac_transmit_endpoint.transmit(
            &buffer[..written],
            WiFiRate::PhyRate2MS,
            TxErrorBehaviour::Drop,
        ).await;
    }

    async fn debug_log(&self, rate_ticker: &mut Ticker) {
        rate_ticker.next().await;

        info!("=====================================");
        info!("Clients Connected: {}", self.client_manager.all_clients_mask.num_clients());
        info!("All Client Mask: {}", self.client_manager.all_clients_mask);
        info!("Client List:");
        for client_opt in &self.client_manager.clients {
            if let Some(client) = client_opt {
                info!("Client: aid: {}, mac: {:X?}, state {:?}",client.association_id.aid(),client.associated_mac_address, client.state);
            }
        }
    }

}
impl InterfaceRunner for DsWiFiRunner<'_> {


    async fn run(&mut self) -> ! {
        info!("Runner Says Hi");
        let mut beacon_ticker = Ticker::every(Duration::from_millis(100));
        let start_time = Instant::now();
        let mut log_ticker = Ticker::every(Duration::from_millis(1000));

        loop {

            match select4(self.interface_control.wait_for_off_channel_request(),
                          self.send_beacon(start_time, self.interface_control, &self.transmit_endpoint, MACAddress::from(self.mac_address), &mut beacon_ticker),
                          self.bg_rx_queue.receive(),
                          self.debug_log(&mut log_ticker)
            ).await {
                Either4::First(off_channel_request) => {
                    off_channel_request.reject();
                }
                Either4::Second(_) => {}
                Either4::Third(data) => {
                    self.handle_bg_rx(data).await;
                }
                Either4::Fourth(_) => {}
            }


        }
    }
}