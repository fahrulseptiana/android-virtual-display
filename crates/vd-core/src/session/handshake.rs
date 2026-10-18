//! Control-channel handshake.
//!
//! Pure, message-level state machines for both sides of the handshake. They
//! never touch a socket: they consume and produce [`SessionMessage`]s, so they
//! can be driven by any transport (and unit tested trivially).
//!
//! ```text
//!   ClientHandshake                       HostHandshake
//!     │ hello() ── Hello ─────────────────────▶ on_message()
//!     │                                        Reply(Welcome)
//!     │ on_message(Welcome)                    state == Established
//!     │ Established(Session)                   session() == Some(..)
//! ```

use crate::error::{Error, Result};
use crate::protocol::HostInfo;
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
}

/// Lifecycle position of a handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing exchanged yet.
    Starting,
    /// A message was sent/received; awaiting the peer.
    Awaiting,
    /// Handshake succeeded.
    Established,
    /// Handshake failed (rejected or invalid).
    Failed,
}

/// What the caller should do after feeding a message to a handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this message. Afterwards check [`state`](ClientHandshake::state):
    /// [`State::Established`] means the peer was accepted.
    Reply(SessionMessage),
    /// The handshake completed on this side; the session may start.
    Established(Session),
    /// The peer or the local side rejected the session.
    Rejected(RejectReason),
}

/// Client half of the handshake.
#[derive(Debug)]
pub struct ClientHandshake {
    hello: SessionMessage,
    client: ClientInfo,
    capabilities: Capabilities,
    state: State,
    session: Option<Session>,
}

impl ClientHandshake {
    /// Prepares a handshake; send [`hello`](Self::hello) to begin.
    pub fn new(client: ClientInfo, capabilities: Capabilities) -> Self {
        let hello = SessionMessage::hello(client.clone(), capabilities.clone());
        Self {
            hello,
            client,
            capabilities,
            state: State::Starting,
            session: None,
        }
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
    pub fn on_message(&mut self, message: SessionMessage) -> Result<Action> {
        if self.state != State::Starting {
            return Err(Error::Protocol("client handshake out of order"));
        }

        match message {
            SessionMessage::Welcome {
                protocol,
                host,
                capabilities,
            } => {
                if protocol == 0 || protocol > SESSION_PROTOCOL_VERSION {
                    return Err(Error::UnsupportedVersion {
                        peer: protocol,
                        local: SESSION_PROTOCOL_VERSION,
                    });
                }
                let capabilities = self.capabilities.intersect(&capabilities);
                let session = Session {
                    protocol: protocol.min(SESSION_PROTOCOL_VERSION),
                    host,
                    client: self.client.clone(),
                    codec: capabilities.codecs.first().copied(),
                    capabilities,
                };
                self.state = State::Established;
                self.session = Some(session.clone());
                Ok(Action::Established(session))
            }
            SessionMessage::Reject { reason } => {
                self.state = State::Failed;
                Ok(Action::Rejected(reason))
            }
            SessionMessage::Hello { .. } => Err(Error::Protocol("host sent a hello to a client")),
        }
    }
}

/// Host half of the handshake.
#[derive(Debug)]
pub struct HostHandshake {
    host: HostInfo,
    capabilities: Capabilities,
    state: State,
    session: Option<Session>,
}

impl HostHandshake {
    /// Prepares a handshake that waits for a client's `Hello`.
    pub fn new(host: HostInfo, capabilities: Capabilities) -> Self {
        Self {
            host,
            capabilities,
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
    /// On success returns [`Action::Reply`] carrying the `Welcome`; the session
    /// is then available from [`session`](Self::session). On a version mismatch
    /// it returns [`Action::Reply`] carrying a `Reject` and moves to
    /// [`State::Failed`].
    pub fn on_message(&mut self, message: SessionMessage) -> Result<Action> {
        if self.state != State::Awaiting {
            return Err(Error::Protocol("host handshake out of order"));
        }

        match message {
            SessionMessage::Hello {
                protocol,
                client,
                capabilities,
            } => {
                if protocol == 0 || protocol > SESSION_PROTOCOL_VERSION {
                    self.state = State::Failed;
                    return Ok(Action::Reply(SessionMessage::Reject {
                        reason: RejectReason::ProtocolMismatch,
                    }));
                }

                let agreed = self.capabilities.intersect(&capabilities);
                self.session = Some(Session {
                    protocol: protocol.min(SESSION_PROTOCOL_VERSION),
                    host: self.host.clone(),
                    client,
                    codec: agreed.codecs.first().copied(),
                    capabilities: agreed,
                });
                self.state = State::Established;
                Ok(Action::Reply(SessionMessage::welcome(
                    self.host.clone(),
                    self.capabilities.clone(),
                )))
            }
            SessionMessage::Welcome { .. } | SessionMessage::Reject { .. } => {
                Err(Error::Protocol("client sent a host-only message"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HostId, Platform};
    use crate::session::InputCapabilities;

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

    #[test]
    fn happy_path_agrees_on_a_session() {
        let mut host = HostHandshake::new(host_info(), capabilities(&[VideoCodec::H264], 60));
        let mut client = ClientHandshake::new(client_info(), capabilities(&[VideoCodec::H264], 30));

        let Action::Reply(reply) = host.on_message(client.hello()).unwrap() else {
            panic!("host should reply to a valid hello");
        };
        assert_eq!(host.state(), State::Established);

        let Action::Established(session) = client.on_message(reply).unwrap() else {
            panic!("client should establish the session");
        };
        assert_eq!(client.state(), State::Established);

        assert_eq!(session.host, host_info());
        assert_eq!(session.client, client_info());
        assert_eq!(session.protocol, SESSION_PROTOCOL_VERSION);
        assert_eq!(session.codec, Some(VideoCodec::H264));
        // Intersection: the smaller fps wins.
        assert_eq!(session.capabilities.max_fps, 30);
        // Both sides computed the same session.
        assert_eq!(&session, host.session().unwrap());
    }

    #[test]
    fn negotiates_the_codec_both_sides_share() {
        let mut host = HostHandshake::new(
            host_info(),
            capabilities(&[VideoCodec::H264, VideoCodec::Av1], 60),
        );
        // Client prefers HEVC, then H.264; host has H.264 and AV1.
        let mut client = ClientHandshake::new(
            client_info(),
            capabilities(&[VideoCodec::Hevc, VideoCodec::H264], 60),
        );

        let Action::Reply(reply) = host.on_message(client.hello()).unwrap() else {
            panic!("expected welcome");
        };
        let Action::Established(session) = client.on_message(reply).unwrap() else {
            panic!("expected session");
        };
        assert_eq!(session.codec, Some(VideoCodec::H264));
    }

    #[test]
    fn rejects_an_incompatible_protocol() {
        let mut host = HostHandshake::new(host_info(), Capabilities::default());
        let hello = SessionMessage::Hello {
            protocol: SESSION_PROTOCOL_VERSION + 1,
            client: client_info(),
            capabilities: Capabilities::default(),
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
    fn client_surfaces_a_rejection() {
        let mut client = ClientHandshake::new(client_info(), Capabilities::default());
        let action = client
            .on_message(SessionMessage::Reject {
                reason: RejectReason::Unauthorized,
            })
            .unwrap();
        assert_eq!(action, Action::Rejected(RejectReason::Unauthorized));
        assert_eq!(client.state(), State::Failed);
    }

    #[test]
    fn out_of_order_messages_are_rejected() {
        let mut host = HostHandshake::new(host_info(), Capabilities::default());
        let err = host
            .on_message(SessionMessage::Reject {
                reason: RejectReason::Busy,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }
}
