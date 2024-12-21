use core::marker::PhantomData;
use embassy_futures::select::{select, select3, Either};
use embassy_time::{Duration, Instant, Ticker};
use esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, WiFiRate};
use foa::interface::InterfaceRunner;
use foa::lmac::{LMacInterfaceControl, LMacTransmitEndpoint, OffChannelRequest};
use ieee80211::common::{CapabilitiesInformation, SequenceControl};
use ieee80211::{element_chain, match_frames, supported_rates};
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{BeaconFrame, DeauthenticationFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::BeaconBody;
use ieee80211::scroll::Pwrite;
use log::info;
use crate::DsWiFiSharedResources;

pub struct DsWiFiRunner<'res> {
    pub(crate) transmit_endpoint: LMacTransmitEndpoint<'res>,
    pub(crate) interface_control: &'res LMacInterfaceControl<'res>,
    pub(crate) mac_address: [u8; 6],
}

impl DsWiFiRunner<'_> {
    async fn handle_bg_rx(
        &mut self,
        buffer: BorrowedBuffer<'_, '_>,
    ) {
        let _ = match_frames! {
            buffer.mpdu_buffer(),
            deauth = DeauthenticationFrame => {
                //self.handle_deauth(deauth, connection_state_subscriber).await;
            }
        };
    }

    async fn send_beacon(&self, start_time: Instant, lmac_interface_control: &LMacInterfaceControl<'_>, lmac_transmit_endpoint: &LMacTransmitEndpoint<'_>, mac_address: MACAddress, rate_ticker: &mut Ticker) {
        rate_ticker.next().await;

        let mut buffer = [0u8; 1500];
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
                    VendorSpecificElement::new_prefixed(&[0x00u8,0x09,0xbf],[0x00,0x0a,0x00,0x00,0x00,0x01,0x00,0x01,0x00,0x00,0x00,0x00,0x00,0x07,0x00,0x08,0x01,0xc0,0x00,0xc0,0x00,0x48,0x23,0x6d,0xa8,0x01,0x01,0x04,0x00].as_slice())
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

}
impl InterfaceRunner for DsWiFiRunner<'_> {


    async fn run(&mut self) -> ! {
        info!("Runner Says Hi");
        let mut beacon_ticker = Ticker::every(Duration::from_millis(100));
        let start_time = Instant::now();

        loop {

            match select(self.interface_control.wait_for_off_channel_request(),
                          self.send_beacon(start_time, self.interface_control, &self.transmit_endpoint, MACAddress::from(self.mac_address), &mut beacon_ticker)).await {
                Either::First(off_channel_request) => {
                    off_channel_request.reject();
                }
                Either::Second(_) => {

                }
            }


        }
    }
}