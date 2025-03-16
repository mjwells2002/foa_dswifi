#![no_std]
#![no_main]
#![feature(future_join)]
#![feature(ip_from)]
#![feature(int_roundings)]
#![feature(impl_trait_in_assoc_type)]
extern crate alloc;

use alloc::string::{String, ToString};
use alloc::{format, vec};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::cmp::min;
use core::ffi::c_void;
use core::fmt::{Debug, Display};
use core::mem::MaybeUninit;
use core::ops::Range;
use core::slice;
use block_device_adapters::{BufStream, BufStreamError};
use defmt::{debug, error, info, warn};
use edge_http::io::server::{Connection, DefaultServer, Handler};
use edge_http::Method;
use edge_nal_embassy::{Tcp, TcpAccept, TcpBuffers, TcpSocket};
use ekv::{config, Database, FormatError, MountError, ReadError, ReadTransaction};
use ekv::flash::PageID;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_futures::yield_now;
use embassy_net::{Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_net_wiznet::chip::W5500;
use embassy_net_wiznet::{Device, State};
use embassy_sync::channel::{Channel, DynamicSender, TrySendError};
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer, WithTimeout};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use esp_hal::{dma_buffers, dma_descriptors, ram, rng::Rng, timer::timg::TimerGroup, Async};
use esp_hal::clock::CpuClock::_240MHz;
use esp_hal::dma::{DmaPriority, DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{GpioPin, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{SPI2, SPI3};
use esp_hal::spi::master::{Config, Spi, SpiDma, SpiDmaBus};
use esp_hal::spi::Mode;
use esp_println::println;
use foa::FoARunner;
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
use embedded_fatfs::FsOptions;
use embedded_io_async::{ErrorType, Read, Seek, SeekFrom, Write};
use embedded_sdmmc::{Block, BlockDevice, BlockIdx, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use embedded_sdmmc::sdcard::Error;
use esp_alloc::HeapStats;
use esp_hal::uart::Uart;
use {esp_backtrace as _, defmt as _};
use foa_dswifi::pictochat_packets::MessagePayload;
use embedded_storage::{ReadStorage, Storage};
use esp_hal::gpio::Level::Low;
use esp_hal::system::software_reset;
use esp_hal::time::Rate;
use esp_hal::xtensa_lx::timer::delay;
use esp_storage::FlashStorage;
use sdspi::SdSpi;
use edge_nal::TcpBind;

//test network this is fine to be commited
const WIFI_NETWORK: &str = "Inception";
const WIFI_PASSWORD: &str = "l7TlGp6FeDZw7H";

const CONFIG_PART_START: usize = 0x3b0000;
const CONFIG_PART_SIZE: usize = 0x4F000;
const CONFIG_PART_RANGE: Range<usize> = CONFIG_PART_START..CONFIG_PART_START+CONFIG_PART_SIZE;

//const HEAP_SIZE: usize = 48 * 1024;
const HEAP_2_SIZE: usize = 98 * 1000;
fn init_heap() {
    //static mut HEAP: MaybeUninit<[u8; HEAP_SIZE]> = MaybeUninit::uninit();
    #[link_section =".dram2_uninit"]
    static mut HEAP_2: MaybeUninit<[u8; HEAP_2_SIZE]> = MaybeUninit::uninit();

    unsafe {
        /*esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            HEAP.as_mut_ptr() as *mut u8,
            HEAP_SIZE,
            esp_alloc::MemoryCapability::Internal.into(),
        ));*/
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            HEAP_2.as_mut_ptr() as *mut u8,
            HEAP_2_SIZE,
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
        ExclusiveDevice<Spi<'static, Async>, Output<'static>, NoDelay>,
        Input<'static>,
        Output<'static>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn tcp_listen_task(stack: Stack<'static>, tx_channel: DynamicSender<'static, [u8;14]>) {
    let mut buf = vec![0u8; 1_000];
    let mut sock_rx_buffer = vec![0u8; 500];
    let mut sock_tx_buffer= vec![0u8; 500];
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

#[embassy_executor::task]
async fn http_listen_task(stack: Stack<'static>, ekv_db: &'static Database<CachedFlashWrapper,NoopRawMutex>) {
    let mut server = Box::new(DefaultServer::new());
    let box_buffers = Box::new(TcpBuffers::<6,100,100>::new());
    let tcp = edge_nal_embassy::Tcp::new(stack,&box_buffers);
    let tcp_accept = tcp.bind("0.0.0.0:80".parse().unwrap()).await.unwrap();
    let http_handler = HttpHandler {
        ekv_db
    };
    server.run(None, tcp_accept, http_handler).await.expect("?");
}

struct CachedFlashWrapper {
    range: Range<usize>,
    page_cache: AlignedBuf<{ config::PAGE_SIZE }>,
    page_cache_id: Option<u32>,
}
#[repr(C, align(4))]
struct AlignedBuf<const N: usize>([u8; N]);
impl ekv::flash::Flash for CachedFlashWrapper {
    type Error = i32;

    fn page_count(&self) -> usize {
        (self.range.end - self.range.start).div_floor(config::PAGE_SIZE)
    }

    async fn erase(&mut self, page_id: PageID) -> Result<(), Self::Error> {
        let sector = self.range.start.div_floor(config::PAGE_SIZE) + page_id.index();
        if sector * config::PAGE_SIZE >= self.range.end {
            panic!("Attempt to erase out of bounds");
        }
        if let Some(id) = self.page_cache_id {
            if id == sector as u32 {
                self.page_cache_id = None;
            }
        }
        unsafe {
            esp_storage::ll::spiflash_unlock().expect("Failed to unlock flash");

            match esp_storage::ll::spiflash_erase_sector(sector as u32) {
                Ok(_) => {
                    Ok(())
                }
                Err(c) => {
                    warn!("Erase Error {}",c);
                    Err(c)
                }
            }
        }
    }

    async fn read(&mut self, page_id: PageID, offset: usize, data: &mut [u8]) -> Result<(), Self::Error> {
        if offset + data.len() > config::PAGE_SIZE {
            panic!("cant read > 1 page")
        }
        let sector = self.range.start.div_floor(config::PAGE_SIZE) + page_id.index();
        if let Some(id) = self.page_cache_id {
            if id == sector as u32 {
                data.copy_from_slice(&self.page_cache.0[offset..offset+data.len()]);
                return Ok(());
            }
        }
        let address = page_id.index() * config::PAGE_SIZE + self.range.start;
        let mut buf = AlignedBuf([0; config::PAGE_SIZE]);
        unsafe {
            match esp_storage::ll::spiflash_read(address as u32, buf.0.as_mut_ptr() as *mut u32, buf.0.len() as u32) {
                Ok(_) => {
                    data.copy_from_slice(&buf.0[offset..offset+data.len()]);
                    self.page_cache.0.copy_from_slice(&buf.0);
                    self.page_cache_id = Some(sector as u32);
                    Ok(())
                }
                Err(c) => {
                    warn!("Read Error {}",c);
                    Err(c)
                }
            }
        }
    }

    async fn write(&mut self, page_id: PageID, offset: usize, data: &[u8]) -> Result<(), Self::Error> {
        if offset + data.len() > config::PAGE_SIZE {
            panic!("cant write > 1 page")
        }
        let address = page_id.index() * config::PAGE_SIZE + self.range.start;
        let mut buf = AlignedBuf([0; config::PAGE_SIZE]);
        self.read(page_id, 0, &mut buf.0).await.expect("TODO: panic message");
        buf.0[offset..offset+data.len()].copy_from_slice(data);
        if let Some(id) = self.page_cache_id {
            let sector = self.range.start.div_floor(config::PAGE_SIZE) + page_id.index();
            if id == sector as u32 {
                self.page_cache_id = None;
            }
        }
        unsafe {
            match esp_storage::ll::spiflash_write(address as u32, buf.0.as_ptr() as *const u32, buf.0.len() as u32) {
                Ok(_) => {
                    Ok(())
                }
                Err(c) => {
                    warn!("Write Error {}",c);
                    Err(c)
                }
            }
        }
    }
}


async fn read_key(ekv_db: &Database<CachedFlashWrapper, NoopRawMutex>, key_value: &[u8], mut out_vec: &mut Vec<u8>) -> bool {
    let rtx = ekv_db.read_transaction().await;
    match rtx.read(key_value, &mut out_vec).await {
        Ok(key_size) => {
            out_vec.truncate(key_size);
            true
        }
        Err(e) => match e {
            ReadError::KeyNotFound => {
                false
            }
            _ => {
                warn!("Flash is Corrupted");
                format_ekv(&ekv_db).await;
                software_reset();
            }
        },
    }
}

async fn format_ekv(ekv_db: &Database<CachedFlashWrapper, NoopRawMutex>) {
    info!("Formatting EKV ...");
    let start = Instant::now();
    match ekv_db.format().await {
        Ok(_) => {
            let ms = Instant::now().duration_since(start).as_millis();
            info!("Formatting took {} ms!", ms);
        }
        Err(err) => {
            panic!("Failed to Format {:?}", err);
        }
    }
}

struct HttpHandler {
    ekv_db: &'static Database<CachedFlashWrapper,NoopRawMutex>,
}

impl Handler for HttpHandler {
    type Error<E>
    = edge_http::io::Error<E>
    where
        E: Debug;

    async fn handle<T, const N: usize>(
        &self,
        _task_id: impl Display + Copy,
        conn: &mut Connection<'_, T, N>,
    ) -> Result<(), Self::Error<T::Error>>
    where
        T: Read + Write,
    {
        let method = conn.headers()?.method.clone();
        let path: Vec<_> = conn.headers()?.path.clone().split("/").collect();
        if path.len() > 2 {
            match (path[1], path[2]) {
                ("api","reboot") => {
                    if method != Method::Get {
                        conn.initiate_response(405, Some("Method Not Allowed"), &[("Connection","Close")]).await?;
                    }
                    conn.initiate_response(204, Some("No Content"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                    conn.flush().await?;
                    Timer::after_secs(1).await;
                    software_reset();
                },
                ("api","heap") => {
                    if method != Method::Get {
                        conn.initiate_response(405, Some("Method Not Allowed"), &[("Connection","Close")]).await?;
                    }
                    conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                    let stats: HeapStats = esp_alloc::HEAP.stats();
                    conn.write(format!("{}", stats).as_bytes()).await?;
                },
                ("api","config") => {
                    if method == Method::Get {
                        if path.len() > 3 {
                            let mut data = vec![0u8; 2048];
                            let rtx = self.ekv_db.read_transaction().await;
                            if let Ok(read_size) = rtx.read(path[3].as_bytes(),&mut data).await {
                                conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                                data.truncate(read_size);
                                conn.write_all(&data).await?;
                            } else {
                                conn.initiate_response(404, Some("Not Found"), &[("Connection","Close")]).await?;
                            }
                        } else {
                            conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                        }
                    } else if method == Method::Post {
                        if path.len() > 3 {
                            let mut data = vec![0u8; 2048];
                            let body_size = conn.read(&mut data).await?;
                            let mut wtx = self.ekv_db.write_transaction().await;

                            if let Ok(read_size) = wtx.write(path[3].as_bytes(),&data).await {
                                if let Ok(_) = wtx.commit().await {
                                    conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                                    conn.write_all(&data).await?;
                                } else {
                                    conn.initiate_response(500, Some("Internal Server Error"), &[("Connection","Close")]).await?;
                                }
                            } else {
                                conn.initiate_response(500, Some("Internal Server Error"), &[("Connection","Close")]).await?;
                            }
                        } else {
                            conn.initiate_response(404, Some("Not Found"), &[("Connection","Close")]).await?;
                        }
                    } else {
                        conn.initiate_response(405, Some("Method Not Allowed"), &[("Connection","Close")]).await?;

                    }

                }
                ("api", _) => {
                    conn.initiate_response(418, Some("I'm a teapot"), &[("Connection","Close")]).await?;
                }
                (_, _) => {
                    conn.initiate_response(404, Some("Not Found"), &[("Connection","Close")]).await?;
                }
            }
        }
        if conn.headers()?.path == "/" {
            conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/html"),("Connection","Close")]).await?;
            conn.write_all(b"Hello World").await?;
            conn.flush().await?;
        } else {
            conn.initiate_response(404, Some("Not Found"), &[("Connection","Close")]).await?;
        }

        Ok(())
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(_240MHz));

    let mut rng = Rng::new(peripherals.RNG);

    init_heap();

    info!("Hello, world!");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    let mut led = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());
    let mut force_ekv_erase = Input::new(peripherals.GPIO23, InputConfig::default().with_pull(Pull::Up));

    let flash = CachedFlashWrapper {
        range: CONFIG_PART_RANGE,
        page_cache: AlignedBuf([0; config::PAGE_SIZE]),
        page_cache_id: None,
    };

    let mut ekv_config = ekv::Config::default();
    ekv_config.random_seed = rng.random();
    let ekv_db = mk_static!(ekv::Database::<CachedFlashWrapper,NoopRawMutex>,ekv::Database::new(flash,ekv_config));

    let mut should_erase = false;

    if force_ekv_erase.is_low() {
        info!("Manual Format Triggered ...");
        led.set_high();
        should_erase = true;
        for i in 0..13 {
            let was_button_released = force_ekv_erase.wait_for_high().with_timeout(Duration::from_millis(250)).await;
            if was_button_released.is_ok() {
                should_erase = false;
                info!("Manual format aborted");
                break;
            }
            led.toggle();
        }
        led.set_high();
    }

    if should_erase {
        format_ekv(&ekv_db).await;
        led.set_low();
    }

    if !should_erase {
        match ekv_db.mount().await {
            Ok(_) => {}
            Err(_) => {
                warn!("Failed to mount EKV!");
                should_erase = true;
            }
        }
    }

    let mut wifi_ssid = vec![0u8;32];
    let mut wifi_password = vec![0u8;64];
    let mut no_config = false;
    let mut is_open_network = false;

    no_config = !read_key(&ekv_db,b"wifi_ssid", &mut wifi_ssid).await;
    is_open_network = !read_key(&ekv_db,b"wifi_psk", &mut wifi_password).await;

    if no_config {
        //TODO: config ap
        info!("no config present in flash, writing default and restarting");
        let mut wtx = ekv_db.write_transaction().await;
        wtx.write(b"wifi_psk", WIFI_PASSWORD.as_bytes()).await.expect("TODO: panic message");
        wtx.write(b"wifi_ssid", WIFI_NETWORK.as_bytes()).await.expect("TODO: panic message");
        wtx.commit().await.expect("dontfailpls");
        info!("restarting in 10 seconds");
        Timer::after_secs(10).await;
        software_reset();
    }

    //const KEY_COUNT: usize = 128;
    //const TX_SIZE: usize = 1;

    /*info!("Writing {} keys...", KEY_COUNT);
    let start = Instant::now();
    for k in 0..KEY_COUNT / TX_SIZE {
        let mut wtx = ekv_db.write_transaction().await;
        for j in 0..TX_SIZE {
            let i = k * TX_SIZE + j;
            let key = make_key(i);
            let val = make_value(i);

            match wtx.write(&key, &val).await {
                Ok(n) => {
                }
                Err(a) => {
                    error!("Failed to Write {:?}", a);
                }
            }
        }
        wtx.commit().await.unwrap();
    }
    let ms = Instant::now().duration_since(start).as_millis();
    info!("Done in {} ms! {}ms/key", ms, ms / KEY_COUNT as u64);

    info!("Reading {} keys...", KEY_COUNT);
    let mut buf = [0u8; 2048];
    let start = Instant::now();
    for i in 0..KEY_COUNT {
        let key = make_key(i);
        let val = make_value(i);

        let rtx = ekv_db.read_transaction().await;
        match rtx.read(&key, &mut buf).await {
            Ok(n) => {
                assert_eq!(&buf[..n], &val[..]);
            }
            Err(a) => {
                 error!("Failed to Read {:?}", a);
            }
        }

    }
    let ms = Instant::now().duration_since(start).as_millis();
    info!("Done in {} ms! {}ms/key", ms, ms / KEY_COUNT as u64);
    */
    /*
    let spi3_sclk = peripherals.GPIO17;
    let spi3_miso = peripherals.GPIO19;
    let spi3_mosi = peripherals.GPIO22 ;
    let mut spi3_cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());

    //let spi3_dma_ch = peripherals.DMA_SPI2;
    //let (spi3_rx_buffer, spi3_rx_descriptors, spi3_tx_buffer, spi3_tx_descriptors) = dma_buffers!(4000);
    //let spi3_dma_rx_buf = DmaRxBuf::new(spi3_rx_descriptors, spi3_rx_buffer).unwrap();
    //let spi3_dma_tx_buf = DmaTxBuf::new(spi3_tx_descriptors, spi3_tx_buffer).unwrap();

    let mut spi3 = Spi::new(
        peripherals.SPI3,
        Config::default()
            .with_frequency(Rate::from_khz(400))
            .with_mode(Mode::_0),
    ).unwrap()
        .with_sck(spi3_sclk)
        .with_mosi(spi3_mosi)
        .with_miso(spi3_miso)
        //.with_dma(spi3_dma_ch)
        //.with_buffers(spi3_dma_rx_buf, spi3_dma_tx_buf)
        .into_async();
    loop {
        match sdspi::sd_init(&mut spi3, &mut spi3_cs).await {
            Ok(_) => break,
            Err(e) => {
                warn!("Sd init error: {:?}", e);
                embassy_time::Timer::after_millis(10).await;
            }
        }
    }
    let sd_spi = ExclusiveDevice::new(spi3, spi3_cs, embassy_time::Delay).unwrap();
    let mut sd = SdSpi::<_, _, aligned::A1>::new(sd_spi, embassy_time::Delay);
    loop {
        let res = sd.init().await;
        if res.is_ok() {
            info!("SD Initialization complete!");
            sd.spi()
                .bus_mut().apply_config(&Config::default()
                .with_frequency(Rate::from_mhz(20))
                .with_mode(Mode::_0)).expect("Failed to increase bus speed");
            break;
        } else {
            warn!("{:?}",res.expect_err("not possible"));
        }
        info!("Failed to init SD card, retrying...");

        Timer::after_nanos(5000).await;

    }
    let inner = BufStream::<_, 512>::new(sd);
    let fs = embedded_fatfs::FileSystem::new(inner, FsOptions::new()).await.unwrap();

    let mut f = fs.root_dir().create_file("test.log").await.unwrap();
    let hello = b"Hello world!";
    info!("Writing to file...");
    f.write_all(hello).await.unwrap();
    f.flush().await.unwrap();

    let mut buf = [0u8; 12];
    f.rewind().await.unwrap();
    f.read_exact(&mut buf[..]).await.unwrap();
    info!(
        "Read from file: {}",
        core::str::from_utf8(&buf[..]).unwrap()
    );
    f.close().await.unwrap();

     {
            let mut f = fs.root_dir().create_file("iotest.bin").await.unwrap();
            let mut write_size: usize = 0;
            let write_buf = vec![0u8;32768];
            let start = Instant::now();
            while write_size <= 1_000_000 {
                f.write_all(&write_buf).await.unwrap();
                write_size = write_size + write_buf.len();
            }
            f.flush().await.unwrap();
            let current = Instant::now();
            let millis = (current-start).as_millis();
            info!("Write {} Bytes in {}",write_size,millis);
            let bps = write_size as f32 / (millis as f32 / 1000f32);
            info!("Write BPS: {}",bps);
     }
     {
            let mut f = fs.root_dir().open_file("iotest.bin").await.unwrap();
            let mut read_size: usize = 0;
            let mut read_buf = vec![0u8;32768];
            let start = Instant::now();
            loop {
                let cur_read = f.read(&mut read_buf).await.unwrap();
                read_size += cur_read;
                if cur_read == 0 {
                    break;
                }
            }
            //f.flush().await.unwrap();
            let current = Instant::now();
            let millis = (current-start).as_millis();
            info!("Read {} Bytes in {}",read_size,millis);
            let bps = read_size as f32 / (millis as f32 / 1000f32);
            info!("Read BPS: {}",bps);
    }
    */

    let sck = peripherals.GPIO14;
    let miso = peripherals.GPIO12;
    let mosi = peripherals.GPIO13;
    let cs = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());

    let esph_handshake = Input::new(peripherals.GPIO26, InputConfig::default().with_pull(Pull::Up));
    let esph_ready = Input::new(peripherals.GPIO25, InputConfig::default().with_pull(Pull::None));
    let esph_reset = Output::new(peripherals.GPIO33, Level::Low, OutputConfig::default());

    let dma_channel = peripherals.DMA_SPI3;

    //let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(4000);
    //let dma_rx_buf = DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    //let dma_tx_buf = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

    let spi = Spi::new(
        peripherals.SPI2,
        Config::default()
            .with_frequency(Rate::from_mhz(20))
            .with_mode(Mode::_1),
        ).unwrap()
        .with_sck(sck)
        .with_mosi(mosi)
        .with_miso(miso)
        //.with_dma(dma_channel)
        //.with_buffers(dma_rx_buf, dma_tx_buf)
        .into_async();

    let esph_spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

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
    control.connect(&String::from_utf8(wifi_ssid).unwrap(), &String::from_utf8(wifi_password).unwrap()).await.unwrap();

    let net_stack_resources = mk_static!(NetStackResources<6>, NetStackResources::new());
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

    spawner.spawn(http_listen_task(net_stack,ekv_db)).expect("TODO: panic message");

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
    let mut ticker = Ticker::every(Duration::from_secs(15));
    loop {
        match select4(pictochat_interface.inbound_queue.receive(),pictochat_interface.event_queue.receive(),channel_rx.receive(),ticker.next()).await {
            Either4::First(message) => {
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
            Either4::Second(event) => {
                match event {
                    PictochatInterfaceEvent::ClientConnected(id) => {
                        info!("Client Joined {:?}", id.name)
                    }
                    PictochatInterfaceEvent::ClientDisconnected(id) => {
                        info!("Client Left {:?}", id.name)
                    }           
                }
            },
            Either4::Third(data) => {
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
            },
            Either4::Fourth(_) => {
                let stats: HeapStats = esp_alloc::HEAP.stats();
                println!("{}", stats);
            }

        }
    }

}
