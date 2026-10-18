//! Transport abstraction.
//!
//! [`PacketIo`] is the single seam between the discovery *logic* and the actual
//! network. Production code uses [`UdpPacketIo`]; tests use an in-memory fake,
//! which keeps the discovery algorithms honest without opening sockets.

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};

use crate::error::Result;

/// A connectionless, "send a datagram to an address / receive a datagram from
/// an address" transport.
pub trait PacketIo {
    /// The address this endpoint is bound to.
    fn local_addr(&self) -> Result<SocketAddr>;

    /// Sends one datagram. Implementations must not block.
    fn send_to(&self, payload: &[u8], dst: SocketAddr) -> Result<()>;

    /// Receives one datagram if one is available.
    ///
    /// Returns `Ok(None)` when the socket would block instead of spinning.
    fn recv_from(&self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>>;
}

/// UDP implementation of [`PacketIo`].
///
/// Broadcast is enabled on construction so callers can simply
/// [`send_to`](PacketIo::send_to) the broadcast address.
#[derive(Debug)]
pub struct UdpPacketIo {
    socket: UdpSocket,
}

impl UdpPacketIo {
    /// Binds to `0.0.0.0:<port>` (use `0` for an ephemeral port).
    pub fn bind(port: u16) -> Result<Self> {
        Self::bind_addr((Ipv4Addr::UNSPECIFIED, port))
    }

    /// Binds to an arbitrary address.
    pub fn bind_addr(addr: impl ToSocketAddrs) -> Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket })
    }

    /// Access to the underlying socket (used by the host for diagnostics).
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }
}

impl PacketIo for UdpPacketIo {
    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    fn send_to(&self, payload: &[u8], dst: SocketAddr) -> Result<()> {
        self.socket.send_to(payload, dst)?;
        Ok(())
    }

    fn recv_from(&self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>> {
        match self.socket.recv_from(buf) {
            Ok(received) => Ok(Some(received)),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}
