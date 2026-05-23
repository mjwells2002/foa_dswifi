use core::future::Future;
use defmt::{info, Format};
use edge_captive::reply;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Select};
use embassy_futures::select::Either::{First, Second};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use embassy_time::{Duration, Ticker};
use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::gpio::Level;
use crate::util::TaskMailbox;
use esp_hal::peripherals::Peripherals;
use esp_hal::rmt::{PulseCode, Rmt, TxChannelAsync, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;

static MAILBOX: TaskMailbox<CriticalSectionRawMutex, LedCommand, Result<u8, LedError>, 4> = TaskMailbox::new();

#[derive(Debug,Format)]
pub enum LedError {
    HardwareFault,
}

type Rgb = (u8, u8, u8);
pub enum LedCommand {
    Set(Rgb),
}

pub struct LedController {

}

impl LedController {
    pub fn init(spawner: &Spawner) {
        MAILBOX.init();
        spawner.spawn(led_task()).unwrap();
    }

    pub async fn set(rgb: Rgb) -> Result<u8, LedError> {
        MAILBOX.request(LedCommand::Set(rgb)).await
    }
}

const NUM_LEDS: usize = 2;
// 24 pulse codes per LED (8 bits × 3 channels) + 1 reset/end-marker
const BUF_LEN: usize = NUM_LEDS * 24 + 1;
// Each RMT channel block holds 64 entries; allocate enough blocks for the buffer
const RMT_MEM_BLOCKS: u8 = ((BUF_LEN + 63) / 64) as u8;

const T0H: u16 = 32;    // 400 ns
const T0L: u16 = 68;    // 850 ns
const T1H: u16 = 64;    // 800 ns
const T1L: u16 = 36;    // 450 ns
const T_RESET: u16 = 4000; // 50 µs

pub type PulseCodeA = u32;

fn encode_ws2812b(colors: &[(u8, u8, u8)], buf: &mut [PulseCodeA]) {
    let mut i = 0;
    for &(r, g, b) in colors {
        for byte in [g, r, b] {
            for shift in (0..8).rev() {
                buf[i] = if byte & (1 << shift) != 0 {
                    PulseCode::new(Level::High, T1H, Level::Low, T1L)
                } else {
                    PulseCode::new(Level::High, T0H, Level::Low, T0L)
                };
                i += 1;
            }
        }
    }
    // Low for T_RESET; length2 = 0 ends transmission
    buf[i] = PulseCode::new(Level::Low, T_RESET, Level::Low, 0);
}

#[embassy_executor::task]
async fn led_task() {
    let peripherals = unsafe { Peripherals::steal() }; //evil //TODO: dont do this

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80))
        .unwrap()
        .into_async();

    let mut channel = rmt
        .channel0
        .configure_tx(
            peripherals.GPIO2,
            TxChannelConfig::default()
                .with_clk_divider(1)
                .with_idle_output_level(Level::Low)
                .with_idle_output(true)
                .with_carrier_modulation(false)
                .with_memsize(RMT_MEM_BLOCKS),
        )
        .unwrap();

    let mut buf = [PulseCode::new(Level::Low, T_RESET, Level::Low, 0); BUF_LEN];

    let mut ticker = Ticker::every(Duration::from_millis(200));

    loop {
        match select(MAILBOX.receive(), ticker.next()).await {
            First((cmd,reply)) => {
                match cmd {
                    LedCommand::Set(rgb_value) => {
                        encode_ws2812b(&[rgb_value; NUM_LEDS], &mut buf);
                        match channel.transmit(&buf).await {
                            Ok(_) => {
                                reply.send(Ok(NUM_LEDS as u8)).await;
                            },
                            Err(e) => {
                                reply.send(Err(LedError::HardwareFault)).await;
                            }
                        }
                    }
                }
            },
            Second(_) => {
                //TODO: stuff
            }
        }
    }
}