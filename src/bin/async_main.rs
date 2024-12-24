#![no_std]
#![no_main]

use core::mem::MaybeUninit;
use embassy_executor::Spawner;

use embassy_time::Timer;
use esp_alloc::EspHeap;
use esp_backtrace as _;
use esp_hal::{rng::Rng, timer::timg::TimerGroup};
use foa::{bg_task::SingleInterfaceRunner, FoAStackResources};
use log::{info, LevelFilter};
use foa_dswifi::{DsWiFiInitInfo, DsWiFiInterface, DsWiFiSharedResources};

const HEAP_SIZE: usize = 1 * 1024;

fn init_heap() {
    static mut HEAP: MaybeUninit<[u8; HEAP_SIZE]> = MaybeUninit::uninit();

    unsafe {
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            HEAP.as_mut_ptr() as *mut u8,
            HEAP_SIZE,
            esp_alloc::MemoryCapability::Internal.into(),
        ));

    }
}

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

    init_heap();


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