use bitflags::bitflags;
use ieee80211::scroll;
use ieee80211::scroll::ctx::{MeasureWith, TryIntoCtx};
use ieee80211::scroll::{Endian, Pwrite};
use crate::DsWifiClientMask;

pub struct DSWiFiBeaconTag<Payload: TryIntoCtx<()> + MeasureWith<()>> {
    pub oui_type: u8,
    pub stepping_offset: [u8; 2],
    pub lcd_video_sync: [u8; 2],
    pub fixed_id: [u8; 4],
    pub game_id: [u8; 4],
    pub stream_code: u16,
    pub beacon_type: BeaconType,
    pub cmd_data_size: u16,
    pub reply_data_size: u16,
    pub payload: Option<Payload>,
}
impl<Payload: TryIntoCtx<(),Error = scroll::Error> + MeasureWith<()>> TryIntoCtx<> for DSWiFiBeaconTag<Payload> {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> where <Payload as TryIntoCtx>::Error: From<ieee80211::scroll::Error> {
        let mut offset: usize = 0;

        buf.gwrite_with(self.oui_type, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.stepping_offset, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.lcd_video_sync, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.fixed_id, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.game_id, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.stream_code, &mut offset, Endian::Little)?;
        if let Some(payload) = &self.payload {
            buf.gwrite_with(payload.measure_with(&ctx) as u8, &mut offset, Endian::Little)?;
        } else {
            buf.gwrite_with(0u8, &mut offset, Endian::Little)?;
        }
        buf.gwrite_with(self.beacon_type as u8, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.cmd_data_size, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.reply_data_size, &mut offset, Endian::Little)?;
        if let Some(payload) = self.payload {
            buf.gwrite(payload, &mut offset)?;
        }

        Ok(offset)
    }
}

impl<Payload: MeasureWith<()> + TryIntoCtx> MeasureWith<()> for DSWiFiBeaconTag<Payload> {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut frame_size = 0;

        frame_size += 1; //oui_type
        frame_size += 2; //stepping_offset
        frame_size += 2; //lcd_video_sync
        frame_size += 4; //fixed_id
        frame_size += 4; //game_id
        frame_size += 2; //stream_code
        frame_size += 1; //payload_size
        frame_size += 1; //beacon_type
        frame_size += 2; //cmd_data_size
        frame_size += 2; //reply_data_size
        if let Some(payload) = &self.payload { //payload
            frame_size += payload.measure_with(&ctx);
        }

        frame_size
    }
}

impl<Payload: TryIntoCtx + core::convert::AsRef<[u8]>> Default for DSWiFiBeaconTag<Payload> {
    fn default() -> Self {
        Self {
            oui_type: 0,                        // should never change
            stepping_offset: [0x0a, 0x00],      // should never change
            lcd_video_sync: [0x00, 0x00],       // eh? idk shouldnt change really but it might
            fixed_id: [0x00, 0x00, 0x00, 0x0a], // should never change maybe?
            game_id: [0x00, 0x00, 0x00, 0x00],
            stream_code: 0,
            beacon_type: BeaconType::EMPTY,
            cmd_data_size: 0,
            reply_data_size: 0,
            payload: None,
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

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct HostToClientFlags: u8 {
        const RESERVED_0 = 1 << 0;
        const RESERVED_1 = 1 << 1;
        const RESERVED_2 = 1 << 2;
        const HAS_FOOTER = 1 << 3;
        const RESERVED_4 = 1 << 4;
        const RESERVED_5 = 1 << 5;
        const RESERVED_6 = 1 << 6;
        const RESERVED_7 = 1 << 7;
    }
}

impl Default for HostToClientFlags {
    fn default() -> Self {
        Self::from_bits_truncate(0)
    }
}

// The host to client data frame as I currently understand it.
// The footer flag will be automatically set if a footer is provided.
pub struct HostToClientDataFrame<Payload: TryIntoCtx<(), Error = scroll::Error> + MeasureWith<()>> {
    pub us_per_client_reply: u16,
    pub client_target_mask: DsWifiClientMask,
    pub flags: HostToClientFlags,
    pub payload: Option<Payload>,
    pub footer: Option<HostToClientFooter>,
}

impl<Payload: MeasureWith<()> + ieee80211::scroll::ctx::TryIntoCtx<(), Error = ieee80211::scroll::Error>> MeasureWith<()> for HostToClientDataFrame<Payload> {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut frame_size = 0;
        //order is same as write for readability

        frame_size += 2; // us_per_client_reply
        frame_size += 2; // client_target_mask
        frame_size += 1; // payload_size
        frame_size += 1; // flags
        if let Some(payload) = &self.payload { //payload
            frame_size += payload.measure_with(ctx);
        }
        if self.footer.is_some() {
            frame_size += 2; // footer seq_number
            frame_size += 2; // footer client_target_mask
        }

        frame_size
    }
}
impl<Payload: TryIntoCtx<Error = scroll::Error> + MeasureWith<()> + core::convert::AsRef<[u8]>> TryIntoCtx<()> for HostToClientDataFrame<Payload> {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset: usize = 0;
        buf.gwrite_with(self.us_per_client_reply, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.client_target_mask, &mut offset, Endian::Little)?;
        if let Some(payload) = &self.payload {
            buf.gwrite_with(payload.measure_with(&ctx) as u8, &mut offset, Endian::Little)?;
        } else {
            buf.gwrite_with(0u8, &mut offset, Endian::Little)?;//no payload, so 0 length
        }
        let mut flags = self.flags;
        if self.footer.is_some() {
            flags.set(HostToClientFlags::HAS_FOOTER, true);
        }
        buf.gwrite_with(flags.bits(), &mut offset, Endian::Little)?;
        if let Some(payload) = self.payload {
            buf.gwrite(payload, &mut offset)?;
        }
        if let Some(footer) = &self.footer {
            buf.gwrite_with(footer.data_seq, &mut offset, Endian::Little)?;
            buf.gwrite_with(footer.client_target_mask, &mut offset, Endian::Little)?;
        }

        Ok(offset)
    }
}

pub struct HostToClientFooter {
    pub data_seq: u16,
    pub client_target_mask: DsWifiClientMask,
}