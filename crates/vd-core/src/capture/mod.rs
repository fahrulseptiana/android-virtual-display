//! Desktop capture.
//!
//! [`FrameSource`] is the seam between "where pixels come from" and everything
//! downstream (encoding, streaming). The synthetic [`TestPattern`] implements it
//! so the rest of the pipeline can be built and measured before a real capture
//! backend exists; DXGI Desktop Duplication will slot in behind the same trait.

pub mod test_pattern;

use crate::video::FrameFormat;

pub use test_pattern::TestPattern;

/// A captured image, in whatever format the source produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    /// Capture time in microseconds, on the source's own clock.
    pub timestamp_us: u64,
    pub data: Vec<u8>,
}

impl CapturedFrame {
    /// Bytes a frame of this size needs in `format`, if the format is raw.
    pub fn payload_len(width: u32, height: u32, format: FrameFormat) -> Option<usize> {
        let bytes_per_pixel = format.bytes_per_pixel()?;
        (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(bytes_per_pixel)
    }
}

/// Why a frame could not be captured.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The backend is not available on this machine.
    #[error("capture backend unavailable: {0}")]
    Unavailable(String),
    /// The backend existed but failed.
    #[error("capture failed: {0}")]
    Failed(String),
}

/// A source of frames.
pub trait FrameSource: Send {
    /// Human readable description, for logs.
    fn describe(&self) -> String;

    /// Returns the next frame, or `None` if nothing changed since the last call.
    ///
    /// May block until a frame is available.
    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_payload_lengths() {
        assert_eq!(
            CapturedFrame::payload_len(1920, 1080, FrameFormat::RawBgra),
            Some(1920 * 1080 * 4)
        );
        assert_eq!(CapturedFrame::payload_len(10, 10, FrameFormat::Jpeg), None);
    }
}
