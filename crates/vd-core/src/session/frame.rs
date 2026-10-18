//! Length-prefixed framing for the control channel.
//!
//! A frame is a 4-byte big-endian length followed by a JSON
//! [`SessionMessage`]. The framing layer is pure (it works on byte slices), so
//! it can be driven by any stream implementation — a blocking socket, a
//! tokio task, or a mock in tests.
//!
//! ```text
//!   ┌────────────┬──────────────────────────────┐
//!   │ len (u32)  │ json(SessionMessage)         │
//!   └────────────┴──────────────────────────────┘
//! ```

use crate::error::{Error, Result};
use crate::session::SessionMessage;

/// Largest frame we are willing to accept.
///
/// Session messages are tiny (a few hundred bytes); the limit exists to reject
/// a garbled or hostile length prefix instead of allocating wildly.
pub const MAX_FRAME_LEN: u32 = 64 * 1024;

/// Encodes a message into a complete frame.
pub fn encode(message: &SessionMessage) -> Result<Vec<u8>> {
    let payload =
        serde_json::to_vec(message).map_err(|_| Error::Malformed("session json encode"))?;
    if payload.len() as u64 > MAX_FRAME_LEN as u64 {
        return Err(Error::FrameTooLarge {
            len: payload.len() as u32,
            max: MAX_FRAME_LEN,
        });
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Incremental decoder: push bytes as they arrive, pop complete messages.
///
/// Handles arbitrary fragmentation — a message may be split across any number
/// of pushes, and several messages may arrive in a single push.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends freshly received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Returns the next complete message, or `Ok(None)` if more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<SessionMessage>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }

        let len = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]);
        if len > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge {
                len,
                max: MAX_FRAME_LEN,
            });
        }

        let total = 4 + len as usize;
        if self.buffer.len() < total {
            return Ok(None);
        }

        let payload: Vec<u8> = self.buffer.drain(..total).skip(4).collect();
        let message = serde_json::from_slice(&payload)
            .map_err(|_| Error::Malformed("session json decode"))?;
        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Platform;
    use crate::session::{Capabilities, ClientInfo, RejectReason};

    fn hello() -> SessionMessage {
        SessionMessage::hello(
            ClientInfo {
                name: "Pixel".to_owned(),
                platform: Platform::Android,
                app_version: "0.1.0".to_owned(),
            },
            Capabilities::default(),
        )
    }

    #[test]
    fn round_trips_a_single_frame() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&encode(&hello()).unwrap());
        assert_eq!(decoder.next_message().unwrap(), Some(hello()));
        assert_eq!(decoder.next_message().unwrap(), None);
    }

    #[test]
    fn reassembles_a_frame_split_byte_by_byte() {
        let frame = encode(&hello()).unwrap();
        let mut decoder = FrameDecoder::new();
        for (i, byte) in frame.iter().enumerate() {
            decoder.push(&[*byte]);
            let decoded = decoder.next_message().unwrap();
            if i + 1 < frame.len() {
                assert!(decoded.is_none(), "frame completed early at byte {i}");
            } else {
                assert_eq!(decoded, Some(hello()));
            }
        }
    }

    #[test]
    fn decodes_two_frames_from_one_push() {
        let mut bytes = encode(&hello()).unwrap();
        bytes.extend(
            encode(&SessionMessage::Reject {
                reason: RejectReason::Busy,
            })
            .unwrap(),
        );

        let mut decoder = FrameDecoder::new();
        decoder.push(&bytes);
        assert_eq!(decoder.next_message().unwrap(), Some(hello()));
        assert_eq!(
            decoder.next_message().unwrap(),
            Some(SessionMessage::Reject {
                reason: RejectReason::Busy
            })
        );
        assert_eq!(decoder.next_message().unwrap(), None);
    }

    #[test]
    fn rejects_an_oversized_length_prefix() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&(MAX_FRAME_LEN + 1).to_be_bytes());
        assert!(matches!(
            decoder.next_message().unwrap_err(),
            Error::FrameTooLarge { .. }
        ));
    }
}
