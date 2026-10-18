//! The most recent decoded video frame.
//!
//! The connection worker decodes frames off the UI thread and publishes them
//! here; the UI always renders the newest one. Older frames are dropped rather
//! than queued, so the display never lags behind the stream.

use std::sync::{Arc, Mutex};

/// A decoded frame, ready to be uploaded as a texture.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Sequence number from the sender; lets the UI skip redundant uploads.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    /// RGBA, four bytes per pixel, row-major.
    pub pixels: Vec<u8>,
}

/// A one-slot mailbox between the worker and the UI.
#[derive(Clone, Default)]
pub struct LatestFrame {
    slot: Arc<Mutex<Option<Arc<DecodedFrame>>>>,
}

impl LatestFrame {
    /// Creates an empty mailbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `frame`, replacing whatever was there.
    pub fn publish(&self, frame: DecodedFrame) {
        *self.slot.lock().unwrap() = Some(Arc::new(frame));
    }

    /// The newest frame, if any.
    pub fn latest(&self) -> Option<Arc<DecodedFrame>> {
        self.slot.lock().unwrap().clone()
    }

    /// Drops the current frame (used when a session ends).
    pub fn clear(&self) {
        *self.slot.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64) -> DecodedFrame {
        DecodedFrame {
            sequence,
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        }
    }

    #[test]
    fn keeps_only_the_newest_frame() {
        let mailbox = LatestFrame::new();
        assert!(mailbox.latest().is_none());

        mailbox.publish(frame(1));
        mailbox.publish(frame(2));
        assert_eq!(mailbox.latest().unwrap().sequence, 2);

        mailbox.clear();
        assert!(mailbox.latest().is_none());
    }
}
