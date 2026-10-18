//! Host side of the discovery protocol.

use std::net::SocketAddr;
use std::time::Instant;

use crate::discovery::DiscoveryConfig;
use crate::error::Result;
use crate::net::{PacketIo, UdpPacketIo};
use crate::protocol::{DiscoveryMessage, Envelope, HostInfo};

/// Largest datagram we are willing to read. Discovery packets are tiny.
const RECV_BUFFER: usize = 2048;

/// Answers client probes and emits periodic beacons for `host`.
pub struct Responder<I: PacketIo = UdpPacketIo> {
    io: I,
    config: DiscoveryConfig,
    host: HostInfo,
    next_announce: Option<Instant>,
}

impl Responder<UdpPacketIo> {
    /// Binds the well-known discovery port for `host`.
    pub fn bind(config: DiscoveryConfig, host: HostInfo) -> Result<Self> {
        Ok(Self::new(UdpPacketIo::bind(config.port)?, config, host))
    }
}

impl<I: PacketIo> Responder<I> {
    /// Creates a responder on top of an arbitrary transport.
    pub fn new(io: I, config: DiscoveryConfig, host: HostInfo) -> Self {
        Self {
            io,
            config,
            host,
            next_announce: None,
        }
    }

    /// Address of the discovery socket.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.io.local_addr()
    }

    /// The host this responder advertises.
    pub fn host(&self) -> &HostInfo {
        &self.host
    }

    /// Broadcasts a beacon immediately.
    pub fn announce(&mut self, now: Instant) -> Result<()> {
        let envelope = Envelope::new(DiscoveryMessage::Announce {
            host: self.host.clone(),
        })
        .encode()?;
        for target in self.config.probe_targets() {
            let _ = self.io.send_to(&envelope, target);
        }
        self.next_announce = Some(now + self.config.announce_interval);
        Ok(())
    }

    /// Answers pending probes and emits a beacon when one is due.
    pub fn poll(&mut self, now: Instant) -> Result<()> {
        let mut buf = [0u8; RECV_BUFFER];
        while let Some((len, from)) = self.io.recv_from(&mut buf)? {
            let envelope = match Envelope::decode(&buf[..len]) {
                Ok(envelope) => envelope,
                Err(_) => continue,
            };
            if matches!(envelope.message, DiscoveryMessage::Probe) {
                let reply = Envelope::new(DiscoveryMessage::Announce {
                    host: self.host.clone(),
                })
                .encode()?;
                // Reply unicast so the client learns our address even when the
                // broadcast itself was not delivered.
                let _ = self.io.send_to(&reply, from);
            }
        }

        if self.next_announce.is_none_or(|deadline| now >= deadline) {
            self.announce(now)?;
        }
        Ok(())
    }

    /// Announces that the host is going away. Call before shutdown.
    pub fn bye(&mut self) -> Result<()> {
        let envelope = Envelope::new(DiscoveryMessage::Bye {
            host_id: self.host.id.clone(),
        })
        .encode()?;
        for target in self.config.probe_targets() {
            let _ = self.io.send_to(&envelope, target);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HostId;
    use crate::test_support::MockIo;

    #[test]
    fn replies_to_probe_with_announce() {
        let probe = Envelope::new(DiscoveryMessage::Probe).encode().unwrap();
        let client: SocketAddr = "192.168.1.20:5555".parse().unwrap();
        let io = MockIo::new("0.0.0.0:47700".parse().unwrap()).with_incoming(probe, client);

        let host = HostInfo::new(HostId::from("h1"), "DESKTOP");
        let mut responder = Responder::new(io, DiscoveryConfig::default(), host.clone());
        responder.poll(Instant::now()).unwrap();

        let sent = responder.io.sent();
        assert!(sent.iter().any(|(dst, payload)| {
            *dst == client
                && Envelope::decode(payload).unwrap().message
                    == DiscoveryMessage::Announce { host: host.clone() }
        }));
    }

    #[test]
    fn emits_beacon_when_due() {
        let io = MockIo::new("0.0.0.0:47700".parse().unwrap());
        let config = DiscoveryConfig::default();
        let host = HostInfo::new(HostId::from("h1"), "DESKTOP");
        let mut responder = Responder::new(io, config.clone(), host);

        let t0 = Instant::now();
        responder.poll(t0).unwrap();
        let initial = responder.io.sent().len();
        assert!(initial > 0);

        // Nothing new before the announce interval elapses.
        responder.poll(t0).unwrap();
        assert_eq!(responder.io.sent().len(), initial);

        responder.poll(t0 + config.announce_interval).unwrap();
        assert!(responder.io.sent().len() > initial);
    }
}
