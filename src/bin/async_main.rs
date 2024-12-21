#![no_std]
#![no_main]

use embassy_executor::Spawner;

use embassy_time::Timer;
use esp_backtrace as _;
use esp_hal::{rng::Rng, timer::timg::TimerGroup};
use foa::{bg_task::SingleInterfaceRunner, FoAStackResources};
use log::info;
use foa_dswifi::{DsWiFiInitInfo, DsWiFiInterface, DsWiFiSharedResources};

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[embassy_executor::task]
async fn wifi_task(mut wifi_runner: SingleInterfaceRunner<'static, DsWiFiInterface>) -> ! {
    wifi_runner.run().await
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::logger::init_logger_from_env();

    info!("Hello, world!");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    let stack_resources = mk_static!(
        FoAStackResources<DsWiFiSharedResources>,
        FoAStackResources::new()
    );

    let (_ds_control, runner) = foa::new_with_single_interface::<DsWiFiInterface>(
        stack_resources,
        peripherals.WIFI,
        peripherals.RADIO_CLK,
        peripherals.ADC2,
        DsWiFiInitInfo::default(),
    ).await;

    info!("Spawning Wifi Task");
    spawner.spawn(wifi_task(runner)).unwrap();

    loop {
        Timer::after_secs(1).await;
    }
}