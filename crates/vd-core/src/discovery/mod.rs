//! Host discovery over the local network.
//!
//! The protocol is a simple, symmetric UDP handshake:
//!
//! ```text
//!   client                                  host
//!     │  ── Probe (broadcast) ───────────────▶│
//!     │  ◀────────────── Announce (unicast) ──│
//!     │  ◀────────────── Announce (beacon) ───│   every `announce_interval`
//!     │  ◀────────────── Bye ─────────────────│   on shutdown
//! ```
//!
//! Both sides are *poll driven* (`poll(now)`), which makes them trivially
//! usable from a CLI loop on Windows and from an egui repaint loop on Android.

pub mod browser;
pub mod responder;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::protocol::DEFAULT_DISCOVERY_PORT;

pub use browser::{Browser, DiscoveredHost, DiscoveryEvent};
pub use responder::Responder;

/// Tuning knobs shared by [`Browser`] and [`Responder`].
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// UDP port used by the discovery protocol.
    pub port: u16,
    /// How often a client re-broadcasts a probe while browsing.
    pub probe_interval: Duration,
    /// How often a host emits an unsolicited beacon.
    pub announce_interval: Duration,
    /// How long a host stays in the browser's list without a fresh announce.
    pub host_ttl: Duration,
    /// Additional addresses to send probes to.
    ///
    /// The broadcast address is always included; `127.0.0.1` is added by
    /// default so a host and client on the *same* machine can find each other
    /// during development.
    pub extra_probe_targets: Vec<IpAddr>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_DISCOVERY_PORT,
            probe_interval: Duration::from_millis(1500),
            announce_interval: Duration::from_millis(2000),
            host_ttl: Duration::from_secs(10),
            extra_probe_targets: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        }
    }
}

impl DiscoveryConfig {
    /// Every address a probe/beacon should be sent to.
    pub fn probe_targets(&self) -> Vec<SocketAddr> {
        let mut targets = Vec::with_capacity(self.extra_probe_targets.len() + 1);
        targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), self.port));
        targets.extend(
            self.extra_probe_targets
                .iter()
                .map(|ip| SocketAddr::new(*ip, self.port)),
        );
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_targets_include_broadcast_and_localhost() {
        let targets = DiscoveryConfig::default().probe_targets();
        assert!(targets.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::BROADCAST),
            DEFAULT_DISCOVERY_PORT
        )));
        assert!(targets.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_DISCOVERY_PORT
        )));
    }
}
