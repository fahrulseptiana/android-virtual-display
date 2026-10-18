//! Client side of the discovery protocol.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use crate::discovery::DiscoveryConfig;
use crate::error::Result;
use crate::net::{PacketIo, UdpPacketIo};
use crate::protocol::{DiscoveryMessage, Envelope, HostId, HostInfo};

/// Largest datagram we are willing to read. Discovery packets are tiny.
const RECV_BUFFER: usize = 2048;

/// A host the browser currently believes to be online.
#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    /// Announced information.
    pub info: HostInfo,
    /// Source address the announce arrived from.
    pub addr: SocketAddr,
    /// When we last heard from this host.
    pub last_seen: Instant,
}

impl DiscoveredHost {
    /// How long ago we last heard from this host.
    pub fn age(&self, now: Instant) -> std::time::Duration {
        now.saturating_duration_since(self.last_seen)
    }

    /// Address the client should connect to for the display stream.
    pub fn control_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr.ip(), self.info.control_port)
    }
}

/// Something that changed since the previous [`Browser::poll`].
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    /// A host appeared.
    Found(DiscoveredHost),
    /// A host disappeared (said [`DiscoveryMessage::Bye`] or timed out).
    Lost(HostId),
}

/// Discovers hosts on the local network.
pub struct Browser<I: PacketIo = UdpPacketIo> {
    io: I,
    config: DiscoveryConfig,
    hosts: HashMap<HostId, DiscoveredHost>,
    next_probe: Option<Instant>,
}

impl Browser<UdpPacketIo> {
    /// Binds an ephemeral UDP socket and starts browsing with `config`.
    pub fn bind(config: DiscoveryConfig) -> Result<Self> {
        Ok(Self::new(UdpPacketIo::bind(0)?, config))
    }
}

impl<I: PacketIo> Browser<I> {
    /// Creates a browser on top of an arbitrary transport.
    pub fn new(io: I, config: DiscoveryConfig) -> Self {
        Self {
            io,
            config,
            hosts: HashMap::new(),
            next_probe: None,
        }
    }

    /// Address of the underlying socket.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.io.local_addr()
    }

    /// Broadcasts a probe immediately.
    pub fn scan(&mut self, now: Instant) -> Result<()> {
        let probe = Envelope::new(DiscoveryMessage::Probe).encode()?;
        for target in self.config.probe_targets() {
            // A missing route (e.g. no broadcast interface) must not abort the
            // whole scan; other targets may still succeed.
            let _ = self.io.send_to(&probe, target);
        }
        self.next_probe = Some(now + self.config.probe_interval);
        Ok(())
    }

    /// Processes pending packets, expires stale hosts and, when due, probes.
    ///
    /// Returns the events observed during this call. Call it regularly (e.g.
    /// every 200 ms).
    pub fn poll(&mut self, now: Instant) -> Result<Vec<DiscoveryEvent>> {
        let mut events = Vec::new();

        let mut buf = [0u8; RECV_BUFFER];
        while let Some((len, from)) = self.io.recv_from(&mut buf)? {
            let envelope = match Envelope::decode(&buf[..len]) {
                Ok(envelope) => envelope,
                // Ignore foreign / malformed traffic on the port.
                Err(_) => continue,
            };
            match envelope.message {
                DiscoveryMessage::Announce { host } => {
                    let discovered = DiscoveredHost {
                        info: host,
                        addr: from,
                        last_seen: now,
                    };
                    let is_new = !self.hosts.contains_key(&discovered.info.id);
                    self.hosts
                        .insert(discovered.info.id.clone(), discovered.clone());
                    if is_new {
                        events.push(DiscoveryEvent::Found(discovered));
                    }
                }
                DiscoveryMessage::Bye { host_id } => {
                    if self.hosts.remove(&host_id).is_some() {
                        events.push(DiscoveryEvent::Lost(host_id));
                    }
                }
                DiscoveryMessage::Probe => {}
            }
        }

        events.extend(self.expire(now));

        let probe_due = self.next_probe.is_none_or(|deadline| now >= deadline);
        if probe_due {
            self.scan(now)?;
        }

        Ok(events)
    }

    /// Drops hosts that have been silent for longer than [`DiscoveryConfig::host_ttl`].
    fn expire(&mut self, now: Instant) -> Vec<DiscoveryEvent> {
        let ttl = self.config.host_ttl;
        let stale: Vec<HostId> = self
            .hosts
            .iter()
            .filter(|(_, host)| host.age(now) > ttl)
            .map(|(id, _)| id.clone())
            .collect();

        stale
            .into_iter()
            .filter_map(|id| self.hosts.remove(&id).map(|_| DiscoveryEvent::Lost(id)))
            .collect()
    }

    /// All hosts currently believed to be online.
    pub fn hosts(&self) -> impl Iterator<Item = &DiscoveredHost> {
        self.hosts.values()
    }

    /// Number of hosts currently believed to be online.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// Whether no host has been found.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockIo;
    use std::time::Duration;

    fn announce_packet(host: HostInfo) -> Vec<u8> {
        Envelope::new(DiscoveryMessage::Announce { host })
            .encode()
            .unwrap()
    }

    #[test]
    fn discovers_and_expires_hosts() {
        let host = HostInfo::new(HostId::from("h1"), "DESKTOP");
        let peer: SocketAddr = "192.168.1.10:47700".parse().unwrap();
        let io = MockIo::new("0.0.0.0:1234".parse().unwrap())
            .with_incoming(announce_packet(host.clone()), peer);

        let config = DiscoveryConfig::default();
        let mut browser = Browser::new(io, config.clone());

        let t0 = Instant::now();
        let events = browser.poll(t0).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], DiscoveryEvent::Found(_)));
        assert_eq!(browser.len(), 1);

        let discovered = browser.hosts().next().unwrap();
        assert_eq!(discovered.control_addr().ip(), peer.ip());

        // Time passes without new announces -> host is dropped.
        let events = browser
            .poll(t0 + config.host_ttl + Duration::from_secs(1))
            .unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, DiscoveryEvent::Lost(id) if id.as_str() == "h1")));
        assert!(browser.is_empty());
    }

    #[test]
    fn broadcasts_a_probe_on_first_poll() {
        let io = MockIo::new("0.0.0.0:1234".parse().unwrap());
        let mut browser = Browser::new(io, DiscoveryConfig::default());
        let now = Instant::now();
        browser.poll(now).unwrap();

        let sent = browser.io.sent();
        assert_eq!(sent.len(), DiscoveryConfig::default().probe_targets().len());
        let probe = Envelope::decode(&sent[0].1).unwrap();
        assert_eq!(probe.message, DiscoveryMessage::Probe);
    }

    #[test]
    fn ignores_malformed_datagrams() {
        let io = MockIo::new("0.0.0.0:1234".parse().unwrap())
            .with_incoming(b"garbage".to_vec(), "10.0.0.1:1".parse().unwrap());
        let mut browser = Browser::new(io, DiscoveryConfig::default());
        let events = browser.poll(Instant::now()).unwrap();
        assert!(events.is_empty());
        assert!(browser.is_empty());
    }
}
