//! Measuring how long a frame takes to get from one machine to the other.
//!
//! The two ends share no clock. `Instant` is per-process and per-boot, so the
//! host's frame timestamps mean nothing to the client on their own: the client
//! can see that a frame was stamped `4_000_000` but not whether that was a
//! millisecond or a minute ago.
//!
//! One ping/pong exchange fixes that, in the manner of NTP. When the client
//! sends its reading on the way out and the host returns its own readings on the
//! way back, the four numbers are enough to separate the clocks from the
//! network — and once the offset is known, every frame's age follows.
//!
//! The one thing this cannot measure is the part spent inside the two machines
//! after the readings are taken: the host's clock is read when capture hands the
//! frame over, and the client's when it arrives. Panel scan-out and the delay
//! between a window changing and DXGI reporting it are invisible from here.

/// An estimate of how the host's clock relates to our own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockOffset {
    /// Host clock minus local clock, in microseconds.
    offset_us: i64,
    /// Round-trip time of the exchange that produced it.
    rtt_us: u64,
}

impl ClockOffset {
    /// Derives an offset from a single exchange.
    ///
    /// `sent` and `arrived` are local readings of the ping leaving and the pong
    /// returning; `taken` and `replied` are the host's readings in between. The
    /// host's own processing time is subtracted from the round trip, and the
    /// offset is the midpoint of the two one-way estimates.
    ///
    /// That midpoint assumes both directions take the same time. When they do
    /// not — one of them queued behind a video frame, say — the answer is out by
    /// at most half the round trip, which is why [`ClockSync`] prefers the
    /// quickest exchange it has seen.
    ///
    /// Returns `None` if the readings contradict each other, since a nonsense
    /// offset is worse than none at all.
    pub fn estimate(sent: u64, arrived: u64, taken: u64, replied: u64) -> Option<Self> {
        let outward = taken as i128 - sent as i128;
        let inward = replied as i128 - arrived as i128;
        let processed = replied as i128 - taken as i128;
        let round_trip = arrived as i128 - sent as i128 - processed;

        // The host cannot answer before it was asked, nor can the round trip be
        // shorter than the host's share of it.
        if processed < 0 || round_trip < 0 {
            return None;
        }

        Some(Self {
            offset_us: i64::try_from((outward + inward) / 2).ok()?,
            rtt_us: u64::try_from(round_trip).ok()?,
        })
    }

    /// Host clock minus local clock, in microseconds.
    pub fn offset_us(self) -> i64 {
        self.offset_us
    }

    /// Round-trip time of the exchange that produced this estimate.
    pub fn rtt_us(self) -> u64 {
        self.rtt_us
    }

    /// How far out the estimate may be.
    ///
    /// With a symmetric path the error is zero; the worst case is the whole
    /// delay landing in one direction, which is half the round trip.
    pub fn accuracy_us(self) -> u64 {
        self.rtt_us / 2
    }

    /// How long ago a host timestamp was, measured at local time `now_us`.
    ///
    /// Negative means the timestamp is still in our future, which happens when
    /// the frame was stamped a hair after the host clock was read — or when the
    /// offset estimate is off by more than the frame's age.
    pub fn age_us(self, timestamp_us: u64, now_us: u64) -> i64 {
        let age = now_us as i128 + self.offset_us as i128 - timestamp_us as i128;
        age.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }
}

/// Keeps the most trustworthy offset estimate seen so far.
///
/// A round trip measured while the link was busy is longer than the real one,
/// and a longer round trip means a less certain offset, so the *smallest* round
/// trip is the one worth keeping.
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    best: Option<ClockOffset>,
}

impl ClockSync {
    /// Creates an empty sync, with no estimate yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds in one exchange, keeping the result only if it beats the current
    /// estimate. Returns the estimate now in force.
    pub fn observe(
        &mut self,
        sent: u64,
        arrived: u64,
        taken: u64,
        replied: u64,
    ) -> Option<ClockOffset> {
        let estimate = ClockOffset::estimate(sent, arrived, taken, replied)?;
        let better = match self.best {
            Some(best) => estimate.rtt_us < best.rtt_us,
            None => true,
        };
        if better {
            self.best = Some(estimate);
        }
        self.best
    }

    /// The estimate in force, if any exchange has succeeded.
    pub fn current(&self) -> Option<ClockOffset> {
        self.best
    }

    /// How old a frame stamped `timestamp_us` is, at local time `now_us`.
    ///
    /// `None` until an exchange has produced an estimate.
    pub fn age_us(&self, timestamp_us: u64, now_us: u64) -> Option<i64> {
        self.best.map(|offset| offset.age_us(timestamp_us, now_us))
    }
}

/// A run of age measurements, summarised.
///
/// Deliberately not a rolling window: the caller records a batch and then reads
/// the summary out, which keeps the arithmetic simple and the memory bounded by
/// however often it reports.
#[derive(Clone, Debug, Default)]
pub struct AgeStats {
    samples: Vec<i64>,
}

impl AgeStats {
    /// Creates an empty set of measurements.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one measurement, in microseconds. Negative ages are kept, since a
    /// cluster of them means the offset estimate needs looking at.
    pub fn record(&mut self, age_us: i64) {
        self.samples.push(age_us);
    }

    /// Number of measurements taken.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing has been measured.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Discards the measurements, ready for the next batch.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Mean age, or `None` if nothing was measured.
    pub fn mean_us(&self) -> Option<i64> {
        if self.samples.is_empty() {
            return None;
        }
        let total: i128 = self.samples.iter().map(|&age| age as i128).sum();
        i64::try_from(total / self.samples.len() as i128).ok()
    }

    /// Age at the given percentile, by the nearest-rank method. `0.95` gives a
    /// value that 95% of measurements were at or below.
    pub fn percentile_us(&self, percentile: f32) -> Option<i64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();

        let fraction = percentile.clamp(0.0, 1.0);
        let rank = (fraction * sorted.len() as f32).ceil() as usize;
        let index = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[index])
    }

    /// Largest age seen.
    pub fn max_us(&self) -> Option<i64> {
        self.samples.iter().copied().max()
    }

    /// Smallest age seen.
    pub fn min_us(&self) -> Option<i64> {
        self.samples.iter().copied().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose clock started ten seconds after ours, pinged after the
    /// session has been up for twenty seconds. Both readings are therefore
    /// positive elapsed times, as they would be in reality.
    const SENT: u64 = 20_000_000;
    const OFFSET: i64 = -10_000_000;

    fn exchange(forward_us: i64, back_us: i64, processed_us: i64) -> (u64, u64, u64, u64) {
        let taken = SENT as i64 + OFFSET + forward_us;
        let replied = taken + processed_us;
        let arrived = replied - OFFSET + back_us;
        (SENT, arrived as u64, taken as u64, replied as u64)
    }

    #[test]
    fn recovers_a_known_offset() {
        let (sent, arrived, taken, replied) = exchange(2_000, 3_000, 0);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();

        // Symmetric enough: 2 ms out, 3 ms back leaves the midpoint 0.5 ms high.
        assert!(
            (offset.offset_us() - OFFSET).abs() <= 500,
            "offset was {}",
            offset.offset_us()
        );
        assert_eq!(offset.rtt_us(), 5_000);
    }

    #[test]
    fn excludes_the_hosts_own_processing_time_from_the_round_trip() {
        // The host sat on the ping for 4 ms before answering.
        let (sent, arrived, taken, replied) = exchange(2_000, 3_000, 4_000);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();

        // Only the 5 ms on the wire counts, not the 4 ms in the host.
        assert_eq!(offset.rtt_us(), 5_000);
        assert!(
            (offset.offset_us() - OFFSET).abs() <= 500,
            "offset was {}",
            offset.offset_us()
        );
    }

    #[test]
    fn a_perfectly_symmetric_path_is_exact() {
        let (sent, arrived, taken, replied) = exchange(5_000, 5_000, 1_000);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();
        assert_eq!(offset.offset_us(), OFFSET);
        assert_eq!(offset.accuracy_us(), 5_000);
    }

    #[test]
    fn the_error_never_exceeds_half_the_round_trip() {
        // All the delay in one direction: the worst case for the midpoint.
        let (sent, arrived, taken, replied) = exchange(9_000, 1_000, 0);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();
        let error = (offset.offset_us() - OFFSET).unsigned_abs();
        assert!(error <= offset.accuracy_us(), "error {error}");
        assert_eq!(error, 4_000);
    }

    #[test]
    fn rejects_readings_that_contradict_each_other() {
        // Answered before it was asked.
        assert_eq!(ClockOffset::estimate(1_000, 2_000, 500, 400), None);
        // The pong arrived sooner than the host could possibly have replied.
        assert_eq!(ClockOffset::estimate(1_000, 1_100, 1_050, 1_400), None);
    }

    #[test]
    fn measures_the_age_of_a_frame() {
        let (sent, arrived, taken, replied) = exchange(2_000, 2_000, 0);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();

        // A frame the host stamped as it was sending it, arriving with the pong.
        let age = offset.age_us(taken, arrived);
        assert_eq!(age, 2_000, "should be the trip back");

        // A frame stamped a second before that one is a second older.
        assert_eq!(offset.age_us(taken - 1_000_000, arrived), 1_002_000);
    }

    #[test]
    fn age_can_be_negative_when_the_estimate_lags() {
        let (sent, arrived, taken, replied) = exchange(2_000, 2_000, 0);
        let offset = ClockOffset::estimate(sent, arrived, taken, replied).unwrap();
        // Stamped just *after* the moment the offset was measured.
        assert!(offset.age_us(taken + 5_000, arrived) < 0);
    }

    #[test]
    fn sync_prefers_the_quickest_exchange() {
        let mut sync = ClockSync::new();

        let busy = exchange(20_000, 20_000, 0);
        let first = sync
            .observe(busy.0, busy.1, busy.2, busy.3)
            .expect("first estimate");
        assert_eq!(first.rtt_us(), 40_000);

        let quiet = exchange(1_000, 1_000, 0);
        let second = sync
            .observe(quiet.0, quiet.1, quiet.2, quiet.3)
            .expect("second estimate");
        assert_eq!(second.rtt_us(), 2_000);

        // A slower exchange afterwards must not displace the good one.
        let slow = exchange(50_000, 50_000, 0);
        let third = sync
            .observe(slow.0, slow.1, slow.2, slow.3)
            .expect("third estimate");
        assert_eq!(third.rtt_us(), 2_000);
        assert_eq!(sync.current(), Some(third));
    }

    #[test]
    fn sync_has_no_estimate_until_one_arrives() {
        let mut sync = ClockSync::new();
        assert_eq!(sync.current(), None);
        assert_eq!(sync.age_us(123, 456), None);

        // Garbage in, still no estimate.
        assert_eq!(sync.observe(1_000, 2_000, 500, 400), None);
        assert_eq!(sync.current(), None);
    }

    #[test]
    fn stats_summarise_a_batch() {
        let mut stats = AgeStats::new();
        assert!(stats.is_empty());
        assert_eq!(stats.mean_us(), None);
        assert_eq!(stats.percentile_us(0.95), None);

        for value in [10, 20, 30, 40, 50] {
            stats.record(value);
        }
        assert_eq!(stats.len(), 5);
        assert_eq!(stats.mean_us(), Some(30));
        assert_eq!(stats.min_us(), Some(10));
        assert_eq!(stats.max_us(), Some(50));
        // Nearest rank: 95% of five samples is the fifth.
        assert_eq!(stats.percentile_us(0.95), Some(50));
        assert_eq!(stats.percentile_us(0.5), Some(30));
        assert_eq!(stats.percentile_us(0.0), Some(10));

        stats.clear();
        assert!(stats.is_empty());
        assert_eq!(stats.mean_us(), None);
    }

    #[test]
    fn stats_are_unbothered_by_an_unsorted_batch() {
        let mut stats = AgeStats::new();
        for value in [30, 10, 50, 20, 40] {
            stats.record(value);
        }
        assert_eq!(stats.percentile_us(0.5), Some(30));
        assert_eq!(stats.max_us(), Some(50));
    }
}
