
use core::slice::from_raw_parts;
use alloc::string::{String, ToString};
use alloc::{format, vec};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::cmp::min;
use core::ffi::c_void;
use core::fmt::{Debug, Display};
use core::mem::{transmute, MaybeUninit};
use core::{default, mem, slice};
use core::future::Future;
use core::hint::black_box;
use core::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use core::ptr::addr_of_mut;
use core::str::FromStr;
use aligned::A1;
use block_device_adapters::{BufStream, BufStreamError};
use defmt::{debug, error, info, warn};
use edge_dhcp::server::{Server, ServerOptions};
use edge_http::io::server::{Connection, DefaultServer, Handler};
use edge_http::Method;
use edge_http::ws::{MAX_BASE64_KEY_LEN, MAX_BASE64_KEY_RESPONSE_LEN, NONCE_LEN};
use edge_nal_embassy::{Tcp, TcpAccept, TcpBuffers, TcpSocket, UdpBuffers, UdpSocket};
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, select4, Either, Either3, Either4, Select};
use embassy_futures::yield_now;
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_sync::channel::{Channel, DynamicReceiveFuture, DynamicReceiver, DynamicSender, TrySendError};
use embassy_sync::mutex::Mutex;
use embassy_time::{Delay, Duration, Instant, Ticker, Timer, WithTimeout};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use esp_hal::{dma_buffers, dma_descriptors, i2c, ram, rng::Rng, timer::timg::TimerGroup, Async};
use esp_hal::clock::CpuClock::{_240MHz, _80MHz};
use esp_hal::dma::{DmaPriority, DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{SDHOST, SPI2, SPI3};
use esp_hal::spi::master::{Config, Spi, SpiDma, SpiDmaBus};
use esp_hal::spi::Mode;
use esp_println::println;
use foa::FoARunner;
use foa::{FoAResources, VirtualInterface};
use ieee80211::mac_parser::MACAddress;
use static_cell::StaticCell;
use foa_dswifi::{DsWiFiInitInfo, DsWiFiInterface, DsWiFiInterfaceControlEvent, DsWiFiInterfaceControlEventResponse, DsWiFiSharedResources, DsWifiClientMaskMath};
use foa_dswifi::pictochat_application::{PictoChatApplication, PictoChatUserManager, PictochatInterface, PictochatInterfaceEvent, PictochatSharedData};
use foa_dswifi::runner::DsWiFiRunner;
use static_cell::make_static;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
    DhcpConfig, Runner as NetRunner, StackResources as NetStackResources,
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_fatfs::{FsOptions};
use embedded_io_async::{ErrorType, Read, Seek, SeekFrom, Write};
use embedded_sdmmc::{Block, BlockDevice, BlockIdx, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use embedded_sdmmc::sdcard::Error;
use esp_alloc::HeapStats;
use esp_hal::uart::{Parity, Uart};
use {esp_backtrace as _, defmt as _};
use foa_dswifi::pictochat_packets::MessagePayload;
use embedded_storage::{ReadStorage, Storage};
use esp_hal::gpio::Level::Low;
use esp_hal::system::{software_reset, CpuControl};
use esp_hal::time::Rate;
use esp_hal::xtensa_lx::timer::delay;
use esp_storage::FlashStorage;
use edge_nal::{TcpBind, UdpBind};
use edge_nal::io::ReadExactError;
use ekv::Database;
use embassy_net::udp::PacketMetadata;
use embassy_net_esp_hosted::{Control, Security};
use embedded_graphics::Drawable;
use embedded_graphics::geometry::{Point, Size};
use embedded_graphics::mono_font::ascii::{FONT_4X6, FONT_6X10, FONT_6X13};
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::primitives::{Primitive, PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use esp_hal::config::WatchdogConfig;
use esp_hal::i2c::master::I2c;
use esp_hal::psram::psram_raw_parts;
use esp_hal_embassy::Executor;
use ssd1306::{I2CDisplayInterface, Ssd1306};
use ssd1306::mode::DisplayConfig;
use ssd1306::prelude::Brightness;
use ssd1306::rotation::DisplayRotation;
use ssd1306::size::DisplaySize128x64;
use crate::display::DisplayUpdate;
use crate::internal_flash::{CachedFlashWrapper, InternalFlash};
use embassy_net_esp_hosted::ApStatus;
use embassy_net_esp_hosted::Bandwidth::{Ht20, Ht40};
use esp_bootloader_esp_idf::ota::Slot;
use esp_bootloader_esp_idf::partitions::{DataPartitionSubType, PartitionEntry};
use crate::util::get_file;

fn guess_mime_type(filename: &str) -> &'static str {
    match filename.rsplit('.').next() {
        Some(ext) => match ext.to_lowercase().as_str() {
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" => "application/javascript",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "txt" => "text/plain",
            "wasm" => "application/wasm",
            "pdf" => "application/pdf",
            "bin" => "application/octet-stream",
            "mp4" => "video/mp4",
            "m4v" => "video/mp4",
            "webm" => "video/webm",
            "ogv" => "video/ogg",
            "avi" => "video/x-msvideo",
            "mov" => "video/quicktime",
            "wmv" => "video/x-ms-wmv",
            "flv" => "video/x-flv",
            "mkv" => "video/x-matroska",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "ogg" => "audio/ogg",
            "oga" => "audio/ogg",
            "m4a" => "audio/mp4",
            "flac" => "audio/flac",
            "aac" => "audio/aac",
            "opus" => "audio/opus",
            "weba" => "audio/webm",
            _ => "application/octet-stream",
        },
        None => "application/octet-stream",
    }
}

pub fn url_decode(input: String) -> String {
    let mut output = String::with_capacity(input.len()); // worst case: same size

    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if hi < 16 && lo < 16 {
                    output.push((hi << 4 | lo) as char);
                    i += 3;
                } else {
                    // Invalid percent encoding — keep as is
                    output.push('%');
                    i += 1;
                }
            }
            b'+' => {
                output.push(' ');
                i += 1;
            }
            c => {
                output.push(c as char);
                i += 1;
            }
        }
    }

    output
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 255,
    }
}



#[embassy_executor::task]
pub async fn http_listen_task(stack: Stack<'static>, flash: &'static InternalFlash, control: Mutex<NoopRawMutex, Control<'static>>) {
    let mut server = Box::new(edge_http::io::server::Server::<8,10_000,64>::new());
    let box_buffers = Box::new(TcpBuffers::<8,10_000,3000>::new());
    let tcp = edge_nal_embassy::Tcp::new(stack,&box_buffers);
    let tcp_accept = tcp.bind("0.0.0.0:80".parse().unwrap()).await.unwrap();

    let http_handler = HttpHandler {
        flash,
        control
    };
    server.run(None, tcp_accept, http_handler).await.expect("?");
}

pub struct HttpHandler {
    flash: &'static InternalFlash,
    control: Mutex<NoopRawMutex, Control<'static>>,
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
                            let rtx = self.flash.read_transaction().await;
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
                    }
                    else if method == Method::Post {
                        if path.len() > 3 {
                            let mut data = vec![0u8; 2048];
                            let body_size = conn.read(&mut data).await?;
                            data.truncate(body_size);
                            let mut wtx = self.flash.write_transaction().await;

                            if let Ok(_) = wtx.write(path[3].as_bytes(),&data).await {
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
                    }
                    else {
                        conn.initiate_response(405, Some("Method Not Allowed"), &[("Connection","Close")]).await?;

                    }

                }
                ("api", "scan") => {
                    // let mut control = self.control.lock().await;
                    // let networks = control.get_scan_network_list().await.unwrap();
                    // conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                    //
                    // for network in networks.entries {
                    //     conn.write_all(network.ssid.as_bytes()).await?;
                    //     conn.write_all(&[13,10]).await?;
                    // }
                    conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                    conn.write_all(b"Scanning Disabled").await?;
                },
                ("api","benchmark") => {
                    conn.initiate_response(200, Some("OK"), &[("Content-Type", "text/plain"),("Connection","Close")]).await?;
                    let data = vec![0u8;5000];
                    loop {
                        conn.write_all(&data).await?;
                    }
                }
                ("api", _) => {
                    conn.initiate_response(418, Some("I'm a teapot"), &[("Connection","Close")]).await?;
                }
                (_, _) => {
                    //conn.initiate_response(404, Some("Not Found"), &[("Connection","Close")]).await?;
                }
            }
        }

        let path = conn.headers()?.path.clone();
        let mut path_strip = if path.starts_with("/") {
            path.strip_prefix("/").unwrap().to_string()
        } else { path.parse().unwrap() };

        if path == "/" {
            path_strip.push_str("index.html");
        }

        path_strip = url_decode(path_strip);

        let path_strip = path_strip.as_str();

        let mut is_gzip = false;

        if let Some(accept_encoding) = conn.headers()?.headers.get("Accept-Encoding") {
            if accept_encoding.contains("gzip") {
                is_gzip = true;
            }
        }

        if is_gzip {
            match get_file(format!("{}.gz", path_strip).as_str()) {
                Some(file) => {
                    info!("Serving file from flash, {} using gzip encoded version", path_strip);
                    conn.initiate_response(200, Some("OK"), &[("Content-Type", guess_mime_type(path_strip)),("Content-Encoding", "gzip"),("Content-Length", format!("{}",file.len()).as_str()),("Connection","Close")]).await?;
                    conn.write_all(file).await?;
                    conn.flush().await?;
                    conn.complete().await?;
                    return Ok(());
                }
                None => {}
            }
        }

        match get_file(path_strip) {
            Some(file) => {
                info!("Serving file from flash, {}", path_strip);
                conn.initiate_response(200, Some("OK"), &[("Content-Type", guess_mime_type(path_strip)),("Content-Length", format!("{}",file.len()).as_str()),("Connection","Close")]).await?;
                conn.write_all(file).await?;
                conn.flush().await?;
            }
            None => {
                if !is_gzip && get_file(format!("{}.gz", path_strip).as_str()).is_some() {
                    info!("File was requested but not served as only gzip version is present, {}", path_strip);
                    conn.initiate_response(406, Some("Not Acceptable"), &[("Connection","Close")]).await?;
                    conn.complete().await?;
                    return Ok(())
                }

                conn.initiate_response(302, Some("Found"), &[("Connection","Close"),("Location","http://10.82.50.1/")]).await?;
            }
        }

        conn.complete().await?;
        Ok(())
    }
}