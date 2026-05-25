use core::cell::RefCell;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;
use defmt::{info, warn, Format};
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either::*};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pubsub::{PubSubChannel, Subscriber};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::{ExclusiveDevice, RefCellDevice};
use embedded_io_async::{Read, Seek, SeekFrom, Write};
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::peripherals::Peripherals;
use esp_hal::spi::master::{Config, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_hal::dma_buffers;
use aligned::A1;
use block_device_adapters::{BufStream, StreamSlice};
use embedded_fatfs::{FileSystem, FsOptions};
use mbrs::Mbr;
use sdspi::SdSpi;

use crate::util::TaskMailbox;

static MAILBOX: TaskMailbox<CriticalSectionRawMutex, SdCardCommand, Result<SdCardResponse, SdCardError>, 4> =
    TaskMailbox::new();

static EVENTS: PubSubChannel<CriticalSectionRawMutex, SdCardEvent, 4, 4, 1> = PubSubChannel::new();

pub type SdCardEventSubscriber = Subscriber<'static, CriticalSectionRawMutex, SdCardEvent, 4, 4, 1>;

#[derive(Clone)]
pub enum SdCardEvent {
    CardInserted,
    CardRemoved,
}

#[derive(Debug, Format)]
pub enum SdCardError {
    CardNotPresent,
    MountFailed,
    FileNotFound,
    IoError,
}

pub enum FileStat {
    File { size: u64 },
    Directory,
}

pub enum SdCardCommand {
    ReadFile(String),
    ReadFileChunk(String, u64, usize),
    WriteFile(String, Vec<u8>),
    AppendFile(String, Vec<u8>),
    DeleteFile(String),
    ListFiles(String),
    Stat(String),
}

pub enum SdCardResponse {
    Data(Vec<u8>),
    Entries(Vec<String>),
    Stat(FileStat),
    Done,
}

pub struct SdCardController;

impl SdCardController {
    pub fn init(spawner: &Spawner) {
        MAILBOX.init();
        spawner.spawn(sdcard_task()).unwrap();
    }

    pub async fn read_file(path: String) -> Result<Vec<u8>, SdCardError> {
        match MAILBOX.request(SdCardCommand::ReadFile(path)).await {
            Ok(SdCardResponse::Data(d)) => Ok(d),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn write_file(path: String, data: Vec<u8>) -> Result<(), SdCardError> {
        match MAILBOX.request(SdCardCommand::WriteFile(path, data)).await {
            Ok(SdCardResponse::Done) => Ok(()),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn append_file(path: String, data: Vec<u8>) -> Result<(), SdCardError> {
        match MAILBOX.request(SdCardCommand::AppendFile(path, data)).await {
            Ok(SdCardResponse::Done) => Ok(()),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_file(path: String) -> Result<(), SdCardError> {
        match MAILBOX.request(SdCardCommand::DeleteFile(path)).await {
            Ok(SdCardResponse::Done) => Ok(()),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn list_files(path: String) -> Result<Vec<String>, SdCardError> {
        match MAILBOX.request(SdCardCommand::ListFiles(path)).await {
            Ok(SdCardResponse::Entries(e)) => Ok(e),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn read_file_chunk(path: String, offset: u64, chunk_size: usize) -> Result<Vec<u8>, SdCardError> {
        match MAILBOX.request(SdCardCommand::ReadFileChunk(path, offset, chunk_size)).await {
            Ok(SdCardResponse::Data(d)) => Ok(d),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub async fn stat(path: String) -> Result<FileStat, SdCardError> {
        match MAILBOX.request(SdCardCommand::Stat(path)).await {
            Ok(SdCardResponse::Stat(s)) => Ok(s),
            Ok(_) => Err(SdCardError::IoError),
            Err(e) => Err(e),
        }
    }

    pub fn read_file_chunked(path: String, chunk_size: usize) -> ChunkedFileReader {
        ChunkedFileReader { path, chunk_size, offset: 0, done: false }
    }

    pub fn subscribe() -> Option<SdCardEventSubscriber> {
        EVENTS.subscriber().ok()
    }
}

pub struct ChunkedFileReader {
    path: String,
    chunk_size: usize,
    offset: u64,
    done: bool,
}

impl ChunkedFileReader {
    pub async fn next(&mut self) -> Option<Result<Vec<u8>, SdCardError>> {
        if self.done {
            return None;
        }
        match SdCardController::read_file_chunk(self.path.clone(), self.offset, self.chunk_size).await {
            Ok(data) if data.is_empty() => {
                self.done = true;
                None
            }
            Ok(data) => {
                self.offset += data.len() as u64;
                if data.len() < self.chunk_size {
                    self.done = true;
                }
                Some(Ok(data))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[embassy_executor::task]
async fn sdcard_task() {
    let publisher = EVENTS.publisher().unwrap();
    info!("sdcard bootup");

    loop {
        let peripherals = unsafe { Peripherals::steal() };


        let sd_sclk = peripherals.GPIO25;
        let sd_miso = peripherals.GPIO22;
        let sd_mosi = peripherals.GPIO21;
        let mut sd_det = Input::new(peripherals.GPIO35, InputConfig::default());

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
        )
            .unwrap()
            .with_sck(sd_sclk)
            .with_mosi(sd_mosi)
            .with_miso(sd_miso)
            .with_dma(spi2_dma_ch)
            .with_buffers(spi2_dma_rx_buf, spi2_dma_tx_buf)
            .into_async();

        // Drain mailbox commands with CardNotPresent until a card is detected
        loop {
            match select(MAILBOX.receive(), sd_det.wait_for_low()).await {
                First((_, reply)) => reply.send(Err(SdCardError::CardNotPresent)).await,
                Second(_) => break,
            }
        }

        info!("SD card detected, initializing");

        // CS pin is recreated each card cycle; GPIO4 peripheral is cheap to re-steal
        let mut sd_cs = Output::new(
            unsafe { Peripherals::steal().GPIO4 },
            Level::High,
            OutputConfig::default(),
        );

        // Low-level init requires raw bus access; borrow the RefCell directly
        loop {
            match sdspi::sd_init(&mut sd_spi2, &mut sd_cs).await {
                Ok(_) => break,
                Err(_) => {
                    Timer::after_millis(500).await;
                }
            }
        }

        let sd_dev = ExclusiveDevice::new(&mut sd_spi2, sd_cs, Delay).unwrap();
        let mut sd = SdSpi::<_, _, A1>::new(sd_dev, Delay);

        loop {
            if sd.init().await.is_ok() {
                sd.spi().bus_mut()
                    .apply_config(
                        &Config::default()
                            .with_frequency(Rate::from_mhz(12))
                            .with_mode(Mode::_0),
                    )
                    .expect("Failed to increase bus speed");
                break;
            }
            info!("Failed to init SD card, retrying...");
            Timer::after_nanos(5000).await;
        }

        info!("SD card initialized, mounting filesystem");

        let mut sd_stream = BufStream::<_, { 512 }>::new(sd);
        let mut mbr_block = [0u8; 512];

        if sd_stream.seek(SeekFrom::Start(0)).await.is_err() {
            warn!("Failed to seek to MBR");
            continue;
        }
        if sd_stream.read_exact(&mut mbr_block).await.is_err() {
            warn!("Failed to read MBR");
            continue;
        }

        let mut mbr = match Mbr::try_from_bytes(&mbr_block) {
            Ok(m) => m,
            Err(_) => { warn!("Invalid MBR"); continue; }
        };

        let mut part_location = None;
        for part_option in mbr.partition_table.entries.iter_mut() {
            if let Some(part) = part_option {
                part_location = Some((
                    part.start_sector_lba() as u64 * 512u64,
                    part.end_sector_lba() as u64 * 512u64,
                ));
            }
        }

        let (part_start, part_end) = match part_location {
            Some(loc) => loc,
            None => { warn!("No partition found in MBR"); continue; }
        };

        let partition_stream = match StreamSlice::new(sd_stream, part_start, part_end).await {
            Ok(s) => s,
            Err(_) => { warn!("Failed to create partition stream"); continue; }
        };

        let sdcard_fs = match FileSystem::new(partition_stream, FsOptions::new()).await {
            Ok(fs) => fs,
            Err(_) => { warn!("Failed to mount filesystem"); continue; }
        };

        info!("SD card filesystem mounted");
        publisher.publish_immediate(SdCardEvent::CardInserted);

        loop {
            match select(MAILBOX.receive(), sd_det.wait_for_high()).await {
                Second(_) => {
                    sdcard_fs.unmount().await.ok();
                    publisher.publish_immediate(SdCardEvent::CardRemoved);
                    break;
                }
                First((cmd, reply)) => match cmd {
                    SdCardCommand::ReadFileChunk(path, offset, size) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            let mut f = sdcard_fs
                                .root_dir()
                                .open_file(&path)
                                .await
                                .map_err(|_| SdCardError::FileNotFound)?;
                            f.seek(SeekFrom::Start(offset))
                                .await
                                .map_err(|_| SdCardError::IoError)?;
                            let mut buf = vec![0u8; size];
                            let mut total = 0;
                            loop {
                                if total == size { break; }
                                match f.read(&mut buf[total..]).await {
                                    Ok(0) => break,
                                    Ok(n) => total += n,
                                    Err(_) => {
                                        f.close().await.ok();
                                        return Err(SdCardError::IoError);
                                    }
                                }
                            }
                            f.close().await.ok();
                            buf.truncate(total);
                            Ok(SdCardResponse::Data(buf))
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::ReadFile(path) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            let mut f = sdcard_fs
                                .root_dir()
                                .open_file(&path)
                                .await
                                .map_err(|_| SdCardError::FileNotFound)?;
                            let mut data: Vec<u8> = Vec::new();
                            let mut buf = vec![0u8; 512];
                            loop {
                                match f.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => data.extend_from_slice(&buf[..n]),
                                    Err(_) => {
                                        f.close().await.ok();
                                        return Err(SdCardError::IoError);
                                    }
                                }
                            }
                            f.close().await.ok();
                            Ok(SdCardResponse::Data(data))
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::WriteFile(path, data) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            sdcard_fs.root_dir().remove(&path).await.ok();
                            let mut f = sdcard_fs
                                .root_dir()
                                .create_file(&path)
                                .await
                                .map_err(|_| SdCardError::IoError)?;
                            f.write_all(&data).await.map_err(|_| SdCardError::IoError)?;
                            f.flush().await.map_err(|_| SdCardError::IoError)?;
                            f.close().await.ok();
                            Ok(SdCardResponse::Done)
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::AppendFile(path, data) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            let mut f = sdcard_fs
                                .root_dir()
                                .create_file(&path)
                                .await
                                .map_err(|_| SdCardError::IoError)?;
                            f.seek(SeekFrom::End(0))
                                .await
                                .map_err(|_| SdCardError::IoError)?;
                            f.write_all(&data).await.map_err(|_| SdCardError::IoError)?;
                            f.flush().await.map_err(|_| SdCardError::IoError)?;
                            f.close().await.ok();
                            Ok(SdCardResponse::Done)
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::DeleteFile(path) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            sdcard_fs
                                .root_dir()
                                .remove(&path)
                                .await
                                .map_err(|_| SdCardError::FileNotFound)?;
                            Ok(SdCardResponse::Done)
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::ListFiles(path) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            let mut entries: Vec<String> = Vec::new();
                            let dir = if path.is_empty() || path == "/" {
                                sdcard_fs.root_dir()
                            } else {
                                sdcard_fs
                                    .root_dir()
                                    .open_dir(path.trim_start_matches('/'))
                                    .await
                                    .map_err(|_| SdCardError::FileNotFound)?
                            };
                            let mut iter = dir.iter();
                            while let Some(entry_result) = iter.next().await {
                                match entry_result {
                                    Ok(entry) => entries.push(entry.file_name()),
                                    Err(_) => return Err(SdCardError::IoError),
                                }
                            }
                            Ok(SdCardResponse::Entries(entries))
                        }
                        .await;
                        reply.send(result).await;
                    }
                    SdCardCommand::Stat(path) => {
                        let result: Result<SdCardResponse, SdCardError> = async {
                            let stripped = path.trim_start_matches('/');

                            // root folder is a folder
                            if stripped.is_empty() {
                                return Ok(SdCardResponse::Stat(FileStat::Directory));
                            }

                            if let Ok(mut f) = sdcard_fs.root_dir().open_file(stripped).await {
                                let size = f.seek(SeekFrom::End(0)).await
                                    .map_err(|_| SdCardError::IoError)?;
                                f.close().await.ok();
                                Ok(SdCardResponse::Stat(FileStat::File { size }))
                            } else if sdcard_fs.root_dir().open_dir(stripped).await.is_ok() {
                                Ok(SdCardResponse::Stat(FileStat::Directory))
                            } else {
                                Err(SdCardError::FileNotFound)
                            }
                        }
                        .await;
                        reply.send(result).await;
                    }
                },
            }
        }
    }
}
