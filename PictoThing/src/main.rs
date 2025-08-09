#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
#![feature(ip_from)]
#![feature(int_roundings)]
mod internal_flash;
mod util;
mod display;
mod http_server;

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::{format, vec};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::str::FromStr;
use defmt::{info, warn};
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal_embassy::UdpBuffers;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_futures::yield_now;
use embassy_net::{IpEndpoint, Ipv4Cidr, Stack, StaticConfigV4};
use embassy_sync::channel::{Channel, DynamicReceiver, DynamicSender};
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer, WithTimeout};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use esp_hal::{dma_buffers, i2c, rng::Rng, timer::timg::TimerGroup, Async};
use esp_hal::clock::CpuClock::_240MHz;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::master::{Config, Spi, SpiDmaBus};
use esp_hal::spi::Mode;
use foa::FoARunner;
use foa::{FoAResources, VirtualInterface};
use ieee80211::mac_parser::MACAddress;
use foa_dswifi::DsWiFiSharedResources;
use foa_dswifi::pictochat_application::{PictoChatApplication, PictochatInterface, PictochatInterfaceEvent, PictochatSharedData};
use foa_dswifi::runner::DsWiFiRunner;
use embassy_net::{
    dns::DnsSocket,
    DhcpConfig, StackResources as NetStackResources,
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_io_async::{Read, Write};
use {esp_backtrace as _, defmt as _};
use foa_dswifi::pictochat_packets::MessagePayload;
use esp_hal::time::Rate;
use edge_nal::{UdpBind};
use embassy_net::udp::PacketMetadata;
use embassy_net_esp_hosted::{Control, Security};
use esp_hal::i2c::master::I2c;
use esp_hal::psram::psram_raw_parts;
use crate::display::DisplayUpdate;
use crate::internal_flash::InternalFlash;
use embassy_net_esp_hosted::ApStatus;
use embassy_net_esp_hosted::Bandwidth::Ht20;
use esp_bootloader_esp_idf::ota::Slot;
use esp_bootloader_esp_idf::partitions::DataPartitionSubType;
use crate::http_server::http_listen_task;
use crate::util::get_file;

use {esp_backtrace as _, defmt as _};

esp_bootloader_esp_idf::esp_app_desc!();


fn init_heap(psram_start: *mut u8, psram_size: usize) {
    unsafe {
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            psram_start,
            psram_size,
            esp_alloc::MemoryCapability::External.into(),
        ));
    }
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
async fn pictochat_task(pictochat_app: &'static mut PictoChatApplication<'static>) -> ! {
    pictochat_app.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, embassy_net_esp_hosted::NetDriver<'static>>) -> ! {
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
async fn udp_send_task(stack: Stack<'static>) {
    let mut sock_rx_buffer = vec![0u8; 1200];
    let mut sock_tx_buffer= vec![0u8; 1200];
    let mut rx_meta = vec![PacketMetadata::EMPTY; 16];
    let mut tx_meta = vec![PacketMetadata::EMPTY; 16];
    let mut socket = embassy_net::udp::UdpSocket::new(stack,&mut rx_meta, &mut sock_rx_buffer,&mut tx_meta, &mut sock_tx_buffer);
    info!("Listening on TCP:1234...");
    if let Err(e) = socket.bind(1234) {
        warn!("accept error: {:?}", e);
    }
    let mut buf= vec![0u8; 1200];
    let (n, ep) = socket.recv_from(&mut buf).await.unwrap();
    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
        info!("rxd from {}: {}", ep, s);
    }
    loop {
        socket.send_to(&mut buf, ep).await.expect("TODO: panic message");
    }
}

#[embassy_executor::task]
async fn tcp_listen_task(stack: Stack<'static>) {
    let buf = vec![0u8; 10_000];
    let mut sock_rx_buffer = vec![0u8; 100_000];
    let mut sock_tx_buffer= vec![0u8; 100_000];
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut sock_rx_buffer, &mut sock_tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));
        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        info!("Received connection from {:?}", socket.remote_endpoint());
        loop {
            let mut r = socket.write_all(&buf).await;
            if r.is_err() {
                break;
            }
            r = socket.flush().await;
            if r.is_err() {
                break;
            }
        }
    }
}

const PROTO_VERSION: u16 = 0;

#[repr(u8)]
enum PacketType {
    HANDSHAKE = 0x00,
    ERROR = 0xAD,
    DATA = 0xFD,
}
fn build_handshake(protocol_version: u16) -> [u8; 3] {
    let protocol_bytes = protocol_version.to_be_bytes();
    [PacketType::HANDSHAKE as u8, protocol_bytes[0], protocol_bytes[1]]
}
fn parse_handshake(handshake: [u8; 3]) -> Result<u16, ()> {
    if handshake[0] == PacketType::HANDSHAKE as u8 {
        let mut temp = [0u8; 2];
        temp.copy_from_slice(&handshake[1..]);
        let version = u16::from_be_bytes(temp);
        Ok(version)
    } else {
        Err(())
    }
}

#[embassy_executor::task]
async fn server_connection_task(stack: Stack<'static>, tx_channel: DynamicSender<'static, Vec<u8>>, rx_channel: DynamicReceiver<'static, Vec<u8>>, display: DynamicSender<'static, DisplayUpdate>) {
    let dns_socket = DnsSocket::new(stack);

    let mut sock_rx_buffer = vec![0u8; 5_000];
    let mut sock_tx_buffer = vec![0u8; 5_000];
    let mut socket = embassy_net::tcp::TcpSocket::new(stack,&mut sock_rx_buffer,&mut sock_tx_buffer);
    socket.set_keep_alive(Some(Duration::from_secs(5)));
    socket.set_timeout(Some(Duration::from_secs(120)));

    loop {
        let server = dns_socket
            .query("hen.breadloaf.xyz", embassy_net::dns::DnsQueryType::A)
            .await;

        match server {
            Ok(server) => {
                info!("Found server: {:?}", server[0]);
                if socket.connect(IpEndpoint {
                    addr: server[0],
                    port: 5812,
                }).await.is_ok() {
                    info!("Connected to server!");
                    display.send(DisplayUpdate::SetCloudConnected(true)).await;
                    break;
                }
            }
            Err(e) => {
                info!("Failed to find server: {:?}", e);
            }
        }

        info!("Failed to connect to server, retrying! in 10 seconds");
        Timer::after_secs(10).await;
    }

    socket.write(&build_handshake(PROTO_VERSION)).await.unwrap();
    socket.flush().await.unwrap();
    let mut handshake_resp = [0u8;3];
    socket.read_exact(&mut handshake_resp).await.unwrap();
    let s_ver = parse_handshake(handshake_resp).unwrap();
    info!("Server Version is : {}",s_ver);
    let mut rbuf = [0u8;1];
    loop {
        match select(socket.read_exact(&mut rbuf),rx_channel.receive()).await {
            Either::First(_) => {
                match rbuf[0] {
                    0xAD => {
                        panic!("error");
                    }
                    0xFD => {
                        let mut data_size = [0u8;2];
                        socket.read_exact(&mut data_size).await.unwrap();
                        let read_size = u16::from_be_bytes(data_size);
                        let mut data = vec![0u8;read_size as usize];
                        socket.read_exact(&mut data).await.unwrap();
                        info!("got {} bytes",{read_size});
                        tx_channel.send(data).await;
                    }
                    _ => {}
                }
            },
            Either::Second(data) => {
                info!("sending {}", data.len());
                let mut asdf = [0xFDu8,0,0];
                let a = (data.len() as u16).to_be_bytes();
                asdf[1] = a[0];
                asdf[2] = a[1];
                socket.write_all(&asdf).await.unwrap();
                socket.write_all(&data).await.unwrap();
                socket.flush().await.unwrap();
            }
        }
    }
}


#[embassy_executor::task]
async fn captive_portal_dns_task(stack: Stack<'static>) {
    let mut tx_buf = vec![0; 1500];
    let mut rx_buf = vec![0; 1500];
    let box_buffers = Box::new(UdpBuffers::<2,250,250,2>::new());
    let udp = edge_nal_embassy::Udp::new(stack,&box_buffers);
    edge_captive::io::run(
        &udp,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 53),
        &mut tx_buf,
        &mut rx_buf,
        Ipv4Addr::new(10, 82, 50, 1),
        core::time::Duration::from_secs(60),
    ).await.unwrap();
}

#[embassy_executor::task]
async fn dhcp_server_task(stack: Stack<'static>) {
    let mut buf = vec![0; 1500];
    let ip = Ipv4Addr::new(10, 82, 50, 1);
    let box_buffers = Box::new(UdpBuffers::<2,500,500,2>::new());
    let udp = edge_nal_embassy::Udp::new(stack,&box_buffers);
    let mut socket = udp.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED),edge_dhcp::io::DEFAULT_SERVER_PORT)).await.unwrap();
    let mut gw_buf = [ip];
    let dns_buf = [ip];
    let mut server_opt = ServerOptions::new(ip, Some(&mut gw_buf));
    server_opt.captive_url = Some("http://10.82.50.1");
    server_opt.dns = &dns_buf;
    edge_dhcp::io::server::run(
        &mut Server::<_,16>::new_with_et(ip),
        &server_opt,
        &mut socket,
        &mut buf
    ).await.unwrap();
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(_240MHz));

    let mut should_erase = false;

    {
        let (start, size) = psram_raw_parts(&peripherals.PSRAM);
        info!("PSRAM size = {}", size);
        info!("PSRAM start = {:#x}", start as usize);

        init_heap(start, size);
    }

    let mut storage = esp_storage::FlashStorage::new();

    let mut buffer = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
    let pt = esp_bootloader_esp_idf::partitions::read_partition_table(&mut storage, &mut buffer)
        .unwrap();

    let ota_part = pt
        .find_partition(esp_bootloader_esp_idf::partitions::PartitionType::Data(
            DataPartitionSubType::Ota,
        ))
        .unwrap()
        .unwrap();

    let mut ota_part = ota_part.as_embedded_storage(&mut storage);

    //TODO: build the bootloader with auto-rollback
    let mut ota = esp_bootloader_esp_idf::ota::Ota::new(&mut ota_part).unwrap();
    let current = ota.current_slot().unwrap();

    info!(
        "current image state {:?}",
        ota.current_ota_state()
    );

    info!("current {:?} - next {:?}", current, current.next());

    if ota.current_slot().unwrap() != Slot::None
        && (ota.current_ota_state().unwrap() == esp_bootloader_esp_idf::ota::OtaImageState::New
        || ota.current_ota_state().unwrap()
        == esp_bootloader_esp_idf::ota::OtaImageState::PendingVerify)
    {
        info!("Changed state to VALID");
        ota.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::Valid)
            .unwrap();
    }

    let mut config_start: usize = 0;
    let mut config_end: usize = 0;

    for i in 0..pt.len() {
        match pt.get_partition(i) {
            Ok(part) => {
                info!("Partition {} {:?}",i, part);
                if part.partition_type() == esp_bootloader_esp_idf::partitions::PartitionType::Data(DataPartitionSubType::Undefined) {
                    if part.label_as_str() == "config" {
                        config_start = part.offset() as usize;
                        config_end = config_start + part.len() as usize;
                    }
                }
            }
            Err(_) => {}
        }
    }

    if config_start + config_end <= 0 {
        panic!("Config partition not found");
    }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    let mut rng = Rng::new(peripherals.RNG);
    let flash = mk_static_dram2!(InternalFlash,InternalFlash::new(rng.random(),config_start..config_end));

    if !should_erase {
        if !flash.mount().await {
            should_erase = true;
        }
    }

    info!("Hello, world!");

    let i2c = I2c::new(
        peripherals.I2C0,
        i2c::master::Config::default(),
    )
        .unwrap()
        .with_sda(peripherals.GPIO32)
        .with_scl(peripherals.GPIO33)
        .into_async();

    let display = display::DisplayManager::new(i2c,spawner);

    display.send(DisplayUpdate::AddLogMessage(String::from("Getting Ready ..."))).await;

    let mut force_ekv_erase = Input::new(peripherals.GPIO34, InputConfig::default()); //external pull up

    // let spi3_sclk = peripherals.GPIO25;
    // let spi3_miso = peripherals.GPIO22;
    // let spi3_mosi = peripherals.GPIO21 ;
    // let mut spi3_cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    //
    // //let spi3_dma_ch = peripherals.DMA_SPI2;
    // //let (spi3_rx_buffer, spi3_rx_descriptors, spi3_tx_buffer, spi3_tx_descriptors) = dma_buffers!(4000);
    // //let spi3_dma_rx_buf = DmaRxBuf::new(spi3_rx_descriptors, spi3_rx_buffer).unwrap();
    // //let spi3_dma_tx_buf = DmaTxBuf::new(spi3_tx_descriptors, spi3_tx_buffer).unwrap();
    //
    // let mut spi3 = Spi::new(
    //     peripherals.SPI2,
    //     Config::default()
    //         .with_frequency(Rate::from_khz(400))
    //         .with_mode(Mode::_0),
    // ).unwrap()
    //     .with_sck(spi3_sclk)
    //     .with_mosi(spi3_mosi)
    //     .with_miso(spi3_miso)
    //     //.with_dma(spi3_dma_ch)
    //     //.with_buffers(spi3_dma_rx_buf, spi3_dma_tx_buf)
    //     .into_async();
    // loop {
    //     match sdspi::sd_init(&mut spi3, &mut spi3_cs).await {
    //         Ok(_) => break,
    //         Err(e) => {
    //             warn!("Sd init error: {:?}", e);
    //             embassy_time::Timer::after_millis(10).await;
    //         }
    //     }
    // }
    // let sd_spi = ExclusiveDevice::new(spi3, spi3_cs, embassy_time::Delay).unwrap();
    // let mut sd = SdSpi::<_, _, aligned::A1>::new(sd_spi, embassy_time::Delay);
    // loop {
    //     let res = sd.init().await;
    //     if res.is_ok() {

    //         info!("SD Initialization complete!");
    //         sd.spi()
    //             .bus_mut().apply_config(&Config::default()
    //             .with_frequency(Rate::from_mhz(20))
    //             .with_mode(Mode::_0)).expect("Failed to increase bus speed");
    //         break;
    //     } else {
    //         warn!("{:?}",res.expect_err("not possible"));
    //     }
    //     info!("Failed to init SD card, retrying...");
    //
    //     Timer::after_nanos(5000).await;
    // }
    
    let mut led = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());

    if force_ekv_erase.is_low() {
        display.send(DisplayUpdate::AddLogMessage(String::from("Format Triggered ..."))).await;
        display.send(DisplayUpdate::AddLogMessage(String::from("Continue holding to erase config"))).await;


        info!("Manual Format Triggered ...");
        led.set_high();
        should_erase = true;
        for _ in 0..13 {
            let was_button_released = force_ekv_erase.wait_for_high().with_timeout(Duration::from_millis(250)).await;
            if was_button_released.is_ok() {
                should_erase = false;
                display.send(DisplayUpdate::AddLogMessage(String::from("Format Canceled"))).await;
                info!("Manual format canceled");
                break;
            }
            led.toggle();
        }
        led.set_high();
    }

    let mut wifi_ssid_stack = [0u8;32];
    let mut wifi_password_stack = [0u8;64];
    let mut no_config = false;
    let mut is_open_network = false;
    let mut ssid_size: usize = 0;
    let mut psk_size: usize = 0;

    if should_erase {
        display.send(DisplayUpdate::AddLogMessage(String::from("Erasing Config ..."))).await;
        flash.format_ekv().await;
        led.set_low();
         display.send(DisplayUpdate::AddLogMessage(String::from("Done"))).await;
    }

    (no_config, ssid_size) = flash.read_key(b"wifi_ssid", &mut wifi_ssid_stack).await;
    (is_open_network, psk_size) = flash.read_key(b"wifi_psk", &mut wifi_password_stack).await;

    no_config = !no_config;
    is_open_network = !is_open_network;

    let mut wifi_ssid = vec![0u8;ssid_size];
    let mut wifi_password = vec![0u8;psk_size];

    if ssid_size > 0 {
        wifi_ssid.copy_from_slice(&wifi_ssid_stack[..ssid_size]);
    }

    if psk_size > 0 {
        wifi_password.copy_from_slice(&wifi_password_stack[..psk_size]);
    }


    info!("Has SSID: {}", !no_config);
    info!("Is Open Network: {}", is_open_network);
    info!("SSID: {:?}", wifi_ssid.as_slice());
    info!("PSK: {:?}", wifi_password.as_slice());

    // let spi3_sclk = peripherals.GPIO25;
    // let spi3_miso = peripherals.GPIO22;
    // let spi3_mosi = peripherals.GPIO21 ;
    // let mut spi3_cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    // 
    // //let spi3_dma_ch = peripherals.DMA_SPI2;
    // //let (spi3_rx_buffer, spi3_rx_descriptors, spi3_tx_buffer, spi3_tx_descriptors) = dma_buffers!(4000);
    // //let spi3_dma_rx_buf = DmaRxBuf::new(spi3_rx_descriptors, spi3_rx_buffer).unwrap();
    // //let spi3_dma_tx_buf = DmaTxBuf::new(spi3_tx_descriptors, spi3_tx_buffer).unwrap();
    // 
    // let mut spi3 = Spi::new(
    //     peripherals.SPI2,
    //     Config::default()
    //         .with_frequency(Rate::from_khz(400))
    //         .with_mode(Mode::_0),
    // ).unwrap()
    //     .with_sck(spi3_sclk)
    //     .with_mosi(spi3_mosi)
    //     .with_miso(spi3_miso)
    //     //.with_dma(spi3_dma_ch)
    //     //.with_buffers(spi3_dma_rx_buf, spi3_dma_tx_buf)
    //     .into_async();
    // loop {
    //     match sdspi::sd_init(&mut spi3, &mut spi3_cs).await {
    //         Ok(_) => break,
    //         Err(e) => {
    //             warn!("Sd init error: {:?}", e);
    //             embassy_time::Timer::after_millis(10).await;
    //         }
    //     }
    // }
    // let sd_spi = ExclusiveDevice::new(spi3, spi3_cs, embassy_time::Delay).unwrap();
    // let mut sd = SdSpi::<_, _, aligned::A1>::new(sd_spi, embassy_time::Delay);
    // loop {
    //     let res = sd.init().await;
    //     if res.is_ok() {
    //         info!("SD Initialization complete!");
    //         sd.spi()
    //             .bus_mut().apply_config(&Config::default()
    //             .with_frequency(Rate::from_mhz(20))
    //             .with_mode(Mode::_0)).expect("Failed to increase bus speed");
    //         break;
    //     } else {
    //         warn!("{:?}",res.expect_err("not possible"));
    //     }
    //     info!("Failed to init SD card, retrying...");
    // 
    //     Timer::after_nanos(5000).await;
    // 
    // }
    // let inner = BufStream::<_, 512>::new(sd);
    // let fs = embedded_fatfs::FileSystem::new(inner, FsOptions::new()).await.unwrap();
    // 
    // let mut f = fs.root_dir().create_file("test.log").await.unwrap();
    // let hello = b"Hello world!";
    // info!("Writing to file...");
    // f.write_all(hello).await.unwrap();
    // f.flush().await.unwrap();
    // 
    // let mut buf = [0u8; 12];
    // f.rewind().await.unwrap();
    // f.read_exact(&mut buf[..]).await.unwrap();
    // info!(
    //     "Read from file: {}",
    //     core::str::from_utf8(&buf[..]).unwrap()
    // );
    // f.close().await.unwrap();
    // 
    //  {
    //         let mut f = fs.root_dir().create_file("iotest.bin").await.unwrap();
    //         let mut write_size: usize = 0;
    //         let write_buf = vec![0u8;32768];
    //         let start = Instant::now();
    //         while write_size <= 1_000_000 {
    //             f.write_all(&write_buf).await.unwrap();
    //             write_size = write_size + write_buf.len();
    //         }
    //         f.flush().await.unwrap();
    //         let current = Instant::now();
    //         let millis = (current-start).as_millis();
    //         info!("Write {} Bytes in {}",write_size,millis);
    //         let bps = write_size as f32 / (millis as f32 / 1000f32);
    //         info!("Write BPS: {}",bps);
    //  }
    //  {
    //         let mut f = fs.root_dir().open_file("iotest.bin").await.unwrap();
    //         let mut read_size: usize = 0;
    //         let mut read_buf = vec![0u8;32768];
    //         let start = Instant::now();
    //         loop {
    //             let cur_read = f.read(&mut read_buf).await.unwrap();
    //             read_size += cur_read;
    //             if cur_read == 0 {
    //                 break;
    //             }
    //         }
    //         //f.flush().await.unwrap();
    //         let current = Instant::now();
    //         let millis = (current-start).as_millis();
    //         info!("Read {} Bytes in {}",read_size,millis);
    //         let bps = read_size as f32 / (millis as f32 / 1000f32);
    //         info!("Read BPS: {}",bps);
    // }

    let sck = peripherals.GPIO18;
    let miso = peripherals.GPIO19;
    let mosi = peripherals.GPIO23;
    let cs = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());

    let esph_handshake = Input::new(peripherals.GPIO26, InputConfig::default().with_pull(Pull::Up));
    let esph_ready = Input::new(peripherals.GPIO36, InputConfig::default().with_pull(Pull::None));
    let esph_reset = Output::new(peripherals.GPIO27, Level::Low, OutputConfig::default());

    let dma_channel = peripherals.DMA_SPI3;

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(2048);
    let dma_rx_buf = DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    let spi = Spi::new(
        peripherals.SPI3,
        Config::default()
            .with_frequency(Rate::from_mhz(20))
            .with_mode(Mode::_1),
        ).unwrap()
        .with_sck(sck)
        .with_mosi(mosi)
        .with_miso(miso)
        .with_dma(dma_channel)
        .with_buffers(dma_rx_buf, dma_tx_buf)
        .into_async();

    let esph_spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

    //static ESP_STATE: StaticCell<embassy_net_esp_hosted::State> = StaticCell::new();
    let esp_state = mk_static_dram2!(embassy_net_esp_hosted::State, embassy_net_esp_hosted::State::new());
    let (device, control, runner) = embassy_net_esp_hosted::new(
        esp_state,
        esph_spi_device,
        esph_handshake,
        esph_ready,
        esph_reset,
    ).await;

    let control_mutex = Mutex::<NoopRawMutex,Control>::from(control);

    spawner.spawn(esph_wifi_task(runner)).unwrap();

    let mut seed_bytes = [0u8;8];
    rng.read(&mut seed_bytes);
    let seed: u64 = u64::from_le_bytes(seed_bytes);
    let net_stack_resources = mk_static_dram2!(NetStackResources<20>, NetStackResources::new());
    let (net_stack, net_runner) = embassy_net::new(
        device,
        embassy_net::Config::dhcpv4(DhcpConfig::default()),
        net_stack_resources,
        seed,
    );

    spawner.spawn(net_task(net_runner)).unwrap();

    {
        let mut control = control_mutex.lock().await;
        control.init().await.unwrap();
        if !no_config {
            if let Err(_) = control.connect(&String::from_utf8(Vec::from(wifi_ssid)).unwrap(), &String::from_utf8(Vec::from(wifi_password)).unwrap()).await {
                no_config = true;
                display.send(DisplayUpdate::AddLogMessage(String::from("Unable to connect to wifi".to_string()))).await;
            }
        }
    }

    if no_config {
        let mut control = control_mutex.lock().await;
        net_stack.set_config_v4(embassy_net::ConfigV4::Static(StaticConfigV4 {
            address: Ipv4Cidr::new(Ipv4Addr::from_octets([10,82,50,1]),24),
            gateway: None,
            dns_servers: Default::default(),
        }));
        spawner.spawn(dhcp_server_task(net_stack)).unwrap();
        spawner.spawn(captive_portal_dns_task(net_stack)).unwrap();
        let mac = control.get_mac_addr().await.unwrap();
        let ssid = format!("PictoThing-{:02X}{:02X}{:02X}",mac[3],mac[4],mac[5]);

        display.send(DisplayUpdate::AddLogMessage(String::from("Please reconfigure".to_string()))).await;
        display.send(DisplayUpdate::AddLogMessage(format!("AP: {}",ssid))).await;
        display.send(DisplayUpdate::AddLogMessage("IP: 10.82.50.1".to_string())).await;

        info!("No Config, or unable to connect to WiFi, starting AP");
        control.set_ap_mode().await.expect("TODO: panic message");

        control.start_ap(ApStatus {
            ssid: heapless::String::from_str(ssid.as_ref()).unwrap(),
            psk: heapless::String::from_str("").unwrap(),
            channel: 11,
            security: Security::Open,
            max_connections: 8,
            hidden: false,
            bandwidth: Ht20,
        }).await.expect("TODO: panic message");
    } else {
        display.send(DisplayUpdate::AddLogMessage("Wifi UP, Waiting for IP".to_string())).await;

        info!("Waiting for DHCP...");
        let cfg = wait_for_config(net_stack).await;
        let local_addr = cfg.address.address();
        info!("IP address: {:?}", local_addr);
        display.send(DisplayUpdate::AddLogMessage(format!("IP {:?}",local_addr))).await;

        display.send(DisplayUpdate::SetWifiConnected(true)).await;
    }

    spawner.spawn(http_listen_task(net_stack,flash, control_mutex)).expect("TODO: panic message");

    /* do not make the dswifi interface any interface other than 0
        see https://github.com/esp32-open-mac/esp-wifi-hal/issues/5 for why
     */

    let stack_resources = mk_static_dram2!(FoAResources, FoAResources::new());
    let ([ds_vif, ..], foa_runner) = foa::init(
        stack_resources,
        peripherals.WIFI,
        peripherals.ADC2,
    );
    spawner.spawn(foa_task(foa_runner)).unwrap();

    let ds_resources = mk_static_dram2!(DsWiFiSharedResources<'static>, DsWiFiSharedResources::default());
    let (ds_control,ds_runner) = foa_dswifi::new_ds_wifi_interface(
        mk_static_dram2!(VirtualInterface<'static>, ds_vif),
        ds_resources
    );
    //todo: make this not hacky
    let mac = ds_control.mac_address.clone();
    spawner.spawn(dswifi_task(ds_runner)).unwrap();

    let pictochat_resources = mk_static_dram2!(PictochatSharedData, PictochatSharedData::default());
    let (pictochat_app, pictochat_interface) = mk_static_dram2!((PictoChatApplication,PictochatInterface),PictoChatApplication::new(ds_control, pictochat_resources).await);

    spawner.spawn(pictochat_task(pictochat_app)).unwrap();
    let channel = mk_static_dram2!(Channel<NoopRawMutex,Vec<u8>,4>, Channel::new());
    let channel_2 = mk_static_dram2!(Channel<NoopRawMutex,Vec<u8>,4>, Channel::new());

    spawner.spawn(tcp_listen_task(net_stack)).expect("TODO: panic message");
    spawner.spawn(udp_send_task(net_stack)).expect("aaa");
    spawner.spawn(server_connection_task(net_stack,channel.dyn_sender(),channel_2.dyn_receiver(),display)).expect("AHHHHHHHHHHHHHHHHHHHH ITS ON FIRE");

    let channel_rx = channel.dyn_receiver();
    let channel_tx = channel_2.dyn_sender();

    let mut bad_apple_offset = 0;
    let mut last_frame= Instant::now();

    let mut ticker = Ticker::every(Duration::from_millis(166));
    loop {
        match select4(pictochat_interface.inbound_queue.receive(),pictochat_interface.event_queue.receive(),channel_rx.receive(),ticker.next()).await {
            Either4::First(message) => {
                info!("got message len: {}",message.message.len());
                //todo: sending messages
                let mut out = message.clone();
                //channel_tx.send(out.message.clone()).await;

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
            Either4::Second(event) => {
                match event {
                    PictochatInterfaceEvent::ClientConnected(id) => {
                        info!("Client Joined {:?}", id.name);
                        //display.send(DisplayUpdate::AddLogMessage(format!("Client Joined {:?}", id.name))).await;
                        //display.send(DisplayUpdate::AddClientConnected).await;
                    }
                    PictochatInterfaceEvent::ClientDisconnected(id) => {
                        info!("Client Left {:?}", id.name);
                        //display.send(DisplayUpdate::AddLogMessage(format!("Client Left {:?}", id.name))).await;
                        //display.send(DisplayUpdate::RemoveClientConnected).await;
                    }
                }
            },
            Either4::Third(data) => {
                let mut out = MessagePayload {
                    ..Default::default()
                };
                out.from = MACAddress::from(mac);
                out.message = data;
                match pictochat_interface.outbound_queue.try_send(out) {
                    Ok(_) => {}
                    Err(_) => {
                        warn!("something went wrong");
                    }
                }
            },
            Either4::Fourth(_) => {
                //display.send(DisplayUpdate::AddLogMessage("Tick".to_string())).await;
                let bad_apple_file = get_file("bad_apple.sbin").unwrap();
                let bad_apple_slice = &bad_apple_file[bad_apple_offset..bad_apple_offset+10240];
                bad_apple_offset += 10240;
                if bad_apple_offset >= bad_apple_file.len() {
                    bad_apple_offset = 0;
                }
                let mut out = MessagePayload {
                    ..Default::default()
                };
                out.from = MACAddress::from(mac);
                out.message = bad_apple_slice.to_vec();

                pictochat_interface.outbound_queue.send(out).await;
                let current_frame = Instant::now();
                info!("Time to send: {:?}",(current_frame - last_frame).as_millis());
                let fps = 1000f32 / ((current_frame - last_frame).as_millis()*2) as f32;
                info!("FPS: {}",fps);
                last_frame = current_frame;
                // let networks = control.get_scan_network_list().await.unwrap();
                //
                // info!("found networks {}:", networks.count);
                // for network in networks.entries {
                //     if network.ssid.is_empty() {
                //         info!("*hidden*: {}/{}", network.rssi as i32,network.chnl);
                //     } else {
                //         info!("{}: {}/{}", network.ssid, network.rssi as i32,network.chnl);
                //     }
                // }
                // let stats: HeapStats = esp_alloc::HEAP.stats();
                // println!("{}", stats);
            }

        }
    }

}
