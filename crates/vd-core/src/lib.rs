//! Shared logic for the **Android Virtual Display** project.
//!
//! This crate is deliberately platform agnostic: the same code is compiled into
//! the Windows host and into the Android client. It contains
//!
//! * [`protocol`] – the wire types exchanged between host and client.
//! * [`net`] – a tiny transport abstraction ([`net::PacketIo`]) plus a UDP
//!   implementation, so the discovery logic can be unit tested without a real
//!   network.
//! * [`discovery`] – the host *responder* and the client *browser*.
//! * [`session`] – the control-channel handshake messages.
//!
//! Display/video transport is intentionally **not** implemented yet; only the
//! discovery handshake and the session message types exist today.

pub mod discovery;
pub mod error;
pub mod net;
pub mod protocol;
pub mod session;

#[cfg(test)]
mod test_support;

pub use error::{Error, Result};
pub use protocol::{
    DiscoveryMessage, Envelope, HostId, HostInfo, Platform, DEFAULT_CONTROL_PORT,
    DEFAULT_DISCOVERY_PORT, PROTOCOL_VERSION,
};
pub use session::{
    Capabilities, ClientInfo, InputCapabilities, RejectReason, SessionMessage, VideoCodec,
    SESSION_PROTOCOL_VERSION,
};
