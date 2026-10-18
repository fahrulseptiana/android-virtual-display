//! Liveness tracking for an established control session.
//!
//! [`Heartbeat`] is a tiny, clock-injected state machine: the caller polls it
//! whenever it services the connection and reacts to the returned
//! [`HeartbeatAction`]. It owns no sockets and no timers, so it is deterministic
//! and easy to test.

use std::time::{Duration, Instant};

/// What the caller should do at this instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatAction {
    /// Nothing to do.
    Idle,
    /// Send a `Ping` to the peer.
    SendPing,
    /// The peer has been silent for longer than the timeout.
    TimedOut,
}

/// Tracks when to ping a peer and when to give up on it.
#[derive(Clone, Debug)]
pub struct Heartbeat {
    interval: Duration,
    timeout: Duration,
    next_ping: Instant,
    last_seen: Instant,
}

impl Heartbeat {
    /// Creates a heartbeat that pings every `interval` and times out after
    /// `timeout` without any activity. Initially `timeout` is measured from
    /// `now`.
    pub fn new(interval: Duration, timeout: Duration, now: Instant) -> Self {
        Self {
            interval,
            timeout,
            next_ping: now + interval,
            last_seen: now,
        }
    }

    /// Decides what to do at `now`.
    ///
    /// A timeout takes precedence over sending a ping.
    pub fn poll(&mut self, now: Instant) -> HeartbeatAction {
        if now.saturating_duration_since(self.last_seen) > self.timeout {
            return HeartbeatAction::TimedOut;
        }
        if now >= self.next_ping {
            self.next_ping = now + self.interval;
            return HeartbeatAction::SendPing;
        }
        HeartbeatAction::Idle
    }

    /// Records that anything was received from the peer.
    pub fn on_activity(&mut self, now: Instant) {
        self.last_seen = now;
    }

    /// How long since the peer was last heard from.
    pub fn silence(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heartbeat(now: Instant) -> Heartbeat {
        Heartbeat::new(Duration::from_secs(5), Duration::from_secs(15), now)
    }

    #[test]
    fn pings_on_the_interval() {
        let t0 = Instant::now();
        let mut beat = heartbeat(t0);

        assert_eq!(beat.poll(t0), HeartbeatAction::Idle);
        assert_eq!(
            beat.poll(t0 + Duration::from_secs(4)),
            HeartbeatAction::Idle
        );
        assert_eq!(
            beat.poll(t0 + Duration::from_secs(5)),
            HeartbeatAction::SendPing
        );
        // But only once per interval.
        assert_eq!(
            beat.poll(t0 + Duration::from_secs(6)),
            HeartbeatAction::Idle
        );
    }

    #[test]
    fn activity_postpones_the_timeout() {
        let t0 = Instant::now();
        let mut beat = heartbeat(t0);

        let t = t0 + Duration::from_secs(10);
        beat.on_activity(t);
        // Without the activity the peer would already be past the timeout.
        assert_ne!(
            beat.poll(t + Duration::from_secs(14)),
            HeartbeatAction::TimedOut
        );
        assert_eq!(
            beat.poll(t + Duration::from_secs(16)),
            HeartbeatAction::TimedOut
        );
    }

    #[test]
    fn times_out_after_silence() {
        let t0 = Instant::now();
        let mut beat = heartbeat(t0);
        // The timeout must be *strictly* exceeded.
        assert_ne!(
            beat.poll(t0 + Duration::from_secs(15)),
            HeartbeatAction::TimedOut
        );
        assert_eq!(
            beat.poll(t0 + Duration::from_millis(15_001)),
            HeartbeatAction::TimedOut
        );
    }
}
