//! Control-channel session protocol (phase 2).
//!
//! Discovery ([`crate::discovery`]) tells a client *where* a host is. The
//! control channel, which runs over a reliable stream on
//! [`HostInfo::control_port`](crate::HostInfo::control_port), tells it
//! *whether* it may stream and on what terms.
//!
//! This module defines only the messages exchanged before any video flows:
//!
//! ```text
//!   client                                  host
//!     │  ── Hello { protocol, client, caps } ─▶│
//!     │  ◀──────────── Welcome { caps } ───────│   accepted
//!     │  ◀──────────── Reject { reason } ──────│   refused
//! ```
//!
//! Pairing/authentication and the video stream itself are later steps; the
//! message set here is intentionally small and forward-compatible (unknown
//! fields carry defaults).

pub mod frame;
pub mod handshake;
pub mod heartbeat;

use serde::{Deserialize, Serialize};

use crate::protocol::{HostInfo, Platform};

/// Version of the control-channel protocol.
///
/// Independent of [`crate::protocol::PROTOCOL_VERSION`], which version guards the
/// UDP discovery datagrams.
pub const SESSION_PROTOCOL_VERSION: u16 = 1;

/// A video codec a peer is able to encode or decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    /// H.264 / AVC — the baseline; hardware decode is near universal.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// AV1.
    Av1,
}

impl VideoCodec {
    /// Every codec this build knows about, in the order we prefer them.
    pub const ALL: [Self; 3] = [Self::H264, Self::Hevc, Self::Av1];
}

/// Input kinds a peer can send (client) or accept (host).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCapabilities {
    #[serde(default)]
    pub pointer: bool,
    #[serde(default)]
    pub keyboard: bool,
}

/// What a peer is able to do.
///
/// All fields default so that a newer peer sending extra fields, or an older
/// peer omitting them, still decodes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Supported codecs, most preferred first.
    #[serde(default)]
    pub codecs: Vec<VideoCodec>,
    #[serde(default)]
    pub max_width: u32,
    #[serde(default)]
    pub max_height: u32,
    #[serde(default)]
    pub max_fps: u16,
    #[serde(default)]
    pub input: InputCapabilities,
}

impl Capabilities {
    /// The largest capability set both peers can satisfy.
    ///
    /// Codecs keep `self`'s preference order; numeric limits take the smaller of
    /// the two; input kinds require both sides to support them.
    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            codecs: self
                .codecs
                .iter()
                .filter(|codec| other.codecs.contains(codec))
                .copied()
                .collect(),
            max_width: self.max_width.min(other.max_width),
            max_height: self.max_height.min(other.max_height),
            max_fps: self.max_fps.min(other.max_fps),
            input: InputCapabilities {
                pointer: self.input.pointer && other.input.pointer,
                keyboard: self.input.keyboard && other.input.keyboard,
            },
        }
    }
}

/// Identifies the connecting client in logs and, later, in pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub platform: Platform,
    #[serde(default)]
    pub app_version: String,
}

/// Why a host refused a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The client speaks a protocol version the host cannot.
    ProtocolMismatch,
    /// The host is already serving another client.
    Busy,
    /// The client is not paired / not allowed.
    Unauthorized,
}

/// A control-channel message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMessage {
    /// Client → host: opens a session, proposing a protocol version.
    Hello {
        protocol: u16,
        client: ClientInfo,
        capabilities: Capabilities,
    },
    /// Host → client: accepts, stating its own terms.
    Welcome {
        protocol: u16,
        host: HostInfo,
        capabilities: Capabilities,
    },
    /// Host → client: refuses the session.
    Reject { reason: RejectReason },
    /// Either side: liveness probe once the session is established.
    Ping,
    /// Reply to a [`SessionMessage::Ping`].
    Pong,
}

impl SessionMessage {
    /// Convenience constructor for a [`SessionMessage::Hello`] at the current version.
    pub fn hello(client: ClientInfo, capabilities: Capabilities) -> Self {
        Self::Hello {
            protocol: SESSION_PROTOCOL_VERSION,
            client,
            capabilities,
        }
    }

    /// Convenience constructor for a [`SessionMessage::Welcome`] at the current version.
    pub fn welcome(host: HostInfo, capabilities: Capabilities) -> Self {
        Self::Welcome {
            protocol: SESSION_PROTOCOL_VERSION,
            host,
            capabilities,
        }
    }

    /// The protocol version carried by this message, if it has one.
    pub fn protocol(&self) -> Option<u16> {
        match self {
            Self::Hello { protocol, .. } | Self::Welcome { protocol, .. } => Some(*protocol),
            Self::Reject { .. } | Self::Ping | Self::Pong => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HostId;

    fn client() -> ClientInfo {
        ClientInfo {
            name: "Pixel".to_owned(),
            platform: Platform::Android,
            app_version: "0.1.0".to_owned(),
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            codecs: vec![VideoCodec::H264, VideoCodec::Hevc],
            max_width: 1920,
            max_height: 1200,
            max_fps: 60,
            input: InputCapabilities {
                pointer: true,
                keyboard: true,
            },
        }
    }

    fn round_trip(message: &SessionMessage) -> SessionMessage {
        let json = serde_json::to_string(message).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn hello_round_trips() {
        let message = SessionMessage::hello(client(), caps());
        assert_eq!(message.protocol(), Some(SESSION_PROTOCOL_VERSION));
        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn welcome_round_trips() {
        let host = HostInfo::new(HostId::from("host-1"), "DESKTOP");
        let message = SessionMessage::welcome(host, caps());
        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn reject_round_trips() {
        let message = SessionMessage::Reject {
            reason: RejectReason::Unauthorized,
        };
        assert_eq!(message.protocol(), None);
        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn missing_optional_capability_fields_default() {
        // A peer may omit fields it does not know about.
        let json = r#"{"kind":"hello","protocol":1,"client":{"name":"x"},"capabilities":{}}"#;
        let message: SessionMessage = serde_json::from_str(json).unwrap();
        let SessionMessage::Hello { capabilities, .. } = message else {
            panic!("expected hello");
        };
        assert_eq!(capabilities, Capabilities::default());
    }
}
