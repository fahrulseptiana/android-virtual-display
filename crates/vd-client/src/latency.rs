//! How long frames take to arrive, and how the two clocks line up.
//!
//! The connection thread measures and the UI reports, so the numbers live behind
//! a mutex. Contention is one producer against one reader and the critical
//! sections are a few additions, so it is not worth anything cleverer.

use std::sync::{Arc, Mutex};

use log::{debug, warn};
use vd_core::{AgeStats, Clock, ClockOffset, ClockSync};

/// A batch of frame ages, summarised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LatencyReadout {
    /// Average age of a frame as it arrived.
    pub mean_us: i64,
    /// Median age.
    pub p50_us: i64,
    /// Age that 95% of frames were at or below.
    pub p95_us: i64,
    /// Worst age in the batch.
    pub max_us: i64,
    /// How many frames the summary covers.
    pub samples: usize,
}

impl LatencyReadout {
    /// Mean age in milliseconds, for display.
    pub fn mean_ms(&self) -> f64 {
        self.mean_us as f64 / 1000.0
    }

    /// 95th percentile age in milliseconds, for display.
    pub fn p95_ms(&self) -> f64 {
        self.p95_us as f64 / 1000.0
    }

    /// Worst age in milliseconds, for display.
    pub fn max_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }
}

/// Frame latency measurements, shared between the connection and the UI.
#[derive(Clone)]
pub struct Latency {
    inner: Arc<Mutex<Inner>>,
    clock: Clock,
}

struct Inner {
    /// How our clock relates to the host's, learned from ping/pong exchanges.
    sync: ClockSync,
    /// Ages of frames that have arrived since the last summary.
    ages: AgeStats,
    /// The most recent summary, kept so the UI has something to show.
    latest: Option<LatencyReadout>,
}

impl Latency {
    /// Starts measuring, with no estimate of the clock offset yet.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sync: ClockSync::new(),
                ages: AgeStats::new(),
                latest: None,
            })),
            clock: Clock::new(),
        }
    }

    /// The reading both ends' timestamps are compared against.
    ///
    /// Clones share the origin, and the UI holds one, so a frame's age can be
    /// taken at the moment it is presented rather than only when it arrived.
    pub fn now_us(&self) -> u64 {
        self.clock.now_us()
    }

    /// Folds in one ping/pong exchange.
    ///
    /// `arrived` is read here, as close to the pong's arrival as the caller can
    /// manage; anything spent before this call counts as network time.
    pub fn observe_exchange(
        &self,
        sent_us: u64,
        recv_us: u64,
        reply_us: u64,
    ) -> Option<ClockOffset> {
        let arrived = self.clock.now_us();
        let mut inner = self.lock();
        let offset = match inner.sync.observe(sent_us, arrived, recv_us, reply_us) {
            Some(offset) => offset,
            None => {
                warn!("ignoring a ping/pong exchange with contradictory timestamps");
                return None;
            }
        };

        debug!(
            "clock offset {:+.3} ms, round trip {:.3} ms, accurate to {:.3} ms",
            offset.offset_us() as f64 / 1000.0,
            offset.rtt_us() as f64 / 1000.0,
            offset.accuracy_us() as f64 / 1000.0,
        );
        Some(offset)
    }

    /// How old a frame stamped `timestamp_us` is now, or `None` before the
    /// first exchange has pinned the offset down.
    pub fn age_us(&self, timestamp_us: u64) -> Option<i64> {
        let now = self.clock.now_us();
        self.lock().sync.age_us(timestamp_us, now)
    }

    /// Records the age of a frame that has just arrived.
    pub fn record(&self, timestamp_us: u64) {
        let now = self.clock.now_us();
        let mut inner = self.lock();
        if let Some(age) = inner.sync.age_us(timestamp_us, now) {
            inner.ages.record(age);
        }
    }

    /// Folds the frames recorded so far into a summary and starts a new batch.
    ///
    /// Returns `None` when nothing has been recorded, or when the offset is
    /// still unknown.
    pub fn summarise(&self) -> Option<LatencyReadout> {
        let mut inner = self.lock();
        if inner.ages.is_empty() {
            return None;
        }

        let readout = LatencyReadout {
            mean_us: inner.ages.mean_us()?,
            p50_us: inner.ages.percentile_us(0.5)?,
            p95_us: inner.ages.percentile_us(0.95)?,
            max_us: inner.ages.max_us()?,
            samples: inner.ages.len(),
        };
        inner.ages.clear();
        inner.latest = Some(readout);
        Some(readout)
    }

    /// The most recent summary, for the UI to display.
    pub fn latest(&self) -> Option<LatencyReadout> {
        self.lock().latest
    }

    /// The clock estimate in force, if any.
    pub fn offset(&self) -> Option<ClockOffset> {
        self.lock().sync.current()
    }

    /// A poisoned lock would mean another thread panicked mid-measurement, which
    /// is no reason to take the whole client down.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for Latency {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose clock started two seconds after ours.
    const OFFSET_US: i64 = 2_000_000;

    fn latency_with_offset() -> Latency {
        let latency = Latency::new();
        let sent = latency.now_us();
        // The host reads its clock and replies at once. Any processing time
        // added here would have to fit inside the round trip, which is only as
        // long as the calls below take.
        let taken = (sent as i64 + OFFSET_US) as u64;
        latency.observe_exchange(sent, taken, taken);

        // The round trip here is whatever the real elapsed time was, so the
        // estimate is close to the true offset but not exactly it.
        latency
    }

    #[test]
    fn measures_how_old_a_frame_is() {
        let latency = latency_with_offset();
        let offset = latency.offset().expect("an estimate after one exchange");

        // A frame the host stamped 50 ms ago, in the host's clock.
        let now_host = (latency.now_us() as i64 + offset.offset_us()) as u64;
        let age = latency.age_us(now_host - 50_000).unwrap();
        assert!(
            (age - 50_000).abs() < 2_000,
            "expected about 50 ms old, got {age} µs"
        );
    }

    #[test]
    fn summaries_report_a_batch_and_reset() {
        let latency = latency_with_offset();
        let offset = latency.offset().unwrap();
        let now_host = (latency.now_us() as i64 + offset.offset_us()) as u64;

        assert_eq!(latency.summarise(), None, "nothing recorded yet");
        assert_eq!(latency.latest(), None);

        // Five frames, all stamped just before now, so each is ~40 ms old.
        for _ in 0..5 {
            latency.record(now_host - 40_000);
        }

        let readout = latency.summarise().expect("a summary");
        assert_eq!(readout.samples, 5);
        assert!(
            (readout.mean_us - 40_000).abs() < 5_000,
            "mean was {}",
            readout.mean_us
        );
        assert!(readout.max_us >= readout.p50_us);
        assert!(readout.p95_us >= readout.p50_us);
        assert!(readout.max_us >= readout.p95_us);
        assert_eq!(latency.latest(), Some(readout));

        // The batch is emptied, so there is nothing to report until more frames
        // arrive — but the last readout stays for display.
        assert_eq!(latency.summarise(), None);
        assert_eq!(latency.latest(), Some(readout));
    }

    #[test]
    fn records_nothing_until_the_offset_is_known() {
        let latency = Latency::new();
        assert_eq!(latency.offset(), None);
        assert_eq!(latency.age_us(123), None);

        latency.record(123);
        assert_eq!(latency.summarise(), None);
    }
}
