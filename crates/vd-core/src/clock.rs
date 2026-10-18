//! A monotonic clock with an arbitrary origin.
//!
//! Timestamps only mean something relative to the clock that produced them, and
//! `Instant` has no shared epoch between two processes — or even between two
//! threads that each called `Instant::now()` for themselves. The host therefore
//! stamps everything it sends, video frames and heartbeat replies alike, from a
//! single [`Clock`]. That shared origin is what lets a client line its own clock
//! up with the host's and work out how old a frame was by the time it arrived.

use std::time::Instant;

/// A monotonic clock whose readings are microseconds since it was created.
///
/// Cloning is cheap and shares the origin, so every part of a program that
/// timestamps anything can hold its own handle to the same clock.
#[derive(Clone, Debug)]
pub struct Clock {
    origin: Instant,
}

impl Clock {
    /// Starts a clock at "now".
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// Microseconds elapsed since the clock was created.
    ///
    /// Saturates instead of wrapping. Reaching `u64::MAX` microseconds would
    /// take over five hundred thousand years, so this is a formality — but a
    /// wrapped timestamp would look like a time traveller to the latency maths,
    /// and saturating keeps it merely very large.
    pub fn now_us(&self) -> u64 {
        self.origin
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn advances_with_time() {
        let clock = Clock::new();
        let before = clock.now_us();
        thread::sleep(Duration::from_millis(20));
        let after = clock.now_us();
        assert!(after > before, "{after} should be later than {before}");
    }

    #[test]
    fn clones_share_the_origin() {
        let clock = Clock::new();
        let copy = clock.clone();
        thread::sleep(Duration::from_millis(10));

        // A clone is the same clock, not a fresh one: it has been running since
        // the original started, not since the clone was made.
        let from_clone = copy.now_us();
        assert!(
            from_clone >= 10_000,
            "clone restarted the clock: {from_clone}"
        );
        assert!(clock.now_us() >= from_clone);
    }
}
