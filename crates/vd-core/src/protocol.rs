//! Wire format shared by the host and the client.
//!
//! Every datagram is a magic prefix followed by a JSON encoded [`Envelope`].
//! JSON keeps the protocol debuggable (you can read packets in Wireshark) at a
//! negligible cost for the handful of bytes a discovery handshake needs.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Bumped whenever the wire format changes in an incompatible way.
pub const PROTOCOL_VERSION: u16 = 1;

/// Magic prefix identifying a Virtual Display datagram.
pub const MAGIC: [u8; 4] = *b"VDD1";

/// UDP port the host listens on and the client broadcasts to.
pub const DEFAULT_DISCOVERY_PORT: u16 = 47700;

/// TCP port reserved for the future display/control connection.
pub const DEFAULT_CONTROL_PORT: u16 = 47701;

/// Stable, machine-local identifier of a host.
///
/// It is opaque to the protocol; clients only use it to de-duplicate hosts and
/// to react to a [`DiscoveryMessage::Bye`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    /// Creates an identifier from anything string-like.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for HostId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for HostId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Operating system a host is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Linux,
    Macos,
    Android,
    /// Forward-compatible catch-all for future platforms.
    Other(String),
}

impl Platform {
    /// The platform this binary was compiled for.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other(std::env::consts::OS.to_owned())
        }
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows => f.write_str("windows"),
            Self::Linux => f.write_str("linux"),
            Self::Macos => f.write_str("macos"),
            Self::Android => f.write_str("android"),
            Self::Other(name) => f.write_str(name),
        }
    }
}

/// Everything a client needs to know about a discovered host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// Stable identifier of the host.
    pub id: HostId,
    /// Human readable name (usually the machine name).
    pub name: String,
    /// Operating system the host runs.
    #[serde(default)]
    pub platform: Platform,
    /// Version of the host application.
    #[serde(default)]
    pub app_version: String,
    /// Port the client should connect to for the (future) display stream.
    #[serde(default = "default_control_port")]
    pub control_port: u16,
}

fn default_control_port() -> u16 {
    DEFAULT_CONTROL_PORT
}

impl HostInfo {
    /// Builds a [`HostInfo`] with sensible defaults for the current platform.
    pub fn new(id: HostId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            platform: Platform::current(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            control_port: DEFAULT_CONTROL_PORT,
        }
    }
}

/// Payload carried by an [`Envelope`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscoveryMessage {
    /// Sent by a client to ask "is anybody out there?".
    Probe,
    /// Sent by a host in reply to a [`DiscoveryMessage::Probe`], or periodically
    /// as an unsolicited beacon.
    Announce { host: HostInfo },
    /// Sent by a host when it is shutting down.
    Bye { host_id: HostId },
}

/// Versioned container for a [`DiscoveryMessage`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: u16,
    pub message: DiscoveryMessage,
}

impl Envelope {
    /// Wraps a message using the current [`PROTOCOL_VERSION`].
    pub fn new(message: DiscoveryMessage) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            message,
        }
    }

    /// Encodes the envelope into a datagram: `MAGIC || json`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&MAGIC);
        serde_json::to_writer(&mut buf, self).map_err(|_| Error::Malformed("json encode"))?;
        Ok(buf)
    }

    /// Decodes a datagram produced by [`Envelope::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = bytes
            .strip_prefix(&MAGIC)
            .ok_or(Error::Malformed("bad magic prefix"))?;
        let envelope: Self =
            serde_json::from_slice(body).map_err(|_| Error::Malformed("bad json body"))?;
        if envelope.protocol > PROTOCOL_VERSION {
            return Err(Error::UnsupportedVersion {
                peer: envelope.protocol,
                local: PROTOCOL_VERSION,
            });
        }
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_host() -> HostInfo {
        HostInfo::new(HostId::from("host-1"), "DESKTOP")
    }

    #[test]
    fn round_trips_announce() {
        let envelope = Envelope::new(DiscoveryMessage::Announce {
            host: sample_host(),
        });
        let decoded = Envelope::decode(&envelope.encode().unwrap()).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn round_trips_unit_variants() {
        let probe = Envelope::new(DiscoveryMessage::Probe);
        assert_eq!(Envelope::decode(&probe.encode().unwrap()).unwrap(), probe);

        let bye = Envelope::new(DiscoveryMessage::Bye {
            host_id: HostId::from("host-1"),
        });
        assert_eq!(Envelope::decode(&bye.encode().unwrap()).unwrap(), bye);
    }

    #[test]
    fn rejects_missing_magic() {
        let err = Envelope::decode(b"nope").unwrap_err();
        assert!(matches!(err, Error::Malformed(_)));
    }

    #[test]
    fn rejects_newer_protocol() {
        let mut bytes = Vec::from(MAGIC);
        let json = format!(
            r#"{{"protocol":{},"message":{{"kind":"probe"}}}}"#,
            PROTOCOL_VERSION + 1
        );
        bytes.extend_from_slice(json.as_bytes());
        assert!(matches!(
            Envelope::decode(&bytes).unwrap_err(),
            Error::UnsupportedVersion { .. }
        ));
    }
}
