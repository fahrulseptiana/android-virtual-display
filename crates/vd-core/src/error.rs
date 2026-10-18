use std::io;

/// Errors produced by the shared core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Any socket / IO failure surfaced by the transport.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// The payload could not be decoded.
    #[error("malformed packet: {0}")]
    Malformed(&'static str),

    /// A peer announced a protocol newer than we understand.
    #[error("unsupported protocol version: peer={peer}, local={local}")]
    UnsupportedVersion { peer: u16, local: u16 },

    /// A frame declared a length we refuse to buffer.
    #[error("frame too large: {len} bytes (max {max})")]
    FrameTooLarge { len: u32, max: u32 },

    /// A peer sent a message that is not valid for the current state.
    #[error("protocol violation: {0}")]
    Protocol(&'static str),

    /// The operating system's random source was unavailable.
    #[error("could not obtain system entropy")]
    Entropy,

    /// Encryption or decryption of a session message failed.
    #[error("crypto error: {0}")]
    Crypto(&'static str),
}

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
