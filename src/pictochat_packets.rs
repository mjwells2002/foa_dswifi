use alloc::vec;
use alloc::vec::Vec;
use ieee80211::mac_parser::MACAddress;
use ieee80211::scroll;
use ieee80211::scroll::ctx::{MeasureWith, TryFromCtx, TryIntoCtx};
use ieee80211::scroll::{Endian, Pread, Pwrite};
use ieee80211::scroll::Endian::Little;
use crate::DsWifiClientMask;

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

impl TryIntoCtx<()> for PictochatBeacon {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], _: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.header, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.chatroom as u8, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.client_count, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.footer, &mut offset, Endian::Little)?;

        Ok(offset)
    }
}

impl MeasureWith<()> for PictochatBeacon {
    fn measure_with(&self, _: &()) -> usize {
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

#[derive(Debug, Eq, PartialEq)]
pub struct PictochatHeader {
    pub type_id: u16,
    pub size_with_header: u16,
}

impl MeasureWith<()> for PictochatHeader {
    fn measure_with(&self, _: &()) -> usize {
        4
    }
}
impl TryIntoCtx<()> for PictochatHeader {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.type_id, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.size_with_header, &mut offset, Endian::Little)?;
        Ok(offset)
    }
}

impl TryFromCtx<'_, ()> for PictochatHeader {
    type Error = scroll::Error;

    fn try_from_ctx(from: &[u8], _: ()) -> Result<(Self, usize), Self::Error> {
        let mut offset = 0;
        Ok((Self {
            type_id: from.gread_with(&mut offset, Little)?,
            size_with_header: from.gread_with(&mut offset, Little)?,
        }, offset))
    }
}
pub struct PictochatType45 {
    pub header: PictochatHeader,
    pub magic: [u8; 4],
    pub members: [MACAddress;16],
}

impl MeasureWith<()> for PictochatType45 {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut size = 0;
        size += self.header.measure_with(ctx);
        size += self.magic.len();
        size += self.members.len() * 6;

        size
    }
}

impl TryIntoCtx<()> for PictochatType45 {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;

        buf.gwrite_with(self.header, &mut offset, ctx)?;
        buf.gwrite_with(self.magic, &mut offset, Endian::Little)?;
        for member in self.members {
            for o in (0..6).step_by(2) {
                buf.gwrite_with(member[o+1], &mut offset, Endian::Little)?;
                buf.gwrite_with(member[o], &mut offset, Endian::Little)?;
            }
        }

        Ok(offset)
    }
}

impl Default for PictochatType45 {
    fn default() -> Self {
        Self {
            header: PictochatHeader {
                type_id: 5,
                size_with_header: 104,
            },
            magic: [0xfd, 0x32, 0xea, 0x59],
            members: [MACAddress::new([0;6]);16],
        }
    }
}

pub struct PictochatType1 {
    pub header: PictochatHeader,
    pub console_id: DsWifiClientMask,
    pub magic_1: [u8;2],
    pub data_size: u16,
    pub magic_2: [u8;10],
}

impl Default for PictochatType1 {
    fn default() -> Self {
        Self {
            header: PictochatHeader {
                type_id: 1,
                size_with_header: 20,
            },
            console_id: 0,
            magic_1: [0xff, 0xff],
            data_size: 0,
            magic_2: [0x00, 0x00,
                0x00, 0x00, 0x00, 0x00,
                0xb7, 0x78, 0xd5, 0x29],
        }
    }
}

impl MeasureWith<()> for PictochatType1 {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut size = 0;
        size += self.header.measure_with(ctx);
        size += 2; //console_id
        size += self.magic_1.len();
        size += 2; //data size
        size += self.magic_2.len();

        size
    }
}

impl TryIntoCtx<()> for PictochatType1 {
    type Error = scroll::Error;
    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.header, &mut offset, ctx)?;
        buf.gwrite_with(self.console_id, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.magic_1, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.data_size, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.magic_2, &mut offset, Endian::Little)?;

        Ok(offset)
    }
}

impl TryFromCtx<'_, ()> for PictochatType1 {
    type Error = scroll::Error;

    fn try_from_ctx(from: &[u8], ctx: ()) -> Result<(Self, usize), Self::Error> {
        let mut offset = 0;
        Ok((Self {
            header: from.gread_with(&mut offset, ctx)?,
            console_id: from.gread_with(&mut offset, Little)?,
            magic_1: from.gread_with(&mut offset, Little)?,
            data_size: from.gread_with(&mut offset, Little)?,
            magic_2: from.gread_with(&mut offset, Little)?,
        }, offset))
    }
}

pub struct PictochatType2 {
    pub header: PictochatHeader,
    pub sending_console_id: u8,
    pub payload_type: u8,
    //payload_length: u8,
    pub transfer_flags: u8,
    pub write_offset: u16,
    //todo: do something better
    pub payload: Vec<u8>,
}

impl MeasureWith<()> for PictochatType2 {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut size = 0;
        size += self.header.measure_with(ctx);
        size += 1; //sending_console_id
        size += 1; //payload_type
        size += 1; //payload_length
        size += 1; //transfer_flags
        size += 2; //write_offset;
        size += self.payload.len(); //payload

        size
    }
}

impl TryIntoCtx<()> for PictochatType2 {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.header, &mut offset, ctx)?;
        buf.gwrite_with(self.sending_console_id, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.payload_type, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.payload.len() as u8, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.transfer_flags, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.write_offset, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.payload.as_slice(), &mut offset, ctx)?;

        Ok(offset)
    }
}

impl TryFromCtx<'_, ()> for PictochatType2 {
    type Error = scroll::Error;

    fn try_from_ctx(from: &[u8], ctx: ()) -> Result<(Self, usize), Self::Error> {
        let mut offset = 0;
        let header: PictochatHeader = from.gread_with(&mut offset, ctx)?;
        let sending_console_id: u8 = from.gread_with(&mut offset, Little)?;
        let payload_type: u8 = from.gread_with(&mut offset, Little)?;
        let payload_length: u8 = from.gread_with(&mut offset, Little)?;
        let transfer_flags: u8 = from.gread_with(&mut offset, Little)?;
        let write_offset: u16 = from.gread_with(&mut offset, Little)?;
        let mut payload = vec![0u8; payload_length as usize];
        payload.copy_from_slice(&from[offset..offset+payload_length as usize]);
        offset += payload_length as usize;
        Ok((Self {
            header,
            sending_console_id,
            payload_type,
            transfer_flags,
            write_offset,
            payload,
        }, offset))
    }
}

//TODO: figure out the text encoding, its 16 bit width, and the lower 7 bits seem ascii compatible, and its not utf-16le
#[derive(Debug,Eq,PartialEq)]
pub struct ConsoleIdPayload {
    pub magic: [u8;2],
    pub to: MACAddress,
    pub name: [u8;20],
    pub bio: [u8;52],
    pub colour: u16,
    pub birth_day: u8,
    pub birth_month: u8,
}

impl Default for ConsoleIdPayload {
    fn default() -> Self {
        Self {
            magic: [0x03, 0x00],
            to: MACAddress::new([0u8;6]),
            name: [0u8;20],
            bio: [0u8;52],
            colour: 0,
            birth_day: 1,
            birth_month: 1,
        }
    }
}

impl MeasureWith<()> for ConsoleIdPayload {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut size = 0;
        size += self.magic.len();
        size += self.to.len();
        size += self.name.len();
        size += self.bio.len();
        size += 2; // colour
        size += 1; // bday
        size += 1; // bmonth

        size
    }
}

impl TryIntoCtx<()> for ConsoleIdPayload {
    type Error = scroll::Error;
    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.magic, &mut offset, Endian::Little)?;
        for o in (0..6).step_by(2) {
            buf.gwrite_with(self.to[o+1], &mut offset, Endian::Little)?;
            buf.gwrite_with(self.to[o], &mut offset, Endian::Little)?;
        }
        buf.gwrite_with(self.name, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.bio, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.colour, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.birth_day, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.birth_month, &mut offset, Endian::Little)?;

        Ok(offset)
    }
}

impl TryFromCtx<'_, ()> for ConsoleIdPayload {
    type Error = scroll::Error;

    fn try_from_ctx(from: &[u8], ctx: ()) -> Result<(Self, usize), Self::Error> {
        let mut offset = 0;
        let magic = from.gread_with(&mut offset, Little)?;
        let mut mac = [0u8;6];

        for o in (0..6).step_by(2) {
            mac[o+1] = from.gread_with(&mut offset, Little)?;
            mac[o] = from.gread_with(&mut offset, Little)?;
        }

        Ok((Self {
            magic,
            to: MACAddress::new(mac),
            name: from.gread_with(&mut offset, Little)?,
            bio: from.gread_with(&mut offset, Little)?,
            colour: from.gread_with(&mut offset, Little)?,
            birth_day: from.gread_with(&mut offset, Little)?,
            birth_month: from.gread_with(&mut offset, Little)?,
        }, offset))
    }
}

pub struct MessagePayload {
    pub magic: u8,
    pub subtype: u8,
    pub from: MACAddress,
    pub magic_1: [u8; 14],
    pub safezone: [u8; 14],
    pub message: Vec<u8>,
}

impl Default for MessagePayload {
    fn default() -> Self {
        Self {
            magic: 3,
            subtype: 2,
            from: MACAddress::new([0u8;6]),
            magic_1: [0x00u8, 0x05, 0x00, 0x00, 0x00, 0x00, 0x03, 0x06, 0x08, 0x0D, 0x08, 0x0D, 0x12, 0x1B],
            safezone: [0x00u8; 14],
            message: vec![0; 0],
        }
    }
}

impl TryIntoCtx<()> for MessagePayload {
    type Error = scroll::Error;

    fn try_into_ctx(self, buf: &mut [u8], ctx: ()) -> Result<usize, Self::Error> {
        let mut offset = 0;
        buf.gwrite_with(self.magic, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.subtype, &mut offset, Endian::Little)?;
        for o in (0..6).step_by(2) {
            buf.gwrite_with(self.from[o+1], &mut offset, Endian::Little)?;
            buf.gwrite_with(self.from[o], &mut offset, Endian::Little)?;
        }
        buf.gwrite_with(self.magic_1, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.safezone, &mut offset, Endian::Little)?;
        buf.gwrite_with(self.message.as_slice(), &mut offset, ctx)?;

        Ok(offset)
    }
}

impl MeasureWith<()> for MessagePayload {
    fn measure_with(&self, ctx: &()) -> usize {
        let mut size = 0;
        size += 1; // magic
        size += 1; // subtype
        size += 6; // from
        size += self.magic_1.len();
        size += self.safezone.len();
        size += self.message.len();

        size
    }
}