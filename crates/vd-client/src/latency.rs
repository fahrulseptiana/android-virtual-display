//! How long frames take to arrive, to decode and to draw.
//!
//! The connection thread measures the first two, the UI the last two, so the
//! numbers live behind a mutex. Contention is one producer against one reader
//! and the critical sections are a few additions, so it is not worth anything
//! cleverer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, warn};
use vd_core::{Clock, ClockOffset, ClockSync, SampleStats};

/// A run of microsecond measurements, summarised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moments {
    pub mean_us: i64,
    pub p50_us: i64,
    pub p95_us: i64,
    pub max_us: i64,
}

impl Moments {
    /// Summarises a run, or `None` if nothing was recorded.
    fn of(stats: &SampleStats) -> Option<Self> {
        if stats.is_empty() {
            return None;
        }
        Some(Self {
            mean_us: stats.mean_us()?,
            p50_us: stats.percentile_us(0.5)?,
            p95_us: stats.percentile_us(0.95)?,
            max_us: stats.max_us()?,
        })
    }

    /// Mean in milliseconds, for display.
    pub fn mean_ms(&self) -> f64 {
        self.mean_us as f64 / 1000.0
    }

    /// 95th percentile in milliseconds, for display.
    pub fn p95_ms(&self) -> f64 {
        self.p95_us as f64 / 1000.0
    }

    /// Worst in milliseconds, for display.
    pub fn max_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }
}

/// One reporting period, across the whole path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyReadout {
    /// Frames covered.
    pub samples: usize,
    /// How old a frame was when it arrived from the network.
    pub arrival: Moments,
    /// How old it was when it was drawn. This is the end-to-end figure, as far
    /// as software can see: capture on the host to pixels on the client.
    pub display: Moments,
    /// Time spent decoding each frame.
    pub decode: Moments,
    /// Time spent building and uploading each texture.
    pub render: Moments,
    /// Frames thrown away undrawn because a newer one had already arrived.
    pub dropped: u64,
}

/// Frame timings, shared between the connection and the UI.
#[derive(Clone)]
pub struct Latency {
    inner: Arc<Mutex<Inner>>,
    clock: Clock,
}

struct Inner {
    /// How our clock relates to the host's, learned from ping/pong exchanges.
    sync: ClockSync,
    arrival: SampleStats,
    display: SampleStats,
    decode: SampleStats,
    render: SampleStats,
    /// The most recent summary, kept so the UI has something to show.
    latest: Option<LatencyReadout>,
}

impl Latency {
    /// Starts measuring, with no estimate of the clock offset yet.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sync: ClockSync::new(),
                arrival: SampleStats::new(),
                display: SampleStats::new(),
                decode: SampleStats::new(),
                render: SampleStats::new(),
                latest: None,
            })),
            clock: Clock::new(),
        }
    }

    /// The reading both ends' timestamps are compared against.
    ///
    /// Clones share the origin, and the UI holds one, so a frame's age can be
    /// taken when it is drawn rather than only when it arrived.
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
    pub fn record_arrival(&self, timestamp_us: u64) {
        let now = self.clock.now_us();
        let mut inner = self.lock();
        if let Some(age) = inner.sync.age_us(timestamp_us, now) {
            inner.arrival.record(age);
        }
    }

    /// Records the age of a frame the moment it is handed to the GPU.
    pub fn record_display(&self, timestamp_us: u64) {
        let now = self.clock.now_us();
        let mut inner = self.lock();
        if let Some(age) = inner.sync.age_us(timestamp_us, now) {
            inner.display.record(age);
        }
    }

    /// Records how long one frame took to decode.
    pub fn record_decode(&self, took: Duration) {
        self.lock().decode.record(took.as_micros() as i64);
    }

    /// Records how long one frame took to build and upload as a texture.
    pub fn record_render(&self, took: Duration) {
        self.lock().render.record(took.as_micros() as i64);
    }

    /// Folds everything recorded so far into a summary and starts a new batch.
    ///
    /// `dropped` comes from the frame queue, which the UI owns; it is passed in
    /// rather than tracked twice. Returns `None` when nothing has been recorded
    /// or the clock offset is still unknown.
    pub fn summarise(&self, dropped: u64) -> Option<LatencyReadout> {
        let mut inner = self.lock();
        let arrival = Moments::of(&inner.arrival)?;

        let readout = LatencyReadout {
            samples: inner.arrival.len(),
            arrival,
            display: Moments::of(&inner.display).unwrap_or_default(),
            decode: Moments::of(&inner.decode).unwrap_or_default(),
            render: Moments::of(&inner.render).unwrap_or_default(),
            dropped,
        };

        inner.arrival.clear();
        inner.display.clear();
        inner.decode.clear();
        inner.render.clear();
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
        latency
    }

    /// A timestamp `age_us` before now, on the host's clock.
    fn stamp(latency: &Latency, age_us: i64) -> u64 {
        let offset = latency.offset().expect("an estimate").offset_us();
        (latency.now_us() as i64 + offset - age_us) as u64
    }

    #[test]
    fn measures_how_old_a_frame_is() {
        let latency = latency_with_offset();
        let age = latency.age_us(stamp(&latency, 50_000)).unwrap();
        assert!(
            (age - 50_000).abs() < 5_000,
            "expected about 50 ms old, got {age} µs"
        );
    }

    #[test]
    fn summaries_cover_the_whole_path_and_reset() {
        let latency = latency_with_offset();
        assert_eq!(latency.summarise(0), None, "nothing recorded yet");
        assert_eq!(latency.latest(), None);

        for _ in 0..5 {
            latency.record_arrival(stamp(&latency, 40_000));
            latency.record_display(stamp(&latency, 55_000));
            latency.record_decode(Duration::from_millis(9));
            latency.record_render(Duration::from_millis(7));
        }

        let readout = latency.summarise(3).expect("a summary");
        assert_eq!(readout.samples, 5);
        assert_eq!(readout.dropped, 3);
        assert!(
            (readout.arrival.mean_ms() - 40.0).abs() < 5.0,
            "arrival was {}",
            readout.arrival.mean_ms()
        );
        assert!(
            (readout.display.mean_ms() - 55.0).abs() < 5.0,
            "display was {}",
            readout.display.mean_ms()
        );
        assert_eq!(readout.decode.mean_ms(), 9.0);
        assert_eq!(readout.render.mean_ms(), 7.0);
        assert!(readout.arrival.max_us >= readout.arrival.p95_us);
        assert!(readout.arrival.p95_us >= readout.arrival.p50_us);
        assert_eq!(latency.latest(), Some(readout));

        // The batch is emptied, so there is nothing to report until more frames
        // arrive — but the last readout stays for display.
        assert_eq!(latency.summarise(9), None);
        assert_eq!(latency.latest(), Some(readout));
    }

    #[test]
    fn records_nothing_until_the_offset_is_known() {
        let latency = Latency::new();
        assert_eq!(latency.offset(), None);
        assert_eq!(latency.age_us(123), None);

        latency.record_arrival(123);
        latency.record_display(123);
        // Durations need no clock, but a summary with no arrival ages has
        // nothing to anchor it.
        latency.record_decode(Duration::from_millis(5));
        assert_eq!(latency.summarise(0), None);
    }
}
