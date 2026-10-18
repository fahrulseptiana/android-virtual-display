//! Control-channel handshake.
//!
//! Pure, message-level state machines for both sides. They never touch a
//! socket: they consume and produce [`SessionMessage`]s, so any transport can
//! drive them and tests need no network.
//!
//! ```text
//!   ClientHandshake                         HostHandshake
//!     │ hello() ── Hello { client_public } ────▶ on_message()
//!     │                                          derives keys
//!     │  on_message(Welcome { host_public,   ◀── Reply(Welcome)
//!     │             salt, tag })
//!     │  derives keys, verifies the host tag
//!     │  AwaitingConfirmation { pin }           state == Established
//!     │  confirm()  (the user approves the PIN)
//!     │  Established(Session)                   session() == Some(..)
//! ```
//!
//! The host auto-approves: it establishes the session as soon as it receives a
//! valid `Hello`. Only the client asks its user to confirm, via
//! [`ClientHandshake::confirm`].

use crate::error::{Error, Result};
use crate::protocol::{HostId, HostInfo};
use crate::session::pairing::{self, EphemeralKey, PairingKeys, KEY_LEN};
use crate::session::{
    Capabilities, ClientInfo, RejectReason, SessionMessage, VideoCodec, SESSION_PROTOCOL_VERSION,
};

/// What the two peers agreed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Negotiated protocol version (the lower of the two).
    pub protocol: u16,
    /// The host's identity.
    pub host: HostInfo,
    /// The client's identity.
    pub client: ClientInfo,
    /// Preferred codec both sides support, if any.
    pub codec: Option<VideoCodec>,
    /// The intersection of both peers' capabilities.
    pub capabilities: Capabilities,
    /// Session key and PIN agreed during pairing.
    pub keys: PairingKeys,
}

/// Lifecycle position of a handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing exchanged yet.
    Starting,
    /// A message was sent/received; awaiting the peer or the user.
    Awaiting,
    /// Handshake succeeded.
    Established,
    /// Handshake failed (rejected or invalid).
    Failed,
}

/// What the caller should do after feeding a message to a handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this message. Afterwards check the state: [`State::Established`]
    /// means the peer was accepted.
    Reply(SessionMessage),
    /// The client must ask its user to confirm the PIN. Call
    /// [`ClientHandshake::confirm`] once they approve (or drop the connection
    /// if they don't).
    AwaitingConfirmation { pin: u32 },
    /// The handshake completed on this side; the session may start.
    Established(Session),
    /// The peer or the local side rejected the session.
    Rejected(RejectReason),
}

/// Client half of the handshake.
#[derive(Debug)]
pub struct ClientHandshake {
    key: EphemeralKey,
    hello: SessionMessage,
    client: ClientInfo,
    capabilities: Capabilities,
    pending: Option<Pending>,
    state: State,
    session: Option<Session>,
}

/// Everything derived from a `Welcome`, held until the user confirms.
#[derive(Debug)]
struct Pending {
    protocol: u16,
    host: HostInfo,
    agreed: Capabilities,
    keys: PairingKeys,
    /// The host's long-lived identity key, for the caller's trust decision.
    identity_public: [u8; KEY_LEN],
}

impl ClientHandshake {
    /// Prepares a handshake, generating an ephemeral key. Send
    /// [`hello`](Self::hello) to begin.
    pub fn new(client: ClientInfo, capabilities: Capabilities) -> Result<Self> {
        let key = EphemeralKey::generate()?;
        let hello = SessionMessage::hello(client.clone(), capabilities.clone(), key.public());
        Ok(Self {
            key,
            hello,
            client,
            capabilities,
            pending: None,
            state: State::Starting,
            session: None,
        })
    }

    /// The first message to send to the host.
    pub fn hello(&self) -> SessionMessage {
        self.hello.clone()
    }

    /// Current lifecycle position.
    pub fn state(&self) -> State {
        self.state
    }

    /// The established session, once [`State::Established`].
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Feeds a message from the host.
    ///
    /// On a valid `Welcome` this returns [`Action::AwaitingConfirmation`]; the
    /// caller must then ask the user and call [`confirm`](Self::confirm).
    pub fn on_message(&mut self, message: SessionMessage) -> Result<Action> {
        if self.state != State::Starting {
            return Err(Error::Protocol("client handshake out of order"));
        }

        match message {
            SessionMessage::Welcome {
                protocol,
                host,
                capabilities,
                host_public,
                identity_public,
                salt,
                tag,
            } => {
                if protocol == 0 || protocol > SESSION_PROTOCOL_VERSION {
                    return Err(Error::UnsupportedVersion {
                        peer: protocol,
                        local: SESSION_PROTOCOL_VERSION,
                    });
                }

                // The second secret can only be computed by a host holding the
                // identity key it just claimed, which is what authenticates it.
                let shared = self.key.shared_secret(&host_public);
                let authenticated = self.key.shared_secret(&identity_public);
                let keys = PairingKeys::derive(&shared, &authenticated, &salt);
                let transcript =
                    pairing::transcript(protocol, &self.key.public(), &host_public, &salt);
                if !keys.verify_tag(&transcript, &tag) {
                    self.state = State::Failed;
                    return Ok(Action::Rejected(RejectReason::Unauthorized));
                }

                let pin = keys.pin;
                self.pending = Some(Pending {
                    protocol,
                    host,
                    agreed: self.capabilities.intersect(&capabilities),
                    keys,
                    identity_public,
                });
                self.state = State::Awaiting;
                Ok(Action::AwaitingConfirmation { pin })
            }
            SessionMessage::Reject { reason } => {
                self.state = State::Failed;
                Ok(Action::Rejected(reason))
            }
            SessionMessage::Hello { .. }
            | SessionMessage::VideoConfig { .. }
            | SessionMessage::VideoControl { .. }
            | SessionMessage::Ping { .. }
            | SessionMessage::Pong { .. } => {
                Err(Error::Protocol("host sent a client-only message"))
            }
        }
    }

    /// The host identity offered in the `Welcome`, for the trust decision.
    pub fn host_identity(&self) -> Option<&[u8; KEY_LEN]> {
        self.pending
            .as_ref()
            .map(|pending| &pending.identity_public)
    }

    /// The host's id, once a `Welcome` has been received.
    pub fn host_id(&self) -> Option<&HostId> {
        self.pending.as_ref().map(|pending| &pending.host.id)
    }

    /// Confirms the pairing after the user approved the PIN.
    pub fn confirm(&mut self) -> Result<Action> {
        if self.state != State::Awaiting {
            return Err(Error::Protocol("nothing to confirm"));
        }
        let pending = self
            .pending
            .take()
            .ok_or(Error::Protocol("nothing to confirm"))?;

        let session = Session {
            protocol: pending.protocol.min(SESSION_PROTOCOL_VERSION),
            host: pending.host,
            client: self.client.clone(),
            codec: pending.agreed.codecs.first().copied(),
            capabilities: pending.agreed,
            keys: pending.keys,
        };
        self.state = State::Established;
        self.session = Some(session.clone());
        Ok(Action::Established(session))
    }
}

/// Host half of the handshake.
#[derive(Debug)]
pub struct HostHandshake {
    host: HostInfo,
    capabilities: Capabilities,
    /// Long-lived identity secret; the matching public key is published in
    /// `Welcome` and is what clients remember.
    identity_secret: [u8; KEY_LEN],
    state: State,
    session: Option<Session>,
}

impl HostHandshake {
    /// Prepares a handshake that waits for a client's `Hello`.
    pub fn new(host: HostInfo, capabilities: Capabilities, identity_secret: [u8; KEY_LEN]) -> Self {
        Self {
            host,
            capabilities,
            identity_secret,
            state: State::Awaiting,
            session: None,
        }
    }

    /// Current lifecycle position.
    pub fn state(&self) -> State {
        self.state
    }

    /// The established session, once [`State::Established`].
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Feeds a message from the client.
    ///
    /// The host auto-approves: a valid `Hello` is answered with a `Welcome`
    /// carrying the host's ephemeral key and a key-confirmation tag, and the
    /// session is established immediately.
    pub fn on_message(&mut self, message: SessionMessage) -> Result<Action> {
        if self.state != State::Awaiting {
            return Err(Error::Protocol("host handshake out of order"));
        }

        match message {
            SessionMessage::Hello {
                protocol,
                client,
                capabilities,
                client_public,
            } => {
                if protocol == 0 || protocol > SESSION_PROTOCOL_VERSION {
                    self.state = State::Failed;
                    return Ok(Action::Reply(SessionMessage::Reject {
                        reason: RejectReason::ProtocolMismatch,
                    }));
                }

                let key = EphemeralKey::generate()?;
                let salt = pairing::generate_salt()?;
                let shared = key.shared_secret(&client_public);
                // DH against our own identity key: the client computes the same
                // value from our published public key and its ephemeral secret.
                let authenticated =
                    pairing::shared_with_secret(&self.identity_secret, &client_public);
                let keys = PairingKeys::derive(&shared, &authenticated, &salt);
                let transcript =
                    pairing::transcript(protocol, &client_public, &key.public(), &salt);
                let tag = keys.confirm_tag(&transcript);

                let agreed = self.capabilities.intersect(&capabilities);
                self.session = Some(Session {
                    protocol: protocol.min(SESSION_PROTOCOL_VERSION),
                    host: self.host.clone(),
                    client,
                    codec: agreed.codecs.first().copied(),
                    capabilities: agreed,
                    keys,
                });
                self.state = State::Established;

                Ok(Action::Reply(SessionMessage::welcome(
                    self.host.clone(),
                    self.capabilities.clone(),
                    key.public(),
                    pairing::public_from_secret(&self.identity_secret),
                    salt,
                    tag,
                )))
            }
            SessionMessage::Welcome { .. }
            | SessionMessage::Reject { .. }
            | SessionMessage::VideoConfig { .. } => {
                Err(Error::Protocol("client sent a host-only message"))
            }
            // Client-to-host traffic, which belongs to an established session
            // and has no meaning while one is still being negotiated.
            SessionMessage::VideoControl { .. }
            | SessionMessage::Ping { .. }
            | SessionMessage::Pong { .. } => Err(Error::Protocol(
                "session message before the handshake finished",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HostId, Platform};
    use crate::session::pairing::KEY_LEN;
    use crate::session::InputCapabilities;

    /// A host handshake with a fixed identity key.
    fn host_handshake(codecs: &[VideoCodec], fps: u16) -> HostHandshake {
        HostHandshake::new(host_info(), capabilities(codecs, fps), [7u8; KEY_LEN])
    }

    fn host_info() -> HostInfo {
        HostInfo::new(HostId::from("host-1"), "DESKTOP")
    }

    fn client_info() -> ClientInfo {
        ClientInfo {
            name: "Pixel".to_owned(),
            platform: Platform::Android,
            app_version: "0.1.0".to_owned(),
        }
    }

    fn capabilities(codecs: &[VideoCodec], fps: u16) -> Capabilities {
        Capabilities {
            codecs: codecs.to_vec(),
            max_width: 1920,
            max_height: 1200,
            max_fps: fps,
            input: InputCapabilities {
                pointer: true,
                keyboard: true,
            },
        }
    }

    /// Drives a host to accept the client's hello, returning the reply and the
    /// host's session.
    fn accept(host: &mut HostHandshake, client: &ClientHandshake) -> (SessionMessage, Session) {
        let Action::Reply(reply) = host.on_message(client.hello()).unwrap() else {
            panic!("host should reply to a valid hello");
        };
        (reply, host.session().unwrap().clone())
    }

    #[test]
    fn happy_path_agrees_on_a_session_and_key() {
        let mut host = host_handshake(&[VideoCodec::H264], 60);
        let mut client =
            ClientHandshake::new(client_info(), capabilities(&[VideoCodec::H264], 30)).unwrap();

        let (reply, host_session) = accept(&mut host, &client);

        let Action::AwaitingConfirmation { pin } = client.on_message(reply).unwrap() else {
            panic!("client should await confirmation");
        };
        assert_eq!(pin, host_session.keys.pin);
        assert!(pin < crate::session::pairing::PIN_MODULUS);

        let Action::Established(session) = client.confirm().unwrap() else {
            panic!("client should establish after confirming");
        };
        assert_eq!(client.state(), State::Established);

        // Both sides derived the same session key.
        assert_eq!(session.keys.key, host_session.keys.key);
        assert_eq!(session.host, host_info());
        assert_eq!(session.client, client_info());
        assert_eq!(session.protocol, SESSION_PROTOCOL_VERSION);
        assert_eq!(session.codec, Some(VideoCodec::H264));
        // Intersection: the smaller fps wins.
        assert_eq!(session.capabilities.max_fps, 30);
    }

    #[test]
    fn negotiates_the_codec_both_sides_share() {
        let mut host = host_handshake(&[VideoCodec::H264, VideoCodec::Av1], 60);
        let mut client = ClientHandshake::new(
            client_info(),
            capabilities(&[VideoCodec::Hevc, VideoCodec::H264], 60),
        )
        .unwrap();

        let (reply, _) = accept(&mut host, &client);
        let Action::AwaitingConfirmation { .. } = client.on_message(reply).unwrap() else {
            panic!("expected confirmation prompt");
        };
        let Action::Established(session) = client.confirm().unwrap() else {
            panic!("expected session");
        };
        assert_eq!(session.codec, Some(VideoCodec::H264));
    }

    #[test]
    fn client_rejects_a_bad_host_tag() {
        let mut host = host_handshake(&[VideoCodec::H264], 60);
        let mut client =
            ClientHandshake::new(client_info(), capabilities(&[VideoCodec::H264], 60)).unwrap();

        let (reply, _) = accept(&mut host, &client);
        let SessionMessage::Welcome {
            protocol,
            host: host_info,
            capabilities,
            host_public,
            identity_public,
            salt,
            ..
        } = reply
        else {
            panic!("expected welcome");
        };
        let tampered = SessionMessage::Welcome {
            protocol,
            host: host_info,
            capabilities,
            host_public,
            identity_public,
            salt,
            tag: [0u8; KEY_LEN],
        };

        let action = client.on_message(tampered).unwrap();
        assert_eq!(action, Action::Rejected(RejectReason::Unauthorized));
        assert_eq!(client.state(), State::Failed);
    }

    #[test]
    fn rejects_an_incompatible_protocol() {
        let mut host = HostHandshake::new(host_info(), Capabilities::default(), [7u8; KEY_LEN]);
        let hello = SessionMessage::Hello {
            protocol: SESSION_PROTOCOL_VERSION + 1,
            client: client_info(),
            capabilities: Capabilities::default(),
            client_public: [0u8; KEY_LEN],
        };

        let Action::Reply(SessionMessage::Reject { reason }) = host.on_message(hello).unwrap()
        else {
            panic!("expected reject");
        };
        assert_eq!(reason, RejectReason::ProtocolMismatch);
        assert_eq!(host.state(), State::Failed);
        assert!(host.session().is_none());
    }

    #[test]
    fn cannot_confirm_before_the_welcome() {
        let mut client = ClientHandshake::new(client_info(), Capabilities::default()).unwrap();
        assert!(client.confirm().is_err());
    }

    #[test]
    fn out_of_order_messages_are_rejected() {
        let mut host = HostHandshake::new(host_info(), Capabilities::default(), [7u8; KEY_LEN]);
        let err = host
            .on_message(SessionMessage::Reject {
                reason: RejectReason::Busy,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }
}
