#![no_std]
#![no_main]
#![feature(future_join)]

use core::ffi::c_void;
use core::mem::MaybeUninit;
use defmt::{debug, error, info, warn};
use embassy_executor::Spawner;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use esp_hal::{rng::Rng, timer::timg::TimerGroup};
use esp_println::println;
use foa::bg_task::FoARunner;
use foa::{FoAResources, VirtualInterface};
use foa_dswifi::{DsWiFiInitInfo, DsWiFiInterface, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWiFiSharedResources, DsWifiClientMaskMath};
use foa_dswifi::pictochat_application::{PictoChatApplication, PictoChatUserManager};
use foa_dswifi::runner::DsWiFiRunner;

use {esp_backtrace as _, defmt as _};

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
async fn foa_task(mut foa_runner: FoARunner<'static>) -> ! {
    foa_runner.run().await
}

#[embassy_executor::task]
async fn dswifi_task(mut sta_runner: DsWiFiRunner<'static, 'static>) -> ! {
    sta_runner.run().await
}

extern "C" {
    fn phy_set_most_tpw(max_txpwr: i8) -> c_void;
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    init_heap();

    info!("Hello, world!");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    let stack_resources = mk_static!(FoAResources, FoAResources::new());
    let ([ds_vif, ..], foa_runner) = foa::init(
        stack_resources,
        peripherals.WIFI,
        peripherals.RADIO_CLK,
        peripherals.ADC2,
    );
    spawner.spawn(foa_task(foa_runner)).unwrap();

    // agc doenst really work correctly with my usecase here,
    // limit the max tx power to avoid it cycling insanely high and insanely low
    // this probably has unintended side effects
    unsafe { phy_set_most_tpw(8); }

    /*
    let (mut sta_control, sta_runner, net_device) = foa_sta::new_sta_interface(
        mk_static!(VirtualInterface<'static>, sta_vif),
        sta_resources,
        None,
    );
     */
    let ds_resources = mk_static!(DsWiFiSharedResources<'static>, DsWiFiSharedResources::default());
    let (ds_control,ds_runner) = foa_dswifi::new_ds_wifi_interface(
        mk_static!(VirtualInterface<'static>, ds_vif),
        ds_resources
    );
    spawner.spawn(dswifi_task(ds_runner)).unwrap();

    let mut pictochat_app = PictoChatApplication {
        ds_wifi_control: ds_control,
        user_state_manager: Mutex::new(PictoChatUserManager {
            users: [const { None };15],
        }),
        state_queue: Channel::new(),
    };

    pictochat_app.run().await;
}