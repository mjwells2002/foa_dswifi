use esp_hal::psram::psram_raw_parts;
use esp_hal::peripherals::Peripherals;
use alloc::string::{String, ToString};
use alloc::{format, vec};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::join;
use core::intrinsics::black_box;
use core::mem::MaybeUninit;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::ptr::{addr_eq, addr_of_mut};
use core::str::FromStr;
use core::task::Poll;
use aligned::A1;
use block_device_adapters::{BufStream, StreamSlice};
use defmt::{expect, info, warn};
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal_embassy::UdpBuffers;
use embassy_executor::{SendSpawner, Spawner};
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_futures::yield_now;
use embassy_net::{IpEndpoint, Ipv4Cidr, Stack, StaticConfigV4};
use embassy_sync::channel::{Channel, DynamicReceiver, DynamicSender, Receiver, Sender};
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer, WithTimeout};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use esp_hal::{dma_buffers, i2c, rng::Rng, timer::timg::TimerGroup, Async};
use esp_hal::clock::CpuClock::_240MHz;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{DriveStrength, Input, InputConfig, Level, Output, OutputConfig, Pull};
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
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embedded_io_async::{Read, Seek, Write};
use {esp_backtrace as _, defmt as _};
use foa_dswifi::pictochat_packets::MessagePayload;
use esp_hal::time::Rate;
use edge_nal::{UdpBind};
use edge_nal::io::SeekFrom::Start;
use embassy_net::udp::PacketMetadata;
use embassy_net_esp_hosted::{Control, Security};
use esp_hal::i2c::master::I2c;
// use esp_hal::psram::psram_raw_parts;
use crate::display::DisplayUpdate;
use crate::internal_flash::InternalFlash;
use embassy_net_esp_hosted::ApStatus;
use embassy_net_esp_hosted::Bandwidth::Ht20;
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::priority_channel::PriorityChannel;
use embassy_sync::signal::Signal;
use embedded_fatfs::{FileSystem, FsOptions};
use embedded_sdmmc::{SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_bootloader_esp_idf::ota::Slot;
use esp_bootloader_esp_idf::partitions::DataPartitionSubType;
use esp_hal::gpio::interconnect::{PeripheralInput, PeripheralOutput};
use esp_hal::system::{software_reset, CpuControl};
use esp_hal::uart::Uart;
use esp_hal_embassy::{Executor, InterruptExecutor};
use mbrs::Mbr;
use sdspi::SdSpi;
use static_cell::{make_static, StaticCell};
use crate::http_server::http_listen_task;
use crate::util::get_file;

struct IoctlParams {
    file_handle: Option<u16>,

}

enum IoctlCmd {
    Open(IoctlParams),
    Close(IoctlParams),
    ReadBlock(IoctlParams),
    ReadStream(IoctlParams),
    WriteBlock(IoctlParams),
    WriteStream(IoctlParams),
}

// struct SDCardManager {
//     incoming_ioctls: PriorityChannel<>
// }


#[embassy_executor::task]
pub async fn sdcard_task() {
    let peripherals = unsafe { Peripherals::steal() };
    info!("sdcard bootup");

    let sd_sclk = peripherals.GPIO25;
    let sd_miso = peripherals.GPIO22;
    let sd_mosi = peripherals.GPIO21 ;
    let mut sd_cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());
    let mut sd_det = Input::new(peripherals.GPIO35, InputConfig::default()); //external pull up

    //

    let spi2_dma_ch = peripherals.DMA_SPI2;

    let (spi2_rx_buffer, spi2_rx_descriptors, spi2_tx_buffer, spi2_tx_descriptors) = dma_buffers!(4000);
    let spi2_dma_rx_buf = DmaRxBuf::new(spi2_rx_descriptors, spi2_rx_buffer).unwrap();
    let spi2_dma_tx_buf = DmaTxBuf::new(spi2_tx_descriptors, spi2_tx_buffer).unwrap();

    Timer::after_micros(182).await;

    let mut sd_spi2 = Spi::new(
        peripherals.SPI2,
        Config::default()
            .with_frequency(Rate::from_khz(100))
            .with_mode(Mode::_0),
    ).unwrap()
        .with_sck(sd_sclk)
        .with_mosi(sd_mosi)
        .with_miso(sd_miso)
        .with_dma(spi2_dma_ch)
        .with_buffers(spi2_dma_rx_buf, spi2_dma_tx_buf)
        .into_async();

    loop {
        sd_det.wait_for_low().await;
        info!("SD_DET low, checking for card");
        match sdspi::sd_init(&mut sd_spi2, &mut sd_cs).await {
            Ok(_) => break,
            Err(e) => {
                embassy_time::Timer::after_millis(500).await;
            }
        }
    }

    info!("SD Card detected, initializing!");

    let sd_spi = ExclusiveDevice::new(sd_spi2, sd_cs, embassy_time::Delay).unwrap();
    let mut sd = SdSpi::<_, _, aligned::A1>::new(sd_spi, embassy_time::Delay);
    loop {
        let res = sd.init().await;
        if res.is_ok() {
            //info!("SD Initialization complete!");
            sd.spi()
                .bus_mut().apply_config(&Config::default()
                .with_frequency(Rate::from_mhz(12))
                .with_mode(Mode::_0)).expect("Failed to increase bus speed");
            break;
        } else {
            //warn!("{:?}",res.expect_err("not possible"));
        }
        info!("Failed to init SD card, retrying...");

        Timer::after_nanos(5000).await;
    }

    info!("SD Card initialized!, about to mount filesystem");

    let mut sd_stream = BufStream::<_, { 512 }>::new(sd);



    // let spi2_dma_ch = peripherals.DMA_SPI2;
    //
    // let (spi2_rx_buffer, spi2_rx_descriptors, spi2_tx_buffer, spi2_tx_descriptors) = dma_buffers!(4000);
    // let spi2_dma_rx_buf = DmaRxBuf::new(spi2_rx_descriptors, spi2_rx_buffer).unwrap();
    // let spi2_dma_tx_buf = DmaTxBuf::new(spi2_tx_descriptors, spi2_tx_buffer).unwrap();
    //
    // //Timer::after_micros(182).await;
    //
    // let mut sd_spi2 = Spi::new(
    //     peripherals.SPI2,
    //     Config::default()
    //         .with_frequency(Rate::from_khz(400))
    //         .with_mode(Mode::_0),
    // ).unwrap()
    //     .with_sck(sd_sclk)
    //     .with_mosi(sd_mosi)
    //     .with_miso(sd_miso)
    //     .with_dma(spi2_dma_ch)
    //     .with_buffers(spi2_dma_rx_buf, spi2_dma_tx_buf)
    //     .into_async();
    //
    // let sd_spi_device = ExclusiveDevice::new(sd_spi2, sd_cs, Delay).unwrap();
    //
    // let sdspi = sdspi::SdSpi::new(sd_spi_device, Delay);
    //
    //let sdcard = SdCard::new(sd_spi_device,Delay);

    //let time_source = DemoTimeSource {} ;
    //info!("SdCard: {}", sdcard.num_bytes().unwrap());
    // let vm = VolumeManager::new(sdcard,time_source);
    // let v = vm.open_volume(VolumeIdx(0)).unwrap();
    // let root_dir = v.open_root_dir().unwrap();
    //
    // vm.device(|x| {
    //     x.spi(|spi| {
    //         let config = Config::default().with_frequency(Rate::from_mhz(20));
    //        spi.bus_mut().apply_config(&config).expect("TODO: panic message");
    //     });
    //     DemoTimeSource { }
    // });
    // loop {
    //     info!("SdCard: {}", sdcard.num_bytes().unwrap());
    //     embassy_time::Timer::after_millis(5000).await;
    // }


    // loop {
    //     let mut f = root_dir.open_file_in_dir("iotest.bin", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate).unwrap();
    //     let mut write_size: usize = 0;
    //     let write_buf = vec![0u8;32768];
    //     let start = Instant::now();
    //     while write_size <= write_buf.len() * 64 {
    //         f.write(&write_buf).unwrap();
    //         write_size = write_size + write_buf.len();
    //     }
    //     f.flush().unwrap();
    //     let current = Instant::now();
    //     let millis = (current-start).as_millis();
    //     info!("Write {} Bytes in {}",write_size,millis);
    //     let bps = write_size as f32 / (millis as f32 / 1000f32);
    //     info!("Write BPS: {}",bps);
    // }

    // let (spi2_rx_buffer, spi2_rx_descriptors, spi2_tx_buffer, spi2_tx_descriptors) = dma_buffers!(4000);
    // let spi2_dma_rx_buf = DmaRxBuf::new(spi2_rx_descriptors, spi2_rx_buffer).unwrap();
    // let spi2_dma_tx_buf = DmaTxBuf::new(spi2_tx_descriptors, spi2_tx_buffer).unwrap();
    //
    // Timer::after_micros(182).await;
    //
    // let mut sd_spi2 = Spi::new(
    //     peripherals.SPI2,
    //     Config::default()
    //         .with_frequency(Rate::from_khz(400))
    //         .with_mode(Mode::_0),
    // ).unwrap()
    //     .with_sck(sd_sclk)
    //     .with_mosi(sd_mosi)
    //     .with_miso(sd_miso)
    //     .with_dma(spi2_dma_ch)
    //     .with_buffers(spi2_dma_rx_buf, spi2_dma_tx_buf)
    //     .into_async();
    //
    // loop {
    //     match sdspi::sd_init(&mut sd_spi2, &mut sd_cs).await {
    //         Ok(_) => break,
    //         Err(e) => {
    //             embassy_time::Timer::after_millis(500).await;
    //         }
    //     }
    // }
    //
    // info!("SD Card detected, initializing!");
    //
    // let sd_spi = ExclusiveDevice::new(sd_spi2, sd_cs, embassy_time::Delay).unwrap();
    // let mut sd = SdSpi::<_, _, aligned::A1>::new(sd_spi, embassy_time::Delay);
    // loop {
    //     let res = sd.init().await;
    //     if res.is_ok() {
    //         //info!("SD Initialization complete!");
    //         // sd.spi()
    //         //     .bus_mut().apply_config(&Config::default()
    //         //     .with_frequency(Rate::from_mhz(1))
    //         //     .with_mode(Mode::_0)).expect("Failed to increase bus speed");
    //         break;
    //     } else {
    //         //warn!("{:?}",res.expect_err("not possible"));
    //     }
    //     info!("Failed to init SD card, retrying...");
    //
    //     Timer::after_nanos(5000).await;
    // }
    //
    // info!("SD Card initialized!, about to mount filesystem");
    //
    // let mut sd_stream = BufStream::<_, { 512 }>::new(sd);
    //
    //
    let mut mbr_block = [0u8; 512];

    sd_stream.seek(Start(0)).await.expect("Failed to seek to start of SD card");
    sd_stream.read_exact(&mut mbr_block).await.unwrap();
    let mut mbr = Mbr::try_from_bytes(&mbr_block).unwrap();
    let mut part_location = None;
    for part_option in mbr.partition_table.entries.iter_mut() {
        if let Some(part) = part_option {
            part_location = Some((part.start_sector_lba() as u64 * 512u64,part.end_sector_lba() as u64 * 512u64));
        }
    }
    let (part_start, part_end) = part_location.expect("No SD partition found");
    let partition_stream = StreamSlice::new(sd_stream,part_start,part_end).await.unwrap();
    let sdcard_fs = FileSystem::new(partition_stream, FsOptions::new()).await.unwrap();

    let mut f = sdcard_fs.root_dir().create_file("test.log").await.unwrap();
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

    loop {
        {
            let mut f = sdcard_fs.root_dir().create_file("iotest.bin").await.unwrap();
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
            let mut f = sdcard_fs.root_dir().open_file("iotest.bin").await.unwrap();
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
    }
}