use embassy_futures::join::join3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Ticker};
use log::{error, info};
use crate::{DsWiFiClientEvent, DsWiFiControl, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWifiClientMaskMath};
use crate::DsWiFiControlEvent::FrameGenerated;

pub struct PictoChatUserStateManager {

}
pub struct PictoChatApplication<'res> {
    pub ds_wifi_control: DsWiFiControl<'res>,
    pub user_state_manager: Mutex<NoopRawMutex, PictoChatUserStateManager>,
}
impl<'res> PictoChatApplication<'res> {
    async fn generate_idle_frame(&self) {

    }
    async fn tx_wait_loop(&self) {
        loop {
            self.ds_wifi_control.data_tx_signal.wait().await;
            self.ds_wifi_control.data_tx_signal.reset();
            self.ds_wifi_control.data_tx_signal_2.signal(FrameGenerated);
        }
    }
    async fn rx_wait_loop(&self) {
        loop {
            let (aaa,bbb) = self.ds_wifi_control.data_rx.receive().await;
            info!("{:X?}", aaa);
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