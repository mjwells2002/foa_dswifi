use alloc::vec::Vec;
use core::mem::transmute;
use core::slice::from_raw_parts;

#[repr(C, packed(1))]
pub struct DSWiFiBeaconTag {
    pub oui_type: u8,
    pub stepping_offset: [u8; 2],
    pub lcd_video_sync: [u8; 2],
    pub fixed_id: [u8; 4],
    pub game_id: [u8; 4],
    pub stream_code: u16,
    pub app_payload_size: u8,
    pub beacon_type: BeaconType,
    pub cmd_data_size: u16,
    pub reply_data_size: u16,
}

impl DSWiFiBeaconTag {
    pub fn to_bytes(&mut self, payload: Option<Vec<u8>>) -> Vec<u8> {
        match payload {
            Some(data) => {
                self.app_payload_size = data.len() as u8;
                let pre_payload: *const u8 = unsafe { transmute(self) };
                unsafe {
                    [
                        from_raw_parts(pre_payload, size_of::<Self>()),
                        data.as_slice(),
                    ]
                        .concat()
                }
            }
            None => unsafe {
                [from_raw_parts(transmute(self), size_of::<Self>()), &[0]].concat()
            },
        }
    }
}

impl Default for DSWiFiBeaconTag {
    fn default() -> Self {
        Self {
            oui_type: 0,                        // should never change
            stepping_offset: [0x0a, 0x00],      // should never change
            lcd_video_sync: [0x00, 0x00],       // eh? idk shouldnt change really but it might
            fixed_id: [0x00, 0x00, 0x00, 0x0a], // should never change maybe?
            game_id: [0x00, 0x00, 0x00, 0x00],
            stream_code: 0,
            app_payload_size: 0,
            beacon_type: BeaconType::EMPTY,
            cmd_data_size: 0,
            reply_data_size: 0,
        }
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum BeaconType {
    MULTICART = 0x01,
    EMPTY = 0x09,
    MULTIBOOT = 0x0b,
}

