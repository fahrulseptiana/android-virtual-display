//! Message transport for the control channel.
//!
//! A [`Channel`] turns [`SessionMessage`]s and [`VideoFrame`]s into framed bytes
//! and back. It starts in plaintext (the handshake needs to be readable) and
//! switches to authenticated encryption once pairing has produced a key.
//!
//! Every frame carries a one-byte kind *inside* the sealed payload, so the peer
//! cannot tell from the outside whether a frame is control or video:
//!
//! ```text
//!   sealed( 0x01 || json(SessionMessage) )
//!   sealed( 0x02 || FrameHeader || payload )
//! ```

use crate::error::{Error, Result};
use crate::session::cipher::SessionCipher;
use crate::session::frame;
use crate::session::SessionMessage;
use crate::video::VideoFrame;

/// Payload kind: a JSON [`SessionMessage`].
const KIND_CONTROL: u8 = 0x01;
/// Payload kind: a binary [`VideoFrame`].
const KIND_VIDEO: u8 = 0x02;

/// Something received on the control channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// A control message.
    ///
    /// Boxed because `SessionMessage` carries fixed-size keys and is several
    /// times larger than a video frame's header; without this every `Frame`
    /// would be sized for the rarer case.
    Control(Box<SessionMessage>),
    /// A video frame.
    Video(VideoFrame),
}

/// Encodes and decodes session traffic, optionally sealed.
#[derive(Debug, Default)]
pub struct Channel {
    cipher: Option<SessionCipher>,
}

impl Channel {
    /// A plaintext channel, used while the handshake is in progress.
    pub fn plaintext() -> Self {
        Self { cipher: None }
    }

    /// Switches the channel to encrypted transport.
    pub fn encrypt(&mut self, cipher: SessionCipher) {
        self.cipher = Some(cipher);
    }

    /// Whether the channel is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Encodes a control message into a complete, length-prefixed frame.
    pub fn encode(&mut self, message: &SessionMessage) -> Result<Vec<u8>> {
        let mut plain = Vec::with_capacity(64);
        plain.push(KIND_CONTROL);
        serde_json::to_writer(&mut plain, message).map_err(|_| Error::Malformed("json encode"))?;
        self.seal(&plain)
    }

    /// Encodes a video frame into a complete, length-prefixed frame.
    pub fn encode_video(&mut self, frame: &VideoFrame) -> Result<Vec<u8>> {
        let encoded = frame.encode();
        let mut plain = Vec::with_capacity(1 + encoded.len());
        plain.push(KIND_VIDEO);
        plain.extend_from_slice(&encoded);
        self.seal(&plain)
    }

    /// Decodes one frame payload.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Frame> {
        let plain = match self.cipher.as_mut() {
            Some(cipher) => cipher.open(payload)?,
            None => payload.to_vec(),
        };

        let Some((kind, body)) = plain.split_first() else {
            return Err(Error::Malformed("empty frame"));
        };
        match *kind {
            KIND_CONTROL => Ok(Frame::Control(Box::new(
                serde_json::from_slice(body).map_err(|_| Error::Malformed("json decode"))?,
            ))),
            KIND_VIDEO => Ok(Frame::Video(VideoFrame::decode(body)?)),
            _ => Err(Error::Malformed("unknown frame kind")),
        }
    }

    /// Applies the cipher (if any) and adds the length prefix.
    fn seal(&mut self, plain: &[u8]) -> Result<Vec<u8>> {
        let payload = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(plain)?,
            None => plain.to_vec(),
        };
        frame::encode_payload(&payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::cipher::Role;
    use crate::session::pairing::KEY_LEN;
    use crate::video::{FrameFormat, FrameHeader};

    const MASTER: [u8; KEY_LEN] = [3u8; KEY_LEN];

    fn video_frame() -> VideoFrame {
        VideoFrame::new(
            FrameHeader {
                sequence: 7,
                width: 2,
                height: 1,
                format: FrameFormat::RawRgba,
                timestamp_us: 1234,
            },
            vec![1, 2, 3, 4, 5, 6, 7, 8],
        )
    }

    fn decode(channel: &mut Channel, frame: Vec<u8>) -> Frame {
        channel.decode(&frame[4..]).unwrap()
    }

    #[test]
    fn plaintext_channel_round_trips_a_message() {
        let mut channel = Channel::plaintext();
        assert!(!channel.is_encrypted());
        let frame = channel
            .encode(&SessionMessage::Ping { sent_us: 0 })
            .unwrap();
        assert_eq!(
            decode(&mut channel, frame),
            Frame::Control(Box::new(SessionMessage::Ping { sent_us: 0 }))
        );
    }

    #[test]
    fn plaintext_channel_round_trips_a_video_frame() {
        let mut channel = Channel::plaintext();
        let frame = channel.encode_video(&video_frame()).unwrap();
        assert_eq!(decode(&mut channel, frame), Frame::Video(video_frame()));
    }

    #[test]
    fn encrypted_channel_round_trips_both_kinds_between_roles() {
        let mut client = Channel::plaintext();
        client.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let mut host = Channel::plaintext();
        host.encrypt(SessionCipher::new(&MASTER, Role::Host));

        assert!(client.is_encrypted() && host.is_encrypted());

        let message = client.encode(&SessionMessage::Ping { sent_us: 0 }).unwrap();
        assert_eq!(
            decode(&mut host, message),
            Frame::Control(Box::new(SessionMessage::Ping { sent_us: 0 }))
        );

        // The host is the one that sends video.
        let video = host.encode_video(&video_frame()).unwrap();
        assert_eq!(decode(&mut client, video), Frame::Video(video_frame()));
    }

    #[test]
    fn encryption_hides_the_content() {
        let mut channel = Channel::plaintext();
        channel.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let frame = channel
            .encode(&SessionMessage::Ping { sent_us: 0 })
            .unwrap();
        assert!(!frame.windows(4).any(|w| w == b"ping"));
    }

    #[test]
    fn a_plaintext_peer_cannot_read_ciphertext() {
        let mut sender = Channel::plaintext();
        sender.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let mut plain = Channel::plaintext();
        let frame = sender.encode(&SessionMessage::Ping { sent_us: 0 }).unwrap();
        assert!(plain.decode(&frame[4..]).is_err());
    }

    #[test]
    fn rejects_an_empty_or_unknown_frame() {
        let mut channel = Channel::plaintext();
        assert!(channel.decode(&[]).is_err());
        assert!(channel.decode(&[0x7f, 1, 2, 3]).is_err());
    }
}
