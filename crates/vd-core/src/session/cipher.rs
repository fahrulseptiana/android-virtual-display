//! Authenticated encryption for an established session.
//!
//! Once pairing has produced a shared key, every control message is sealed with
//! ChaCha20-Poly1305. Each direction uses its own key (derived from the master
//! key with a distinct label) and its own monotonically increasing nonce, so a
//! nonce can never repeat for a key.
//!
//! ```text
//!   master key ─┬─ HKDF "c2h" ─▶ client→host key
//!               └─ HKDF "h2c" ─▶ host→client key
//! ```

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::session::pairing::KEY_LEN;

const NONCE_LEN: usize = 12;
const CLIENT_TO_HOST: &[u8] = b"vd-control c2h v1";
const HOST_TO_CLIENT: &[u8] = b"vd-control h2c v1";

/// Which end of the session a cipher belongs to. Decides which derived key is
/// used for sending and which for receiving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Host,
}

/// Seals and opens session messages.
pub struct SessionCipher {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_counter: u64,
    recv_counter: u64,
}

impl std::fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCipher")
            .field("send_counter", &self.send_counter)
            .field("recv_counter", &self.recv_counter)
            .finish_non_exhaustive()
    }
}

impl SessionCipher {
    /// Builds a cipher for `role` from the master key agreed during pairing.
    pub fn new(master: &[u8; KEY_LEN], role: Role) -> Self {
        let (send_info, recv_info) = match role {
            Role::Client => (CLIENT_TO_HOST, HOST_TO_CLIENT),
            Role::Host => (HOST_TO_CLIENT, CLIENT_TO_HOST),
        };
        Self {
            send: ChaCha20Poly1305::new_from_slice(&derive(master, send_info))
                .expect("a 32-byte key is always valid"),
            recv: ChaCha20Poly1305::new_from_slice(&derive(master, recv_info))
                .expect("a 32-byte key is always valid"),
            send_counter: 0,
            recv_counter: 0,
        }
    }

    /// Encrypts a payload for the peer.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.send_counter);
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or(Error::Crypto("nonce space exhausted"))?;
        self.send
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| Error::Crypto("could not seal the message"))
    }

    /// Decrypts a payload from the peer, failing on tampering.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.recv_counter);
        self.recv_counter = self
            .recv_counter
            .checked_add(1)
            .ok_or(Error::Crypto("nonce space exhausted"))?;
        self.recv
            .decrypt(Nonce::from_slice(&nonce), ciphertext)
            .map_err(|_| Error::Crypto("could not open the message"))
    }
}

/// Derives a direction key from the master key.
fn derive(master: &[u8; KEY_LEN], info: &[u8]) -> [u8; KEY_LEN] {
    let hkdf = Hkdf::<Sha256>::new(None, master);
    let mut key = [0u8; KEY_LEN];
    hkdf.expand(info, &mut key)
        .expect("32 bytes is a valid HKDF output length");
    key
}

/// A 96-bit nonce: four zero bytes and a big-endian counter.
fn nonce(counter: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[NONCE_LEN - 8..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: [u8; KEY_LEN] = [7u8; KEY_LEN];

    fn pair() -> (SessionCipher, SessionCipher) {
        (
            SessionCipher::new(&MASTER, Role::Client),
            SessionCipher::new(&MASTER, Role::Host),
        )
    }

    #[test]
    fn round_trips_in_both_directions() {
        let (mut client, mut host) = pair();

        let from_client = client.seal(b"hello host").unwrap();
        assert_eq!(host.open(&from_client).unwrap(), b"hello host");

        let from_host = host.seal(b"hello client").unwrap();
        assert_eq!(client.open(&from_host).unwrap(), b"hello client");
    }

    #[test]
    fn ciphertext_is_not_plaintext() {
        let (mut client, _) = pair();
        let sealed = client.seal(b"secret").unwrap();
        assert_ne!(sealed, b"secret");
        assert!(sealed.len() > b"secret".len(), "tag must be appended");
    }

    #[test]
    fn the_same_plaintext_seals_differently_each_time() {
        let (mut client, _) = pair();
        assert_ne!(client.seal(b"same").unwrap(), client.seal(b"same").unwrap());
    }

    #[test]
    fn rejects_tampering() {
        let (mut client, mut host) = pair();
        let mut sealed = client.seal(b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(host.open(&sealed).is_err());
    }

    #[test]
    fn rejects_a_different_master_key() {
        let mut client = SessionCipher::new(&MASTER, Role::Client);
        let mut stranger = SessionCipher::new(&[9u8; KEY_LEN], Role::Host);
        let sealed = client.seal(b"hello").unwrap();
        assert!(stranger.open(&sealed).is_err());
    }

    #[test]
    fn a_client_and_a_client_cannot_talk() {
        // Using the same role on both ends means mismatched keys.
        let mut a = SessionCipher::new(&MASTER, Role::Client);
        let mut b = SessionCipher::new(&MASTER, Role::Client);
        let sealed = a.seal(b"hello").unwrap();
        assert!(b.open(&sealed).is_err());
    }
}
