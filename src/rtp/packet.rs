use std::mem::size_of;

use bytes::{Buf, Bytes};
use thiserror::Error;

#[derive(Debug)]
pub(crate) struct RtpPacket {
    /// The size of padding at the end or the packet. If `None`, there is no padding.
    padding: Option<u8>,
    /// An extension header.
    extension_header: Option<ExtensionHeader>,
    has_marker: bool,
    csrc_count: usize,
    payload_type: RtpPayloadType,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    csrc_list: [u32; 15], // FIXME: not ideal :(
    payload: Bytes,
}

#[derive(Debug)]
pub(crate) struct ExtensionHeader {
    id: u16,
    contents: Bytes,
}

#[derive(Debug)]
pub(crate) enum RtpPayloadType {
    Unknown(u8),
}

impl From<u8> for RtpPayloadType {
    fn from(ty: u8) -> Self {
        match ty {
            _ => RtpPayloadType::Unknown(ty),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RtpPacketParseError {
    #[error("Invalid version: {0}")]
    InvalidVersion(u8),

    #[error("The RTP header must at least has 12 bytes, only {0} bytes")]
    InsufficientHeaderSize(u8),

    #[error("Failed to parse CSRC identifiers, expected {0}")]
    InvalidCsrcIdCount(u8),

    #[error("Unable to get the padding size at the end of the packet")]
    NoPaddingSize,

    #[error("Failed to parse extension header")]
    InvalidExtensionHeader,
}

#[rustfmt::skip]
impl RtpPacket {
    const VERSION_MASK: u8      = 0b1100_0000u8;
    const PADDING_BIT: u8       = 0b0010_0000u8;
    const EXTENSION_BIT: u8     = 0b0001_0000u8;
    const CSRC_COUNT_MASK: u8   = 0b0000_1111u8;

    const MARKER_BIT: u8        = 0b1000_0000u8;
    const PAYLOAD_TYPE_MASK: u8 = 0b0111_1111u8;

    const RTP_VERSION: u8       = 0b1000_0000u8; // 2
    const MIN_SIZE: usize       = 12;
}

impl TryFrom<Bytes> for RtpPacket {
    type Error = RtpPacketParseError;

    fn try_from(mut bytes: Bytes) -> Result<Self, Self::Error> {
        if bytes.len() <= RtpPacket::MIN_SIZE {
            return Err(RtpPacketParseError::InsufficientHeaderSize(bytes.len() as u8));
        }

        let first_byte = bytes.get_u8();
        let version = first_byte & RtpPacket::VERSION_MASK;
        if version != RtpPacket::RTP_VERSION {
            return Err(RtpPacketParseError::InvalidVersion(version));
        }
        let has_padding = first_byte & RtpPacket::PADDING_BIT == RtpPacket::PADDING_BIT;
        let has_extension_header = first_byte & RtpPacket::EXTENSION_BIT == RtpPacket::EXTENSION_BIT;
        let csrc_count = (first_byte & RtpPacket::CSRC_COUNT_MASK) as usize;

        let second_byte = bytes.get_u8();
        let has_marker = second_byte & RtpPacket::MARKER_BIT == RtpPacket::MARKER_BIT;
        let payload_type = RtpPayloadType::from(second_byte & RtpPacket::PAYLOAD_TYPE_MASK);

        let sequence_number = bytes.get_u16();
        let timestamp = bytes.get_u32();
        let ssrc = bytes.get_u32();

        let mut csrc_list = [0; 15];
        if bytes.len() < csrc_count * size_of::<u32>(){
            return Err(RtpPacketParseError::InvalidCsrcIdCount(csrc_count as u8));
        }
        for i in 0..csrc_count {
            csrc_list[i] = bytes.get_u32();
        }

        let padding = if has_padding {
            Some(*bytes.last().ok_or(RtpPacketParseError::NoPaddingSize)?)
        } else {
            None
        };
        let (extension_header, payload) = if has_extension_header {
            if bytes.len() < size_of::<u16>() * 2 {
                return Err(RtpPacketParseError::InvalidExtensionHeader);
            }
            let id = bytes.get_u16();
            let length = bytes.get_u16() as usize;
            if bytes.len() < length {
                return Err(RtpPacketParseError::InvalidExtensionHeader);
            }

            let payload = bytes.split_to(length);
            (Some(ExtensionHeader {
                id,
                contents: bytes,
            }), payload)
        } else {
            (None, bytes)
        };

        Ok(RtpPacket {
            padding,
            extension_header,
            has_marker,
            csrc_count,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            csrc_list,
            payload,
        })
    }
}

