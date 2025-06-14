use crate::MaybeUninit;
use alloc::vec::Vec;
use ekv::{config, Database, FormatError, MountError, ReadError, ReadTransaction, WriteTransaction};
use ekv::flash::PageID;
use core::ops::Range;
use defmt::{info, warn};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::Instant;
use esp_hal::system::software_reset;
use crate::mk_static_dram2;

const CONFIG_PART_START: usize = 0x3b0000;
const CONFIG_PART_SIZE: usize = 0x4F000;
const CONFIG_PART_RANGE: Range<usize> = CONFIG_PART_START..CONFIG_PART_START+CONFIG_PART_SIZE;

pub struct CachedFlashWrapper {
    range: Range<usize>,
    io_buffer: AlignedBuf<{ config::PAGE_SIZE }>,
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
        let address = page_id.index() * config::PAGE_SIZE + self.range.start;
        unsafe {
            esp_storage::ll::spiflash_unlock().expect("Failed to unlock flash");
            match esp_storage::ll::spiflash_read(address as u32, self.io_buffer.0.as_mut_ptr() as *mut u32, self.io_buffer.0.len() as u32) {
                Ok(_) => {
                    data.copy_from_slice(&self.io_buffer.0[offset..offset+data.len()]);
                    //info!("read page {}, {}, {}",sector,address, self.io_buffer.0);
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
        unsafe {
            esp_storage::ll::spiflash_unlock().expect("Failed to unlock flash");
            match esp_storage::ll::spiflash_read(address as u32, self.io_buffer.0.as_mut_ptr() as *mut u32, self.io_buffer.0.len() as u32) {
                Ok(_) => {}
                Err(c) => {
                    warn!("Read Error {}",c);
                    return Err(c);
                }
            }
            self.io_buffer.0[offset..offset+data.len()].copy_from_slice(data);
            match esp_storage::ll::spiflash_write(address as u32, self.io_buffer.0.as_ptr() as *const u32, self.io_buffer.0.len() as u32) {
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


pub struct InternalFlash {
    inner: Database<CachedFlashWrapper, NoopRawMutex>,
}

impl InternalFlash {
    pub fn new(random_seed: u32) -> Self {
        let flash = CachedFlashWrapper {
            range: CONFIG_PART_RANGE,
            io_buffer: AlignedBuf([0; config::PAGE_SIZE])
        };
        let mut ekv_config = ekv::Config::default();
        ekv_config.random_seed = random_seed;
        let ekv_db = ekv::Database::new(flash,ekv_config);

        Self {
            inner: ekv_db
        }
    }
    pub async fn read_key(&self, key_value: &[u8], mut out_vec: &mut Vec<u8>) -> bool {
        let rtx = self.inner.read_transaction().await;
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
                    self.format_ekv().await;
                    software_reset();
                }
            },
        }
    }

    pub async fn format_ekv(&self) {
        info!("Formatting EKV ...");
        let start = Instant::now();
        match self.inner.format().await {
            Ok(_) => {
                let ms = Instant::now().duration_since(start).as_millis();
                info!("Formatting took {} ms!", ms);
            }
            Err(err) => {
                panic!("Failed to Format {:?}", err);
            }
        }
    }
    
    pub async fn mount(&self) -> bool {
        match self.inner.mount().await {
            Ok(_) => {true}
            Err(_) => {
                warn!("Failed to mount EKV!");
                false
            }
        }
    }
    
    pub async fn read_transaction(&self) -> ReadTransaction<'_, CachedFlashWrapper, NoopRawMutex> {
        self.inner.read_transaction().await
    }
    pub async fn write_transaction(&self) -> WriteTransaction<'_, CachedFlashWrapper, NoopRawMutex> {
        self.inner.write_transaction().await
    }
}
