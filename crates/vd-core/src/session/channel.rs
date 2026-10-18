//! Message transport for the control channel.
//!
//! A [`Channel`] turns [`SessionMessage`]s into framed bytes and back. It starts
//! in plaintext (the handshake needs to be readable) and switches to
//! authenticated encryption once pairing has produced a key.

use crate::error::{Error, Result};
use crate::session::cipher::SessionCipher;
use crate::session::frame;
use crate::session::SessionMessage;

/// Encodes and decodes session messages, optionally sealed.
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

    /// Encodes a message into a complete, length-prefixed frame.
    pub fn encode(&mut self, message: &SessionMessage) -> Result<Vec<u8>> {
        let json =
            serde_json::to_vec(message).map_err(|_| Error::Malformed("session json encode"))?;
        let payload = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(&json)?,
            None => json,
        };
        frame::encode_payload(&payload)
    }

    /// Decodes one frame payload into a message.
    pub fn decode(&mut self, payload: &[u8]) -> Result<SessionMessage> {
        let plain = match self.cipher.as_mut() {
            Some(cipher) => cipher.open(payload)?,
            None => payload.to_vec(),
        };
        serde_json::from_slice(&plain).map_err(|_| Error::Malformed("session json decode"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::cipher::Role;
    use crate::session::pairing::KEY_LEN;

    const MASTER: [u8; KEY_LEN] = [3u8; KEY_LEN];

    #[test]
    fn plaintext_channel_round_trips() {
        let mut channel = Channel::plaintext();
        assert!(!channel.is_encrypted());
        let frame = channel.encode(&SessionMessage::Ping).unwrap();
        let payload = frame[4..].to_vec();
        assert_eq!(channel.decode(&payload).unwrap(), SessionMessage::Ping);
    }

    #[test]
    fn encrypted_channel_round_trips_between_roles() {
        let mut client = Channel::plaintext();
        client.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let mut host = Channel::plaintext();
        host.encrypt(SessionCipher::new(&MASTER, Role::Host));

        assert!(client.is_encrypted() && host.is_encrypted());

        let frame = client.encode(&SessionMessage::Ping).unwrap();
        assert!(host.decode(&frame[4..]).is_ok());

        let frame = host.encode(&SessionMessage::Pong).unwrap();
        assert_eq!(client.decode(&frame[4..]).unwrap(), SessionMessage::Pong);
    }

    #[test]
    fn encryption_hides_the_payload() {
        let mut channel = Channel::plaintext();
        channel.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let frame = channel.encode(&SessionMessage::Ping).unwrap();
        assert!(!frame.windows(4).any(|w| w == b"ping"));
    }

    #[test]
    fn a_plaintext_peer_cannot_read_ciphertext() {
        let mut sender = Channel::plaintext();
        sender.encrypt(SessionCipher::new(&MASTER, Role::Client));
        let mut plain = Channel::plaintext();
        let frame = sender.encode(&SessionMessage::Ping).unwrap();
        assert!(plain.decode(&frame[4..]).is_err());
    }
}
