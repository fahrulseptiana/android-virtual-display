//! Pairing: an ephemeral X25519 exchange, a short authentication string (the
//! PIN) and key confirmation.
//!
//! Both peers perform an ephemeral Diffie–Hellman exchange and derive the same
//! session key. A six-digit **PIN** is derived from the same secret: it is shown
//! to the client's user, who confirms it before the session starts. The host
//! auto-approves. The PIN doubles as a short authentication string — comparing
//! it out of band is what defeats a man-in-the-middle.
//!
//! Nothing here touches the network; the handshake state machines carry the
//! values over the control channel.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// Length of an X25519 public key / shared secret.
pub const KEY_LEN: usize = 32;
/// Length of the key-derivation salt.
pub const SALT_LEN: usize = 16;
/// The PIN is a `PIN_MODULUS`-way value rendered as six digits.
pub const PIN_MODULUS: u32 = 1_000_000;

/// An ephemeral X25519 key pair.
///
/// The secret never leaves this struct; only [`EphemeralKey::public`] is sent.
#[derive(Clone)]
pub struct EphemeralKey {
    secret: [u8; KEY_LEN],
    public: [u8; KEY_LEN],
}

impl EphemeralKey {
    /// Generates a fresh key pair from the operating system's entropy source.
    pub fn generate() -> Result<Self> {
        let mut secret = [0u8; KEY_LEN];
        getrandom::fill(&mut secret).map_err(|_| Error::Entropy)?;
        Ok(Self::from_secret(secret))
    }

    /// Builds a key pair from a raw secret (useful for tests).
    pub fn from_secret(secret: [u8; KEY_LEN]) -> Self {
        let public = x25519_dalek::x25519(secret, x25519_dalek::X25519_BASEPOINT_BYTES);
        Self { secret, public }
    }

    /// The public half, safe to send to a peer.
    pub fn public(&self) -> [u8; KEY_LEN] {
        self.public
    }

    /// The shared secret with a peer.
    pub fn shared_secret(&self, peer_public: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
        x25519_dalek::x25519(self.secret, *peer_public)
    }
}

impl std::fmt::Debug for EphemeralKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret.
        f.debug_struct("EphemeralKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// Generates a fresh salt for the key derivation.
pub fn generate_salt() -> Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|_| Error::Entropy)?;
    Ok(salt)
}

/// The session key and PIN derived from a shared secret.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingKeys {
    /// Symmetric key for encrypting the session.
    pub key: [u8; KEY_LEN],
    /// Short authentication string, shown to the client's user.
    pub pin: u32,
}

impl std::fmt::Debug for PairingKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingKeys")
            .field("key", &"<redacted>")
            .field("pin", &self.pin)
            .finish()
    }
}

impl PairingKeys {
    /// Derives the session key and PIN from a shared secret and public salt.
    pub fn derive(shared: &[u8; KEY_LEN], salt: &[u8; SALT_LEN]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), shared);

        let mut key = [0u8; KEY_LEN];
        hkdf.expand(b"vd-control key v1", &mut key)
            .expect("32 bytes is a valid HKDF output length");

        let mut pin_bytes = [0u8; 4];
        hkdf.expand(b"vd-control pin v1", &mut pin_bytes)
            .expect("4 bytes is a valid HKDF output length");

        Self {
            key,
            pin: u32::from_be_bytes(pin_bytes) % PIN_MODULUS,
        }
    }

    /// The PIN rendered as six zero-padded digits.
    pub fn pin_display(&self) -> String {
        format!("{:06}", self.pin)
    }

    /// Key-confirmation tag over the handshake transcript.
    pub fn confirm_tag(&self, transcript: &[u8]) -> [u8; KEY_LEN] {
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC accepts keys of any length");
        mac.update(b"vd-control confirm v1");
        mac.update(transcript);
        mac.finalize().into_bytes().into()
    }

    /// Constant-time verification of a peer's confirmation tag.
    pub fn verify_tag(&self, transcript: &[u8], tag: &[u8; KEY_LEN]) -> bool {
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC accepts keys of any length");
        mac.update(b"vd-control confirm v1");
        mac.update(transcript);
        mac.verify_slice(tag).is_ok()
    }
}

/// The canonical transcript both peers hash for key confirmation.
///
/// Order matters and is fixed: protocol, client public key, host public key,
/// salt.
pub fn transcript(
    protocol: u16,
    client_public: &[u8; KEY_LEN],
    host_public: &[u8; KEY_LEN],
    salt: &[u8; SALT_LEN],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + 2 * KEY_LEN + SALT_LEN);
    bytes.extend_from_slice(&protocol.to_be_bytes());
    bytes.extend_from_slice(client_public);
    bytes.extend_from_slice(host_public);
    bytes.extend_from_slice(salt);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange() -> (EphemeralKey, EphemeralKey, [u8; KEY_LEN], [u8; SALT_LEN]) {
        let client = EphemeralKey::generate().unwrap();
        let host = EphemeralKey::generate().unwrap();
        let salt = generate_salt().unwrap();
        let shared = client.shared_secret(&host.public());
        (client, host, shared, salt)
    }

    #[test]
    fn both_sides_derive_the_same_secret() {
        let (client, host, shared, _) = exchange();
        assert_eq!(shared, host.shared_secret(&client.public()));
    }

    #[test]
    fn both_sides_derive_the_same_key_and_pin() {
        let (client, host, shared, salt) = exchange();
        let from_client = PairingKeys::derive(&shared, &salt);
        let from_host = PairingKeys::derive(&host.shared_secret(&client.public()), &salt);

        assert_eq!(from_client, from_host);
        assert!(from_client.pin < PIN_MODULUS);
        assert_eq!(from_client.pin_display().len(), 6);
    }

    #[test]
    fn distinct_salts_give_distinct_keys() {
        let (_, _, shared, salt) = exchange();
        let mut other_salt = salt;
        other_salt[0] ^= 0xff;
        assert_ne!(
            PairingKeys::derive(&shared, &salt).key,
            PairingKeys::derive(&shared, &other_salt).key
        );
    }

    #[test]
    fn confirmation_tag_round_trips() {
        let (client, host, shared, salt) = exchange();
        let keys = PairingKeys::derive(&shared, &salt);
        let tr = transcript(1, &client.public(), &host.public(), &salt);
        let tag = keys.confirm_tag(&tr);

        assert!(keys.verify_tag(&tr, &tag));
    }

    #[test]
    fn confirmation_tag_rejects_tampering() {
        let (client, host, shared, salt) = exchange();
        let keys = PairingKeys::derive(&shared, &salt);
        let tr = transcript(1, &client.public(), &host.public(), &salt);
        let tag = keys.confirm_tag(&tr);

        // A different transcript (e.g. an attacker's public key) fails.
        let other = transcript(1, &host.public(), &client.public(), &salt);
        assert!(!keys.verify_tag(&other, &tag));

        // A different key fails.
        let other_keys = PairingKeys::derive(&[7u8; KEY_LEN], &salt);
        assert!(!other_keys.verify_tag(&tr, &tag));
    }
}
