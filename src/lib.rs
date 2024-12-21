#![no_std]

mod runner;

use core::future::Future;
use core::marker::PhantomData;
use core::sync::atomic::AtomicUsize;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp32_wifi_hal_rs::{BorrowedBuffer, TxErrorBehaviour, WiFiRate};
use esp32_wifi_hal_rs::RxFilterBank::{ReceiverAddress, BSSID};
use esp32_wifi_hal_rs::RxFilterInterface::Zero;
use foa::interface;
use foa::interface::{Interface, InterfaceInput, InterfaceRunner};
use foa::lmac::{LMacInterfaceControl, LMacTransmitEndpoint};
use ieee80211::common::{CapabilitiesInformation, FrameType, ManagementFrameSubtype, SequenceControl};
use ieee80211::{element_chain, supported_rates, GenericFrame};
use ieee80211::elements::{DSSSParameterSetElement, RawIEEE80211Element, VendorSpecificElement};
use ieee80211::mac_parser::{MACAddress, BROADCAST};
use ieee80211::mgmt_frame::{BeaconFrame, ManagementFrameHeader};
use ieee80211::mgmt_frame::body::BeaconBody;
use ieee80211::scroll::Pwrite;
use log::info;

use crate::runner::DsWiFiRunner;

pub struct DsWiFiInterface;

pub struct DsWiFiSharedResources<'res> {
    
    // Misc.
    interface_control: Option<LMacInterfaceControl<'res>>,


}

impl Default for DsWiFiSharedResources<'_> {
    fn default() -> Self {
        Self {
            interface_control: None,
        }
    }
}

pub struct DsWiFiControl<> {}

pub struct DsWiFiInput<> {}

impl<'res> InterfaceInput<'res> for DsWiFiInput<> {
    async fn interface_input(&mut self, borrowed_buffer: BorrowedBuffer<'res, 'res>) {
        info!("InterfaceInput: {:X?}", borrowed_buffer.mpdu_buffer());
        let Ok(generic_frame) = GenericFrame::new(borrowed_buffer.mpdu_buffer(), false) else {
            return;
        };
        match generic_frame.frame_control_field().frame_type() {
            FrameType::Management(mgmt_frame_type) => {
                info!("Management Frame");
                match (mgmt_frame_type) {
                    ManagementFrameSubtype::AssociationRequest => {
                        info!("AssociationRequest Frame");
                    }
                    ManagementFrameSubtype::Authentication => {
                        info!("Authentication Frame");
                    }
                    _ => {
                        info!("Unknown Management Frame");
                    }
                }
            }
            FrameType::Control(_) => {
                todo!()
            }
            FrameType::Data(_) => {
                todo!()
            }
            FrameType::Unknown(_) => {
                info!("Unknown Frame");
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
    type InputType<'res> = DsWiFiInput<>;
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
        interface_control.set_filter_parameters(ReceiverAddress,mac_address,None);

        interface_control.set_filter_status(BSSID,true);
        interface_control.set_filter_status(ReceiverAddress,true);

        shared_resources.interface_control = Some(interface_control);
        let interface_control = shared_resources.interface_control.as_ref().unwrap();

        (
            DsWiFiControl {

            },
            DsWiFiRunner {
                transmit_endpoint,
                interface_control,
                mac_address
            },
            DsWiFiInput {

            }
        )
    }
}

