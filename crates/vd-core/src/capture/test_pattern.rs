//! A synthetic [`FrameSource`] that draws a moving gradient.
//!
//! It exists so the encoder, transport, decoder and renderer can be built and
//! measured end to end before any real capture backend is involved. The output
//! is deterministic apart from the animation, which makes it easy to eyeball
//! (a coloured bar sweeps across the screen) and to test.

use crate::clock::Clock;

use crate::video::FrameFormat;

use super::{CaptureError, CapturedFrame, FrameSource};

/// How many pixels the bar moves per frame.
const BAR_STEP: u32 = 16;
/// Width of the moving bar, in pixels.
const BAR_WIDTH: u32 = 24;

/// Draws a gradient with a sweeping highlight bar.
pub struct TestPattern {
    width: u32,
    height: u32,
    frame: u64,
    clock: Clock,
}

impl TestPattern {
    /// Creates a pattern of the given size, stamping frames from `clock`.
    ///
    /// The clock is passed in rather than created here so that every source
    /// shares the host's epoch: frame timestamps have to be comparable with the
    /// ones the control channel hands out.
    pub fn new(width: u32, height: u32, clock: Clock) -> Self {
        Self {
            width,
            height,
            frame: 0,
            clock,
        }
    }

    /// Renders the current frame into an RGBA buffer.
    fn render(&self) -> Vec<u8> {
        let mut pixels = vec![0u8; (self.width * self.height * 4) as usize];
        let bar_x = (self.frame as u32).wrapping_mul(BAR_STEP) % self.width.max(1);

        for y in 0..self.height {
            for x in 0..self.width {
                let offset = ((y * self.width + x) * 4) as usize;
                let in_bar = x.wrapping_sub(bar_x) < BAR_WIDTH;
                let (r, g, b) = if in_bar {
                    (255, 255, 255)
                } else {
                    (
                        (x * 255 / self.width.max(1)) as u8,
                        (y * 255 / self.height.max(1)) as u8,
                        ((x + y) % 256) as u8,
                    )
                };
                pixels[offset] = r;
                pixels[offset + 1] = g;
                pixels[offset + 2] = b;
                pixels[offset + 3] = 255;
            }
        }
        pixels
    }
}

impl FrameSource for TestPattern {
    fn describe(&self) -> String {
        format!("test pattern {}x{} (rgba)", self.width, self.height)
    }

    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        let frame = CapturedFrame {
            width: self.width,
            height: self.height,
            format: FrameFormat::RawRgba,
            timestamp_us: self.clock.now_us(),
            data: self.render(),
        };
        self.frame = self.frame.wrapping_add(1);
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_rgba_frames_of_the_right_size() {
        let mut source = TestPattern::new(64, 32, Clock::new());
        let frame = source.next_frame().unwrap().unwrap();

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 32);
        assert_eq!(frame.format, FrameFormat::RawRgba);
        assert_eq!(frame.data.len(), 64 * 32 * 4);
        assert_eq!(
            CapturedFrame::payload_len(frame.width, frame.height, frame.format),
            Some(frame.data.len())
        );
    }

    #[test]
    fn animates_between_frames() {
        let mut source = TestPattern::new(128, 64, Clock::new());
        let first = source.next_frame().unwrap().unwrap();
        let second = source.next_frame().unwrap().unwrap();
        assert_ne!(first.data, second.data, "the bar should move");
    }

    #[test]
    fn timestamps_advance() {
        let mut source = TestPattern::new(8, 8, Clock::new());
        let first = source.next_frame().unwrap().unwrap();
        let second = source.next_frame().unwrap().unwrap();
        assert!(second.timestamp_us >= first.timestamp_us);
    }
}
