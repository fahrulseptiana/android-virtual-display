//! Test-only helpers.
//!
//! `MockIo` implements [`crate::net::PacketIo`] entirely in memory so the
//! discovery algorithms can be tested deterministically, without sockets,
//! timers or a real network.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Mutex;

use crate::error::Result;
use crate::net::PacketIo;

/// In-memory [`PacketIo`] used by unit tests.
#[derive(Debug)]
pub struct MockIo {
    local: SocketAddr,
    incoming: Mutex<VecDeque<(Vec<u8>, SocketAddr)>>,
    sent: Mutex<Vec<(SocketAddr, Vec<u8>)>>,
}

impl MockIo {
    /// Creates an endpoint bound to `local` with no queued input.
    pub fn new(local: SocketAddr) -> Self {
        Self {
            local,
            incoming: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Queues a datagram that the next [`PacketIo::recv_from`] will return.
    pub fn with_incoming(self, payload: Vec<u8>, from: SocketAddr) -> Self {
        self.incoming.lock().unwrap().push_back((payload, from));
        self
    }

    /// Everything that was sent through this endpoint, in order.
    pub fn sent(&self) -> Vec<(SocketAddr, Vec<u8>)> {
        self.sent.lock().unwrap().clone()
    }
}

impl PacketIo for MockIo {
    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local)
    }

    fn send_to(&self, payload: &[u8], dst: SocketAddr) -> Result<()> {
        self.sent.lock().unwrap().push((dst, payload.to_vec()));
        Ok(())
    }

    fn recv_from(&self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>> {
        let Some((payload, from)) = self.incoming.lock().unwrap().pop_front() else {
            return Ok(None);
        };
        let len = payload.len().min(buf.len());
        buf[..len].copy_from_slice(&payload[..len]);
        Ok(Some((len, from)))
    }
}
