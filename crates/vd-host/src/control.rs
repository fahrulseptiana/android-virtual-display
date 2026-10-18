//! Control channel server.
//!
//! Accepts TCP connections on the host's [`control_port`](vd_core::HostInfo::control_port)
//! and runs the [`HostHandshake`] against each of them. Each connection is
//! handled on its own thread: the handshake is short, and a slow or hostile
//! client must not stall the discovery loop.
//!
//! The `Welcome` is sent in the clear (the client needs it to derive the key);
//! everything after it is sealed with the session cipher. Once a session is
//! established the same connection also carries video frames from the
//! [`Broadcaster`].

use std::io::{Error as IoError, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use vd_core::session::channel::{Channel, Frame};
use vd_core::session::cipher::{Role, SessionCipher};
use vd_core::session::frame::FrameDecoder;
use vd_core::session::handshake::{Action, HostHandshake};
use vd_core::video::VideoFrame;
use vd_core::{Capabilities, HostInfo, Session, SessionMessage};

use crate::video::Broadcaster;

/// Read buffer for a single `recv`.
const READ_BUFFER: usize = 4096;

/// How long a serving connection blocks in `read` before it gets a chance to
/// flush pending video frames.
const VIDEO_POLL: Duration = Duration::from_millis(10);

/// Listens for, handshakes and streams to control connections.
pub struct ControlServer {
    listener: TcpListener,
    host: HostInfo,
    capabilities: Capabilities,
    video: Arc<Broadcaster>,
}

impl ControlServer {
    /// Binds the control port advertised in discovery.
    pub fn bind(
        host: HostInfo,
        capabilities: Capabilities,
        video: Arc<Broadcaster>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", host.control_port))?;
        // Non-blocking so the host's discovery loop is never held up.
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            host,
            capabilities,
            video,
        })
    }

    /// The address the control channel is listening on.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts every pending connection, handing each to a worker thread.
    pub fn poll(&self) -> std::io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let host = self.host.clone();
                    let capabilities = self.capabilities.clone();
                    let video = self.video.clone();
                    thread::spawn(move || {
                        if let Err(err) = serve(stream, peer, host, capabilities, video) {
                            warn!("control connection from {peer} ended with error: {err}");
                        }
                    });
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }
}

/// Runs the handshake for one connection, then serves control and video until
/// the client leaves.
fn serve(
    mut stream: TcpStream,
    peer: SocketAddr,
    host: HostInfo,
    capabilities: Capabilities,
    video: Arc<Broadcaster>,
) -> Result<(), Box<dyn std::error::Error>> {
    // On Windows an accepted socket inherits the listener's non-blocking mode,
    // so the worker explicitly asks for blocking reads.
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    debug!("control connection from {peer}");

    let mut handshake = HostHandshake::new(host, capabilities);
    let mut decoder = FrameDecoder::new();
    let mut channel = Channel::plaintext();
    let mut buffer = [0u8; READ_BUFFER];
    let mut established = false;
    let mut video_rx: Option<Receiver<Arc<VideoFrame>>> = None;

    loop {
        // Flush any video the capture thread produced since the last lap.
        if let Some(receiver) = video_rx.as_ref() {
            while let Ok(frame) = receiver.try_recv() {
                match channel.encode_video(&frame) {
                    Ok(bytes) => {
                        if let Err(err) = stream.write_all(&bytes) {
                            if is_disconnect(&err) {
                                return Ok(());
                            }
                            return Err(err.into());
                        }
                    }
                    Err(err) => warn!("could not encode a video frame for {peer}: {err}"),
                }
            }
        }

        let read = match stream.read(&mut buffer) {
            Ok(0) => {
                debug!("control connection from {peer} closed");
                return Ok(());
            }
            Ok(read) => read,
            // No control traffic this lap; loop round to send video.
            Err(err) if is_read_timeout(&err) => continue,
            Err(err) if is_disconnect(&err) => {
                debug!("control connection from {peer} went away: {err}");
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        decoder.push(&buffer[..read]);

        loop {
            let payload = match decoder.next_payload() {
                Ok(Some(payload)) => payload,
                Ok(None) => break,
                Err(err) => return Err(err.into()),
            };
            let message = match channel.decode(&payload) {
                Ok(Frame::Control(message)) => message,
                // Clients never send video; ignore it rather than failing.
                Ok(Frame::Video(_)) => continue,
                Err(err) => {
                    warn!("discarding message from {peer}: {err}");
                    return Ok(());
                }
            };

            if established {
                match message {
                    SessionMessage::Ping => {
                        debug!("pong to {peer}");
                        let frame = channel.encode(&SessionMessage::Pong)?;
                        if let Err(err) = stream.write_all(&frame) {
                            if is_disconnect(&err) {
                                return Ok(());
                            }
                            return Err(err.into());
                        }
                    }
                    // Later steps: input events are handled here.
                    other => debug!("ignoring post-handshake message from {peer}: {other:?}"),
                }
                continue;
            }

            let action = handshake.on_message(message)?;
            let Action::Reply(reply) = action else {
                return Err(IoError::new(ErrorKind::InvalidData, "unexpected host action").into());
            };

            // The Welcome is plaintext: the client needs it to derive the key.
            let frame = channel.encode(&reply)?;
            if let Err(err) = stream.write_all(&frame) {
                if is_disconnect(&err) {
                    return Ok(());
                }
                return Err(err.into());
            }

            match handshake.session() {
                Some(session) => {
                    channel.encrypt(SessionCipher::new(&session.keys.key, Role::Host));
                    established = true;
                    video_rx = Some(video.subscribe());
                    // Wake up regularly to send video.
                    stream.set_read_timeout(Some(VIDEO_POLL))?;
                    log_session(session, peer);
                }
                None => {
                    info!("rejected control connection from {peer}");
                    return Ok(());
                }
            }
        }
    }
}

/// A client that vanished without a clean shutdown is not an error.
fn is_disconnect(err: &IoError) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
    )
}

/// A read that simply had nothing to report this lap.
fn is_read_timeout(err: &IoError) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn log_session(session: &Session, peer: SocketAddr) {
    info!(
        "session established with \"{}\" ({}) from {peer} — protocol v{}, codec {}, encrypted",
        session.client.name,
        session.client.platform,
        session.protocol,
        session
            .codec
            .map(|codec| format!("{codec:?}"))
            .unwrap_or_else(|| "none".to_owned()),
    );
}
