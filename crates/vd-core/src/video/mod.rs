//! Video frame wire format.
//!
//! A frame is a fixed-size binary header followed by the encoded payload. It is
//! deliberately separate from the JSON control messages: video needs a compact,
//! binary representation.
//!
//! ```text
//!   ┌─────────┬────────┬───────┬────────┬──────────┬──────────────┬─────────┐
//!   │ version │ format │ width │ height │ sequence │ timestamp_us │ payload │
//!   │  u8     │  u8    │  u32  │  u32   │   u64    │     u64      │  bytes  │
//!   └─────────┴────────┴───────┴────────┴──────────┴──────────────┴─────────┘
//!    \_______________________ 26-byte header ______________________/
//! ```

use crate::error::{Error, Result};

/// Version of this framing. Bumped on incompatible changes.
pub const VIDEO_VERSION: u8 = 1;

/// Size of the fixed header in bytes.
pub const HEADER_LEN: usize = 26;

/// Refuse absurd dimensions rather than trusting them (8K is the practical ceiling).
pub const MAX_DIMENSION: u32 = 8192;

/// How the payload bytes are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFormat {
    /// JPEG image. Interim codec used until H.264 lands.
    Jpeg,
    /// Uncompressed BGRA, four bytes per pixel.
    RawBgra,
    /// Uncompressed RGBA, four bytes per pixel.
    RawRgba,
    /// H.264 Annex-B access unit.
    H264,
    /// H.265 / HEVC Annex-B access unit.
    Hevc,
}

impl FrameFormat {
    /// A stable byte used on the wire.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Jpeg => 1,
            Self::RawBgra => 2,
            Self::RawRgba => 3,
            Self::H264 => 4,
            Self::Hevc => 5,
        }
    }

    /// Parses a wire byte.
    pub fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            1 => Ok(Self::Jpeg),
            2 => Ok(Self::RawBgra),
            3 => Ok(Self::RawRgba),
            4 => Ok(Self::H264),
            5 => Ok(Self::Hevc),
            _ => Err(Error::Malformed("unknown frame format")),
        }
    }

    /// Whether the payload is uncompressed pixels of a known size.
    pub fn is_raw(self) -> bool {
        matches!(self, Self::RawBgra | Self::RawRgba)
    }

    /// Bytes per pixel for raw formats.
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::RawBgra | Self::RawRgba => Some(4),
            _ => None,
        }
    }
}

/// Fixed-size metadata describing a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Monotonic per-session counter; lets the client detect drops.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    /// Capture timestamp in microseconds (same clock as the other frames of a session).
    pub timestamp_us: u64,
}

impl FrameHeader {
    /// Serialises the header to its 26-byte wire form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0u8; HEADER_LEN];
        bytes[0] = VIDEO_VERSION;
        bytes[1] = self.format.to_byte();
        bytes[2..6].copy_from_slice(&self.width.to_be_bytes());
        bytes[6..10].copy_from_slice(&self.height.to_be_bytes());
        bytes[10..18].copy_from_slice(&self.sequence.to_be_bytes());
        bytes[18..26].copy_from_slice(&self.timestamp_us.to_be_bytes());
        bytes
    }

    /// Parses and validates a header.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Malformed("video header too short"));
        }
        let version = bytes[0];
        if version != VIDEO_VERSION {
            return Err(Error::UnsupportedVersion {
                peer: u16::from(version),
                local: u16::from(VIDEO_VERSION),
            });
        }

        let width = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        let height = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::Malformed("video dimensions out of range"));
        }

        Ok(Self {
            width,
            height,
            format: FrameFormat::from_byte(bytes[1])?,
            sequence: u64::from_be_bytes([
                bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15], bytes[16],
                bytes[17],
            ]),
            timestamp_us: u64::from_be_bytes([
                bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23], bytes[24],
                bytes[25],
            ]),
        })
    }

    /// Payload size implied by the header for raw formats.
    pub fn expected_payload_len(&self) -> Option<usize> {
        let bytes_per_pixel = self.format.bytes_per_pixel()?;
        (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(bytes_per_pixel)
    }
}

/// A header together with its encoded payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl VideoFrame {
    /// Builds a frame from a header and payload.
    pub fn new(header: FrameHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    /// Serialises the frame to its wire form.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&self.header.encode());
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// Parses a frame, validating the payload against the header.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let header = FrameHeader::decode(bytes)?;
        let payload = bytes[HEADER_LEN..].to_vec();
        if let Some(expected) = header.expected_payload_len() {
            if payload.len() != expected {
                return Err(Error::Malformed("raw frame payload has the wrong size"));
            }
        }
        Ok(Self { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(format: FrameFormat) -> FrameHeader {
        FrameHeader {
            sequence: 42,
            width: 1280,
            height: 800,
            format,
            timestamp_us: 1_234_567,
        }
    }

    #[test]
    fn header_round_trips() {
        for format in [
            FrameFormat::Jpeg,
            FrameFormat::RawBgra,
            FrameFormat::RawRgba,
            FrameFormat::H264,
            FrameFormat::Hevc,
        ] {
            let original = header(format);
            assert_eq!(FrameHeader::decode(&original.encode()).unwrap(), original);
        }
    }

    #[test]
    fn jpeg_frame_round_trips() {
        let frame = VideoFrame::new(header(FrameFormat::Jpeg), vec![0xff, 0xd8, 0xff, 0xd9]);
        assert_eq!(VideoFrame::decode(&frame.encode()).unwrap(), frame);
    }

    #[test]
    fn raw_frame_checks_the_payload_size() {
        // 2x1 BGRA = 8 bytes.
        let mut h = header(FrameFormat::RawBgra);
        h.width = 2;
        h.height = 1;
        let frame = VideoFrame::new(h, vec![0; 8]);
        assert_eq!(VideoFrame::decode(&frame.encode()).unwrap(), frame);

        let short = VideoFrame::new(h, vec![0; 7]);
        assert!(VideoFrame::decode(&short.encode()).is_err());
    }

    #[test]
    fn rejects_a_truncated_header() {
        assert!(FrameHeader::decode(&[0u8; HEADER_LEN - 1]).is_err());
    }

    #[test]
    fn rejects_an_unknown_version() {
        let mut bytes = header(FrameFormat::Jpeg).encode();
        bytes[0] = VIDEO_VERSION + 1;
        assert!(matches!(
            FrameHeader::decode(&bytes).unwrap_err(),
            Error::UnsupportedVersion { .. }
        ));
    }

    #[test]
    fn rejects_an_unknown_format() {
        let mut bytes = header(FrameFormat::Jpeg).encode();
        bytes[1] = 0xee;
        assert!(matches!(
            FrameHeader::decode(&bytes).unwrap_err(),
            Error::Malformed(_)
        ));
    }

    #[test]
    fn rejects_zero_and_absurd_dimensions() {
        for (width, height) in [(0u32, 100u32), (100, 0), (MAX_DIMENSION + 1, 100)] {
            let mut bytes = header(FrameFormat::Jpeg).encode();
            bytes[2..6].copy_from_slice(&width.to_be_bytes());
            bytes[6..10].copy_from_slice(&height.to_be_bytes());
            assert!(
                FrameHeader::decode(&bytes).is_err(),
                "{width}x{height} should be rejected"
            );
        }
    }
}
