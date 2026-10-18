//! Decoded frames waiting to be drawn.
//!
//! The connection worker decodes off the UI thread and publishes here; the UI
//! pops one frame per repaint.
//!
//! This is a small **queue**, not a single slot. A hardware decoder does not
//! emit pictures evenly: it stalls and then returns two at once, and with a
//! single slot the second overwrites the first before the UI ever sees it —
//! which halved the displayed frame rate. The queue is bounded, and the oldest
//! frame is dropped when it is full, so a UI that falls behind shows the newest
//! frames rather than drifting into the past.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// How many decoded frames may wait for the UI.
const CAPACITY: usize = 3;

/// A decoded frame, ready to be uploaded as a texture.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Monotonic per-session counter; lets the UI notice skipped frames.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    /// RGBA, four bytes per pixel, row-major.
    pub pixels: Vec<u8>,
}

/// A bounded queue between the decoder and the UI.
#[derive(Clone)]
pub struct FrameQueue {
    pending: Arc<Mutex<VecDeque<Arc<DecodedFrame>>>>,
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
        }
    }
}

impl FrameQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `frame`, dropping the oldest if the UI is behind.
    pub fn publish(&self, frame: DecodedFrame) {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= CAPACITY {
            pending.pop_front();
        }
        pending.push_back(Arc::new(frame));
    }

    /// Takes the oldest waiting frame, if any.
    pub fn next(&self) -> Option<Arc<DecodedFrame>> {
        self.pending.lock().unwrap().pop_front()
    }

    /// How many frames are waiting.
    pub fn len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Whether no frame is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops everything (used when a session ends).
    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
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
    fn delivers_frames_in_order() {
        let queue = FrameQueue::new();
        assert!(queue.next().is_none());

        queue.publish(frame(1));
        queue.publish(frame(2));
        assert_eq!(queue.next().unwrap().sequence, 1);
        assert_eq!(queue.next().unwrap().sequence, 2);
        assert!(queue.next().is_none());
    }

    #[test]
    fn a_burst_is_not_collapsed() {
        // The decoder often returns two pictures at once; both must survive.
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.publish(frame(2));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn drops_the_oldest_when_the_ui_falls_behind() {
        let queue = FrameQueue::new();
        for sequence in 0..CAPACITY as u64 + 2 {
            queue.publish(frame(sequence));
        }
        // The newest CAPACITY frames are kept.
        assert_eq!(queue.len(), CAPACITY);
        assert_eq!(queue.next().unwrap().sequence, 2);
    }

    #[test]
    fn clear_empties_the_queue() {
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.clear();
        assert!(queue.is_empty());
    }
}
