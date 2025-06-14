use alloc::format;
use ssd1306::rotation::DisplayRotation;
use ssd1306::size::DisplaySize128x64;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Display;
use embassy_executor::{SpawnToken, Spawner};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel;
use embassy_sync::channel::{Channel, DynamicReceiver};
use embassy_sync::mutex::{Mutex, MutexGuard};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Text},
    Drawable,
};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Point, Size};
use embedded_graphics::image::{Image, ImageRaw};
use embedded_graphics::mono_font::ascii::{FONT_5X7, FONT_6X13};
use embedded_graphics::primitives::Primitive;
use esp_hal::Async;
use ssd1306::{mode::DisplayConfig, Ssd1306, I2CDisplayInterface};
use ssd1306::mode::BufferedGraphicsMode;
use esp_hal::i2c::master::I2c;
use ssd1306::prelude::Brightness;
use crate::mk_static_dram2;
use crate::MaybeUninit;

const STATUS_HEIGHT: i32 = 16;
const MAX_LOG_LINES: usize = 5;
const WIFI_CONNECTED_ICON: ImageRaw<BinaryColor> = ImageRaw::new(include_bytes!("icon/wifi_connected.bin"),16);
const WIFI_DISCONNECTED_ICON: ImageRaw<BinaryColor> = ImageRaw::new(include_bytes!("icon/wifi_disconnected.bin"),16);
const CLOUD_CONNECTED_ICON: ImageRaw<BinaryColor> = ImageRaw::new(include_bytes!("icon/cloud_connected.bin"),16);
const CLOUD_DISCONNECTED_ICON: ImageRaw<BinaryColor> = ImageRaw::new(include_bytes!("icon/cloud_disconnected.bin"),16);


pub struct DisplayManagerInnerState {
    display: Ssd1306<ssd1306::prelude::I2CInterface<I2c<'static, Async>>, DisplaySize128x64, BufferedGraphicsMode<DisplaySize128x64>>,
    log_lines: Vec<String>,
    cloud_connected: bool,
    wifi_connected: bool,
    clients_connected: u8,
}
pub struct DisplayManager {
    inner: Mutex<NoopRawMutex, DisplayManagerInnerState>,
    text_style: MonoTextStyle<'static, BinaryColor>,
    large_text_style: MonoTextStyle<'static, BinaryColor>,
    event_chan: DynamicReceiver<'static, DisplayUpdate>
}

impl DisplayManager {
    pub fn new(i2c: I2c<'static, Async>, spawner: Spawner) -> (embassy_sync::channel::DynamicSender<'static, DisplayUpdate>) {
        let interface = I2CDisplayInterface::new(i2c);
        let mut display = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
            .into_buffered_graphics_mode();
        display.init().unwrap();
        display.set_brightness(Brightness::BRIGHTEST).unwrap();
        display.clear(BinaryColor::Off).unwrap();
        display.flush().unwrap();

        let channel = mk_static_dram2!(Channel<NoopRawMutex, DisplayUpdate, 5>,embassy_sync::channel::Channel::new());

        let r = channel.dyn_sender().clone();
        let t = channel.dyn_receiver();

        let display_manager = Self {
            inner: Mutex::from(DisplayManagerInnerState {
                display,
                log_lines: Vec::new(),
                cloud_connected: false,
                wifi_connected: false,
                clients_connected: 0,
            }),
            text_style: MonoTextStyle::new(&FONT_5X7, BinaryColor::On),
            large_text_style: MonoTextStyle::new(&FONT_6X13, BinaryColor::On),
            event_chan: t,
        };

        spawner.spawn(handle_updates(display_manager)).unwrap();

        r
    }

    pub async fn refresh(&self) {
        let mut inner = self.inner.lock().await;
        let _ = inner.display.clear(BinaryColor::Off);
        self.redraw_status(&mut inner);
        // Draw status bar separator
        let _ = Line::new(
            Point::new(0, STATUS_HEIGHT),
            Point::new(inner.display.size().width as i32, STATUS_HEIGHT),
        )
            .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
            .draw(&mut inner.display);
        self.redraw_log(&mut inner);
        inner.display.flush().expect("TODO: panic message");
    }

    pub async fn set_wifi_connected(&self, connected: bool) {
        let mut inner = self.inner.lock().await;

        inner.wifi_connected = connected;
        self.redraw_status(&mut inner);
    }

    pub async fn set_cloud_connected(&self, connected: bool) {
        let mut inner = self.inner.lock().await;

        inner.cloud_connected = connected;
        self.redraw_status(&mut inner);
    }

    pub async fn set_clients_connected(&self, clients: u8) {
        let mut inner = self.inner.lock().await;

        inner.clients_connected = clients;
        self.redraw_status(&mut inner);
    }

    pub async fn add_log_message(&self, message: String) {
        {
            let mut inner = self.inner.lock().await;
            inner.log_lines.push(message);
            if inner.log_lines.len() >= MAX_LOG_LINES {
                inner.log_lines.remove(0);
            }
        }
        self.refresh().await;
    }

    fn redraw_log(&self, inner: &mut MutexGuard<NoopRawMutex, DisplayManagerInnerState>) {
        let mut y = 8 + STATUS_HEIGHT;
        let lines = inner.log_lines.clone();
        for line in lines.iter() {
            let _ = Text::new(line, Point::new(0, y), self.text_style).draw(&mut inner.display);
            y += 7;
        }
        inner.display.flush().expect("TODO: panic message");
    }
    fn redraw_status(&self, inner: &mut MutexGuard<NoopRawMutex, DisplayManagerInnerState>) {
        let branding = Text::new(
            "PictoThing",
            Point::new(0, 10),
            self.large_text_style,
        );

        let wifi_icon = match inner.wifi_connected {
            true => Image::new(&WIFI_CONNECTED_ICON, Point::new((inner.display.size().width - 16) as i32, 0)),
            false => Image::new(&WIFI_DISCONNECTED_ICON, Point::new((inner.display.size().width - 16) as i32, 0)),
        };
        let cloud_icon = match inner.cloud_connected {
            true => Image::new(&CLOUD_CONNECTED_ICON, Point::new((inner.display.size().width - 32) as i32, 0)),
            false => Image::new(&CLOUD_DISCONNECTED_ICON, Point::new((inner.display.size().width - 32) as i32, 0)),
        };

        let clients_string = format!("{:0>2}", inner.clients_connected);
        let clients_connected = Text::new(clients_string.as_str(), Point::new(inner.display.size().width as i32 - 16 - 32, 10), self.large_text_style);

        branding.draw(&mut inner.display).expect("TODO: panic message");
        wifi_icon.draw(&mut inner.display).expect("TODO: panic message");
        cloud_icon.draw(&mut inner.display).expect("TODO: panic message");
        clients_connected.draw(&mut inner.display).expect("TODO: panic message");

        inner.display.flush().expect("TODO: panic message");
    }

}

#[embassy_executor::task]
pub async fn handle_updates(mut instance: DisplayManager) {
    let mut receiver = instance.event_chan.clone();
    loop {
        match receiver.receive().await {
            DisplayUpdate::SetWifiConnected(connected) => instance.set_wifi_connected(connected).await,
            DisplayUpdate::SetCloudConnected(connected) => instance.set_cloud_connected(connected).await,
            DisplayUpdate::SetClientsConnected(count) => instance.set_clients_connected(count).await,
            DisplayUpdate::AddLogMessage(msg) => instance.add_log_message(msg).await,
            DisplayUpdate::AddClientConnected => {
                instance.set_clients_connected(1).await;
            }
            DisplayUpdate::RemoveClientConnected => {
                instance.set_clients_connected(2).await;
            }
        }
    }
}


#[derive(Debug)]
pub enum DisplayUpdate {
    SetWifiConnected(bool),
    SetCloudConnected(bool),
    SetClientsConnected(u8),
    AddLogMessage(String),
    AddClientConnected,
    RemoveClientConnected,
}
