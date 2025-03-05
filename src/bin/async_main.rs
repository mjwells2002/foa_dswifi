#![no_std]
#![no_main]
#![feature(future_join)]
#![feature(ip_from)]
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use defmt::{debug, error, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_futures::yield_now;
use embassy_net::{Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_net_wiznet::chip::W5500;
use embassy_net_wiznet::{Device, State};
use embassy_sync::channel::{Channel, DynamicSender, TrySendError};
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use esp_hal::{dma_buffers, dma_descriptors, rng::Rng, timer::timg::TimerGroup, Async};
use esp_hal::clock::CpuClock::_240MHz;
use esp_hal::dma::{DmaPriority, DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{GpioPin, Input, Level, Output, Pull};
use esp_hal::peripherals::SPI2;
use esp_hal::spi::master::{Config, Spi, SpiDma, SpiDmaBus};
use esp_hal::spi::Mode;
use esp_hal::time::RateExtU32;
use esp_println::println;
use foa::bg_task::FoARunner;
use foa::{FoAResources, VirtualInterface};
use ieee80211::mac_parser::MACAddress;
use static_cell::StaticCell;
use foa_dswifi::{DsWiFiInitInfo, DsWiFiInterface, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWiFiSharedResources, DsWifiClientMaskMath};
use foa_dswifi::pictochat_application::{PictoChatApplication, PictoChatUserManager, PictochatInterfaceEvent, PictochatSharedData};
use foa_dswifi::runner::DsWiFiRunner;
use static_cell::make_static;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
    DhcpConfig, Runner as NetRunner, StackResources as NetStackResources,
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_io_async::Write;
use esp_hal::uart::Uart;
use {esp_backtrace as _, defmt as _};
use foa_dswifi::pictochat_packets::MessagePayload;

//test network this is fine to be commited
const WIFI_NETWORK: &str = "Inception";
const WIFI_PASSWORD: &str = "l7TlGp6FeDZw7H";

const HEAP_SIZE: usize = 32 * 1024;

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

#[embassy_executor::task]
async fn pictochat_task(mut pictochat_app: PictoChatApplication<'static>) -> ! {
    pictochat_app.run().await
}

#[embassy_executor::task]
async fn ethernet_task(
    mut runner: embassy_net_wiznet::Runner<
        'static,
        W5500,
        ExclusiveDevice<SpiDmaBus<'static, Async>, Output<'static>, Delay>,
        Input<'static>,
        Output<'static>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, embassy_net_wiznet::Device<'static>>) -> ! {
    runner.run().await
}


async fn wait_for_config(stack: Stack<'static>) -> embassy_net::StaticConfigV4 {
    loop {
        if let Some(config) = stack.config_v4() {
            return config.clone();
        }
        yield_now().await;
    }
}

#[embassy_executor::task]
async fn esph_wifi_task(
    runner: embassy_net_esp_hosted::Runner<
        'static,
        ExclusiveDevice<SpiDmaBus<'static, Async>, Output<'static>, NoDelay>,
        Input<'static>,
        Output<'static>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn tcp_listen_task(stack: Stack<'static>, tx_channel: DynamicSender<'static, [u8;14]>) {
    let mut rx_buffer = [0; 1000];
    let mut tx_buffer = [0; 50];
    let mut buf = [0; 1000];
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));
        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        info!("Received connection from {:?}", socket.remote_endpoint());
        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("{:?}", e);
                    break;
                }
            };
            if n == 1 {
                socket.write_all("PI".as_bytes()).await.expect("TODO: panic message");
            } else if n == 14 {
                let mut totx = [0;14];
                totx.copy_from_slice(&buf[0..14]);
                tx_channel.send(totx).await;
                socket.write_all("OK".as_bytes()).await.expect("TODO: panic message");
            } else {
                //warn!("n == {}",n);
            }
        }
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(_240MHz));

    init_heap();

    info!("Hello, world!");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);


    let sck = peripherals.GPIO14;
    let miso = peripherals.GPIO12;
    let mosi = peripherals.GPIO13;
    let cs = Output::new(peripherals.GPIO15, Level::Low);

    let esph_handshake = Input::new(peripherals.GPIO26, Pull::Up);
    let esph_ready = Input::new(peripherals.GPIO25, Pull::None);
    let esph_reset = Output::new(peripherals.GPIO33, Level::Low);

    let dma_channel = peripherals.DMA_SPI2;

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(2000);
    let dma_rx_buf = DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    let spi = Spi::new(
        peripherals.SPI2,
        Config::default()
            .with_frequency(10.MHz())
            .with_mode(Mode::_1),
        ).unwrap()
        .with_sck(sck)
        .with_mosi(mosi)
        .with_miso(miso)
        .with_dma(dma_channel)
        .with_buffers(dma_rx_buf, dma_tx_buf)
        .into_async();

    let esph_spi_device = ExclusiveDevice::new_no_delay(spi, cs);

    static ESP_STATE: StaticCell<embassy_net_esp_hosted::State> = StaticCell::new();
    let (device, mut control, runner) = embassy_net_esp_hosted::new(
        ESP_STATE.init(embassy_net_esp_hosted::State::new()),
        esph_spi_device,
        esph_handshake,
        esph_ready,
        esph_reset,
    ).await;

    spawner.spawn(esph_wifi_task(runner)).unwrap();

    control.init().await.unwrap();
    control.connect(WIFI_NETWORK, WIFI_PASSWORD).await.unwrap();

    let net_stack_resources = mk_static!(NetStackResources<3>, NetStackResources::new());
    let (net_stack, net_runner) = embassy_net::new(
        device,
        embassy_net::Config::dhcpv4(DhcpConfig::default()),
        net_stack_resources,
        1234,
    );

    spawner.spawn(net_task(net_runner)).unwrap();

    info!("Waiting for DHCP...");

    let cfg = wait_for_config(net_stack).await;
    let local_addr = cfg.address.address();
    info!("IP address: {:?}", local_addr);

    /* do not make the dswifi interface any interface other than 0
        see https://github.com/esp32-open-mac/esp-wifi-hal/issues/5 for why
     */

    let stack_resources = mk_static!(FoAResources, FoAResources::new());
    let ([ds_vif, ..], foa_runner) = foa::init(
        stack_resources,
        peripherals.WIFI,
        peripherals.RADIO_CLK,
        peripherals.ADC2,
    );
    spawner.spawn(foa_task(foa_runner)).unwrap();

    let ds_resources = mk_static!(DsWiFiSharedResources<'static>, DsWiFiSharedResources::default());
    let (ds_control,ds_runner) = foa_dswifi::new_ds_wifi_interface(
        mk_static!(VirtualInterface<'static>, ds_vif),
        ds_resources
    );
    //todo: make this not hacky
    let mac = ds_control.mac_address.clone();
    spawner.spawn(dswifi_task(ds_runner)).unwrap();

    let pictochat_resources = mk_static!(PictochatSharedData, PictochatSharedData::default());
    let (pictochat_app, pictochat_interface) = PictoChatApplication::new(ds_control, pictochat_resources).await;

    spawner.spawn(pictochat_task(pictochat_app)).unwrap();
    let channel = mk_static!(Channel<NoopRawMutex,[u8;14],4>, Channel::new());

    spawner.spawn(tcp_listen_task(net_stack,channel.dyn_sender())).expect("TODO: panic message");
    let channel_rx = channel.dyn_receiver();
    loop {
        match select3(pictochat_interface.inbound_queue.receive(),pictochat_interface.event_queue.receive(),channel_rx.receive()).await {
            Either3::First(message) => {
                info!("got message len: {}",message.message.len());
                //todo: sending messages
                let mut out = message.clone();
                out.from = MACAddress::from(mac);
                info!("sound data? : {:?}",out.magic_1);
                info!("sound data 2? : {:?}",out.safezone);
                out.magic_1 = [0, 4, 0, 0, 255, 255, 02, 04, 05, 02, 09, 09, 07, 27];
                out.safezone = [00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00];
                match pictochat_interface.outbound_queue.try_send(out) {
                    Ok(_) => {}
                    Err(_) => {
                        warn!("something went wrong");
                    }
                }
            }
            Either3::Second(event) => {
                match event {
                    PictochatInterfaceEvent::ClientConnected(id) => {
                        info!("Client Joined {:?}", id.name)
                    }
                    PictochatInterfaceEvent::ClientDisconnected(id) => {
                        info!("Client Left {:?}", id.name)
                    }           
                }
            },
            Either3::Third(data) => {
                let mut out = MessagePayload {
                    ..Default::default()
                };
                out.from = MACAddress::from(mac);
                out.magic_1 = data;
                out.message = vec![0x11; 8*16];
                match pictochat_interface.outbound_queue.try_send(out) {
                    Ok(_) => {}
                    Err(_) => {
                        warn!("something went wrong");
                    }
                }
            }

        }
    }

}