//! Shared logic for the **Android Virtual Display** project.
//!
//! This crate is deliberately platform agnostic: the same code is compiled into
//! the Windows host and into the Android client. It contains
//!
//! * [`protocol`] – the discovery wire types and [`HostInfo`].
//! * [`net`] – a tiny transport abstraction ([`net::PacketIo`]) plus a UDP
//!   implementation, so the discovery logic can be unit tested without a real
//!   network.
//! * [`discovery`] – the host *responder* and the client *browser*.
//! * [`session`] – the encrypted control channel: handshake, pairing, framing,
//!   cipher and heartbeats.
//! * [`video`] – the binary video frame format.
//! * [`capture`] – the [`FrameSource`] abstraction (and a synthetic test
//!   pattern) that a real capture backend implements.
//!
//! The video **transport** is not wired up yet; the frame format and capture
//! abstraction exist so it can be built next.

pub mod capture;
pub mod codec;
pub mod discovery;
pub mod error;
pub mod net;
pub mod protocol;
pub mod session;
pub mod video;

#[cfg(test)]
mod test_support;

pub use capture::{CaptureError, CapturedFrame, FrameSource};
pub use codec::{decode_rgba, encode_jpeg, CodecError, EncodedImage, RgbaImage};
pub use error::{Error, Result};
pub use protocol::{
    DiscoveryMessage, Envelope, HostId, HostInfo, Platform, DEFAULT_CONTROL_PORT,
    DEFAULT_DISCOVERY_PORT, PROTOCOL_VERSION,
};
pub use session::channel::{Channel, Frame};
pub use session::cipher::{Role, SessionCipher};
pub use session::frame::{FrameDecoder, MAX_FRAME_LEN};
pub use session::handshake::{Action, ClientHandshake, HostHandshake, Session, State};
pub use session::heartbeat::{Heartbeat, HeartbeatAction};
pub use session::pairing::{EphemeralKey, PairingKeys};
pub use session::{
    Capabilities, ClientInfo, InputCapabilities, RejectReason, SessionMessage, VideoCodec,
    SESSION_PROTOCOL_VERSION,
};
pub use video::{FrameFormat, FrameHeader, VideoFrame, HEADER_LEN, VIDEO_VERSION};
