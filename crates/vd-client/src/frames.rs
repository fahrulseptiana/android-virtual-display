//! Decoded frames waiting to be drawn.
//!
//! The connection worker decodes off the UI thread and publishes here; the UI
//! takes a frame on each repaint.
//!
//! **In order, until a frame is late.** Frames are drawn oldest-first, because
//! the screen refreshes far more often than frames arrive — measured at 53
//! repaints/s against 28 frames/s — and the decoder hands back two pictures at
//! once. Drawing both costs nothing, and taking only the newer one would halve
//! the smoothness of motion while saving no time at all.
//!
//! What is *not* worth drawing is a frame that has fallen well behind the
//! freshest one waiting: the screen would show the past for a repaint and then
//! jump forward. Those are counted and dropped.
//!
//! A queue rather than a bare slot because a hardware decoder does not emit
//! evenly, and publishes are spread over the wait between repaints.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many decoded frames may wait for the UI.
const CAPACITY: usize = 3;

/// How far behind the freshest waiting frame a frame may fall before it is
/// dropped rather than drawn.
///
/// At 30 fps a frame is 33 ms behind its successor, and two pictures returned
/// together differ by exactly that — so this must sit comfortably above one
/// frame interval, or a burst would be thinned to a single picture. It must
/// also stay well below three, or a backlog would be walked through frame by
/// frame instead of being skipped.
const LATE_AFTER_US: i64 = 70_000;

/// A decoded frame, ready to be uploaded as a texture.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Monotonic per-session counter; lets the UI notice skipped frames.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    /// Host clock when this frame was captured, for measuring its age.
    pub timestamp_us: u64,
    /// RGBA, four bytes per pixel, row-major.
    pub pixels: Vec<u8>,
}

/// A bounded queue between the decoder and the UI.
#[derive(Clone)]
pub struct FrameQueue {
    pending: Arc<Mutex<VecDeque<Arc<DecodedFrame>>>>,
    /// Frames thrown away without ever being drawn, cumulative for the session.
    dropped: Arc<AtomicU64>,
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl FrameQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `frame`, dropping the oldest if nothing has read for a while.
    pub fn publish(&self, frame: DecodedFrame) {
        let mut pending = self.lock();
        if pending.len() >= CAPACITY {
            pending.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        pending.push_back(Arc::new(frame));
    }

    /// Takes the frame to draw, given each waiting frame's age in microseconds.
    ///
    /// The age comes from a closure because only the caller knows how the host's
    /// clock relates to this one. A frame whose age cannot be determined is
    /// treated as current, so losing the measurement degrades to drawing in
    /// order rather than to a blank screen.
    pub fn take_for_display(
        &self,
        age_us: impl Fn(u64) -> Option<i64>,
    ) -> Option<Arc<DecodedFrame>> {
        let mut pending = self.lock();
        if pending.is_empty() {
            return None;
        }

        let ages: Vec<i64> = pending
            .iter()
            .map(|frame| age_us(frame.timestamp_us).unwrap_or(0))
            .collect();
        let (index, stale) = choose_frame(&ages, LATE_AFTER_US)?;

        // Drop only what has been overtaken; anything newer stays queued for
        // the repaints that follow, which is what keeps motion smooth.
        for _ in 0..index {
            pending.pop_front();
        }
        if stale > 0 {
            self.dropped.fetch_add(stale as u64, Ordering::Relaxed);
        }
        pending.pop_front()
    }

    /// Takes the newest waiting frame, counting the rest as dropped.
    ///
    /// For callers that only ever want the latest, such as the headless example.
    pub fn latest(&self) -> Option<Arc<DecodedFrame>> {
        let waiting: Vec<_> = self.lock().drain(..).collect();
        let newest = waiting.len().checked_sub(1);
        self.dropped
            .fetch_add(newest.unwrap_or(0) as u64, Ordering::Relaxed);
        newest.and_then(|index| waiting.into_iter().nth(index))
    }

    /// How many frames are waiting.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no frame is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Frames discarded without being drawn since the session started.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drops everything, for the end of a session.
    pub fn clear(&self) {
        self.lock().clear();
        self.dropped.store(0, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<DecodedFrame>>> {
        // A panic in another thread is no reason to lose frames here.
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Chooses which waiting frame to draw, from their ages, oldest first.
///
/// Returns the index to draw and how many earlier frames are too stale to
/// bother with. The freshest frame always qualifies, so this is never empty
/// given a non-empty slice.
fn choose_frame(ages_us: &[i64], late_after_us: i64) -> Option<(usize, usize)> {
    let newest = ages_us.iter().copied().min()?;
    let index = ages_us
        .iter()
        .position(|&age| age - newest <= late_after_us)?;
    Some((index, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64) -> DecodedFrame {
        DecodedFrame {
            sequence,
            width: 1,
            height: 1,
            timestamp_us: sequence * 1000,
            pixels: vec![0, 0, 0, 255],
        }
    }

    /// Ages as the queue would see them: newest frame last, 33 ms apart.
    fn burst(count: usize) -> Vec<i64> {
        (0..count)
            .map(|i| (count - 1 - i) as i64 * 33_000)
            .collect()
    }

    #[test]
    fn draws_in_order_when_nothing_is_late() {
        // A burst of pictures one frame interval apart is not late by any
        // measure, so the oldest is drawn first and nothing is dropped.
        assert_eq!(choose_frame(&burst(2), LATE_AFTER_US), Some((0, 0)));
        assert_eq!(choose_frame(&burst(3), LATE_AFTER_US), Some((0, 0)));
    }

    #[test]
    fn skips_frames_that_have_fallen_behind() {
        // Ages 300, 200, 100, 0 ms: only the newest is worth drawing now.
        let ages = [300_000, 200_000, 100_000, 0];
        assert_eq!(choose_frame(&ages, LATE_AFTER_US), Some((3, 3)));
    }

    #[test]
    fn never_drops_everything() {
        // However far behind, the freshest frame still gets drawn.
        let ages = [9_000_000, 8_000_000];
        assert_eq!(choose_frame(&ages, LATE_AFTER_US), Some((1, 1)));
        assert_eq!(choose_frame(&[], LATE_AFTER_US), None);
    }

    #[test]
    fn takes_frames_in_order_until_one_is_late() {
        let queue = FrameQueue::new();
        assert!(queue.take_for_display(|_| Some(0)).is_none());

        queue.publish(frame(1));
        queue.publish(frame(2));
        // Both are current: each is drawn, and neither counts as dropped.
        assert_eq!(queue.take_for_display(|_| Some(0)).unwrap().sequence, 1);
        assert_eq!(queue.take_for_display(|_| Some(0)).unwrap().sequence, 2);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn drops_the_stale_ones_when_the_ui_falls_behind() {
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.publish(frame(2));
        queue.publish(frame(3));

        let age = |timestamp_us: u64| match timestamp_us {
            1_000 => Some(200_000),
            2_000 => Some(100_000),
            _ => Some(0),
        };
        assert_eq!(queue.take_for_display(age).unwrap().sequence, 3);
        assert_eq!(queue.dropped(), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn an_unknown_age_degrades_to_drawing_in_order() {
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.publish(frame(2));
        // No clock estimate yet: show them rather than nothing.
        assert_eq!(queue.take_for_display(|_| None).unwrap().sequence, 1);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn publishing_far_ahead_of_reading_bounds_memory() {
        let queue = FrameQueue::new();
        for sequence in 0..CAPACITY as u64 + 2 {
            queue.publish(frame(sequence));
        }
        // Only the last CAPACITY survive, and the two that did not are counted.
        assert_eq!(queue.len(), CAPACITY);
        assert_eq!(queue.dropped(), 2);
    }

    #[test]
    fn latest_takes_the_newest_and_counts_the_rest() {
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.publish(frame(2));
        queue.publish(frame(3));
        assert_eq!(queue.latest().unwrap().sequence, 3);
        assert_eq!(queue.dropped(), 2);
        assert!(queue.is_empty());
        assert!(queue.latest().is_none());
    }

    #[test]
    fn clear_forgets_the_session() {
        let queue = FrameQueue::new();
        queue.publish(frame(1));
        queue.publish(frame(2));
        queue.latest();
        assert_eq!(queue.dropped(), 1);

        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn carries_the_capture_timestamp_for_measuring_age() {
        let queue = FrameQueue::new();
        queue.publish(frame(7));
        assert_eq!(queue.latest().unwrap().timestamp_us, 7_000);
    }
}
