use ieee80211::scroll;
use ieee80211::scroll::ctx::{MeasureWith, TryIntoCtx};
use ieee80211::scroll::{Endian, Pwrite};

pub struct PictochatBeacon {
    pub header: [u8; 4],
    pub chatroom: PictochatChatroom,
    pub client_count: u8,
    pub footer: [u8; 2],
}

impl Default for PictochatBeacon {
    fn default() -> Self {
        Self {
            header: [0x48, 0x23, 0x11, 0x0A],
            chatroom: PictochatChatroom::A,
            client_count: 0,
            footer: [0x04, 0x00],
        }
    }
}

impl TryIntoCtx<bool> for PictochatBeacon {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: bool) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.header, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.chatroom, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.client_count, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.footer, &mut offset, Endian::Little)?;

        Ok(offset)
    }
}

impl MeasureWith<bool> for PictochatBeacon {
    fn measure_with(&self, _: &bool) -> usize {
        let mut frame_size = 0;

        frame_size += 4; //header
        frame_size += 1; //chatroom id
        frame_size += 1; //client count
        frame_size += 2; //footer

        frame_size
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum PictochatChatroom {
    A = 0x00,
    B = 0x01,
    C = 0x02,
    D = 0x03
}