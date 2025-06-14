use bitflags::{bitflags, Flags};
use defmt::error;
use embedded_io_async::Read;
use ieee80211::scroll;
use ieee80211::scroll::ctx::{MeasureWith, TryFromCtx, TryIntoCtx};
use ieee80211::scroll::{Endian, Pread, Pwrite};
use ieee80211::scroll::Endian::Little;
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

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> where <Payload as TryIntoCtx>::Error: From<scroll::Error> {
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

impl<Payload: TryIntoCtx + AsRef<[u8]>> Default for DSWiFiBeaconTag<Payload> {
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

pub struct ClientToHostDataFrame {
    pub payload_size: u16,
    pub flags: ClientToHostFlags,
    pub payload: Option<([u8;300],u16)>,
    pub footer_seq_no: Option<u16>,
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ClientToHostFlags: u8 {
        const RESERVED_0 = 1 << 0;
        const RESERVED_1 = 1 << 1;
        const RESERVED_2 = 1 << 2;
        const HAS_FOOTER = 1 << 3;
        const RESERVED_4 = 1 << 4;
        const LENGTH_IS_BYTES = 1 << 5;
        const RESERVED_6 = 1 << 6;
        const RESERVED_7 = 1 << 7;
    }
}

impl TryFromCtx<'_, ()> for ClientToHostDataFrame<> {
    type Error = scroll::Error;

    //TODO: do something better for the payload, this is dirty
    fn try_from_ctx(from: &[u8], _: ()) -> Result<(Self, usize), Self::Error> {
        let mut offset = 0;

        let payload_size_raw: u8 = from.gread_with(&mut offset, Little)?;
        let mut payload_size = payload_size_raw as u16;
        let flags_raw: u8 = from.gread_with(&mut offset, Little)?;
        let flags = ClientToHostFlags::from_bits_truncate(flags_raw);
        if !flags.contains(ClientToHostFlags::LENGTH_IS_BYTES) {
            payload_size = payload_size * 2; //length is halfwords by default unless this bit is set
        }
        let payload = {
            let mut payload = [0u8;300];
            let mut local_payload_size = payload_size as usize;
            if local_payload_size > 300 {
                error!("ignoring payload size of {} bytes, max is 300", payload_size);
                local_payload_size = 0;
            }
            if local_payload_size > 0 && local_payload_size < 300 {
                payload[..local_payload_size].copy_from_slice(&from[offset..offset+local_payload_size]);
            }
            offset += local_payload_size;
            if local_payload_size == 0 {
                None
            } else {
                Some((payload,payload_size))
            }
        };
        let footer = if flags.contains(ClientToHostFlags::HAS_FOOTER) {
            let footer_raw: u16 = from.gread_with(&mut offset, Little)?;
            Some(footer_raw)
        } else { None };

        let me = Self {
            payload_size,
            flags,
            payload,
            footer_seq_no: footer,
        };

        Ok((me,offset))
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
impl<Payload: MeasureWith<()> + TryIntoCtx<(), Error = scroll::Error>> MeasureWith<()> for HostToClientDataFrame<Payload> {
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
impl<Payload: TryIntoCtx<Error = scroll::Error> + MeasureWith<()> + AsRef<[u8]>> TryIntoCtx<()> for HostToClientDataFrame<Payload> {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset: usize = 0;
        buf.gwrite_with(self.us_per_client_reply, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.client_target_mask, &mut offset, Endian::Little)?;
        if let Some(payload) = &self.payload {
            buf.gwrite_with((payload.measure_with(&ctx) / 2) as u8, &mut offset, Endian::Little)?;
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