//! Client-side control connection.
//!
//! Owns the whole lifecycle of a session: TCP connect, handshake, pairing
//! confirmation, heartbeats, and reconnection with exponential backoff.
//! Everything runs on a dedicated thread and reports progress to the UI over a
//! channel, so the egui repaint loop never blocks on network I/O.
//!
//! Pairing needs a decision from the user, which flows the other way: the
//! worker emits [`ConnectionEvent::PairingRequired`] and then blocks until the
//! UI calls [`ConnectionHandle::decide_pairing`].

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use vd_core::codec::decode_rgba;
use vd_core::session::channel::{Channel, Frame};
use vd_core::session::cipher::{Role, SessionCipher};
use vd_core::session::frame::FrameDecoder;
use vd_core::session::handshake::{Action, ClientHandshake};
use vd_core::video::VideoFrame;
use vd_core::{
    Capabilities, ClientInfo, Heartbeat, HeartbeatAction, HostInfo, InputCapabilities, Platform,
    RejectReason, Session, SessionMessage, VideoCodec,
};

use crate::frames::{DecodedFrame, LatestFrame};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `read` blocks before we get a chance to poll the heartbeat.
const READ_POLL: Duration = Duration::from_millis(500);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const READ_BUFFER: usize = 4096;

/// What the connection thread reports back to the UI.
#[derive(Debug)]
pub enum ConnectionEvent {
    /// An attempt is starting; `note` carries the previous failure, if any.
    Connecting { host: String, note: Option<String> },
    /// The host proposed a pairing: the user must approve the PIN.
    PairingRequired { host: String, pin: u32 },
    /// Handshake and pairing succeeded.
    Established(Box<Session>),
    /// An attempt failed; the worker will retry after a backoff.
    Failed { host: String, error: String },
    /// The host refused the session — terminal, the worker stops.
    Rejected { host: String, reason: RejectReason },
}

/// The UI's end of a connection: used to answer pairing requests.
#[derive(Clone, Debug)]
pub struct ConnectionHandle {
    decisions: Sender<bool>,
}

impl ConnectionHandle {
    /// Approves or rejects the pending pairing request.
    pub fn decide_pairing(&self, approved: bool) {
        let _ = self.decisions.send(approved);
    }
}

/// Spawns a thread that keeps a session to `host` alive, reconnecting as needed.
pub fn connect(
    addr: SocketAddr,
    host: HostInfo,
    events: Sender<ConnectionEvent>,
    frames: LatestFrame,
) -> ConnectionHandle {
    let (decisions_tx, decisions_rx) = channel();
    thread::spawn(move || supervise(addr, host, events, decisions_rx, frames));
    ConnectionHandle {
        decisions: decisions_tx,
    }
}

/// Connects, and keeps reconnecting until the host rejects us, the user cancels
/// pairing, or the UI is gone.
fn supervise(
    addr: SocketAddr,
    host: HostInfo,
    events: Sender<ConnectionEvent>,
    decisions: Receiver<bool>,
    frames: LatestFrame,
) {
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut previous_error: Option<String> = None;

    loop {
        debug!("connecting to {}", host.name);
        if events
            .send(ConnectionEvent::Connecting {
                host: host.name.clone(),
                note: previous_error.take(),
            })
            .is_err()
        {
            return; // the UI is gone
        }

        match attempt(addr, &host.name, &events, &decisions, &frames) {
            Attempt::Disconnected(error) => {
                // We had a session, so retry promptly.
                warn!("session with {} ended: {error}", host.name);
                delay = INITIAL_RECONNECT_DELAY;
                previous_error = Some(error);
            }
            Attempt::Failed(error) => {
                warn!(
                    "connection to {} failed: {error}; retrying in {delay:?}",
                    host.name
                );
                delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                previous_error = Some(error);
            }
            Attempt::Rejected(reason) => {
                let _ = events.send(ConnectionEvent::Rejected {
                    host: host.name.clone(),
                    reason,
                });
                return;
            }
            Attempt::Cancelled => {
                info!("connection to {} cancelled", host.name);
                return;
            }
        }

        let error = previous_error.clone().unwrap_or_default();
        if events
            .send(ConnectionEvent::Failed {
                host: host.name.clone(),
                error,
            })
            .is_err()
        {
            return;
        }
        thread::sleep(delay);
    }
}

/// How a single connection attempt ended.
enum Attempt {
    /// A session was established and later dropped.
    Disconnected(String),
    /// A session was never established.
    Failed(String),
    /// The host refused the session.
    Rejected(RejectReason),
    /// The user declined pairing (or the UI went away).
    Cancelled,
}

fn attempt(
    addr: SocketAddr,
    host_name: &str,
    events: &Sender<ConnectionEvent>,
    decisions: &Receiver<bool>,
    frames: &LatestFrame,
) -> Attempt {
    frames.clear();
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(err) => return Attempt::Failed(format!("could not connect: {err}")),
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_POLL));
    info!("control connection to {addr} established, sending hello");

    let mut handshake = match ClientHandshake::new(client_info(), client_capabilities()) {
        Ok(handshake) => handshake,
        Err(err) => return Attempt::Failed(format!("could not generate pairing key: {err}")),
    };
    // Plaintext until the Welcome has been processed and pairing confirmed.
    let mut channel = Channel::plaintext();
    if let Err(err) = write_message(&mut stream, &mut channel, &handshake.hello()) {
        return Attempt::Failed(format!("could not send hello: {err}"));
    }

    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; READ_BUFFER];
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut heartbeat: Option<Heartbeat> = None;

    loop {
        let now = Instant::now();
        match stream.read(&mut buffer) {
            Ok(0) => return Attempt::Disconnected("host closed the connection".to_owned()),
            Ok(read) => {
                decoder.push(&buffer[..read]);
                loop {
                    let payload = match decoder.next_payload() {
                        Ok(Some(payload)) => payload,
                        Ok(None) => break,
                        Err(err) => return Attempt::Disconnected(err.to_string()),
                    };
                    let frame = match channel.decode(&payload) {
                        Ok(frame) => frame,
                        Err(err) => return Attempt::Disconnected(err.to_string()),
                    };

                    if let Some(beat) = heartbeat.as_mut() {
                        // Any traffic proves the host is alive.
                        beat.on_activity(now);
                        if let Frame::Video(video) = frame {
                            publish_frame(frames, &video);
                        }
                        // Post-handshake control messages (pongs) need no action.
                        continue;
                    }

                    let Frame::Control(message) = frame else {
                        return Attempt::Disconnected("video before the handshake".to_owned());
                    };

                    let action = match handshake.on_message(message) {
                        Ok(action) => action,
                        Err(err) => return Attempt::Disconnected(err.to_string()),
                    };

                    if let Action::AwaitingConfirmation { pin } = action {
                        let Some(session) =
                            confirm_pairing(host_name, pin, events, decisions, &mut handshake)
                        else {
                            return Attempt::Cancelled;
                        };
                        // Everything after the Welcome is sealed.
                        channel.encrypt(SessionCipher::new(&session.keys.key, Role::Client));
                        // Confirmation blocks on the user, so the heartbeat must
                        // start now, not when the Welcome arrived.
                        heartbeat = Some(Heartbeat::new(
                            HEARTBEAT_INTERVAL,
                            HEARTBEAT_TIMEOUT,
                            Instant::now(),
                        ));
                        if events
                            .send(ConnectionEvent::Established(Box::new(session)))
                            .is_err()
                        {
                            return Attempt::Cancelled;
                        }
                        continue;
                    }

                    match action {
                        Action::Rejected(reason) => return Attempt::Rejected(reason),
                        // A client never has to reply during the handshake.
                        Action::Reply(_) => {}
                        Action::Established(_) | Action::AwaitingConfirmation { .. } => {
                            unreachable!("handled above")
                        }
                    }
                }
            }
            Err(err) if is_read_timeout(&err) => {}
            Err(err) => return Attempt::Disconnected(format!("read failed: {err}")),
        }

        let now = Instant::now();
        match heartbeat.as_mut() {
            Some(beat) => match beat.poll(now) {
                HeartbeatAction::SendPing => {
                    if let Err(err) =
                        write_message(&mut stream, &mut channel, &SessionMessage::Ping)
                    {
                        return Attempt::Disconnected(format!("could not send ping: {err}"));
                    }
                }
                HeartbeatAction::TimedOut => {
                    let silence = beat.silence(now);
                    return Attempt::Disconnected(format!(
                        "host silent for {:.1}s",
                        silence.as_secs_f32()
                    ));
                }
                HeartbeatAction::Idle => {}
            },
            None if now >= handshake_deadline => {
                return Attempt::Failed("handshake timed out".to_owned())
            }
            None => {}
        }
    }
}

/// Asks the UI to approve the PIN, then completes the handshake.
///
/// Returns `None` if the user declined.
fn confirm_pairing(
    host_name: &str,
    pin: u32,
    events: &Sender<ConnectionEvent>,
    decisions: &Receiver<bool>,
    handshake: &mut ClientHandshake,
) -> Option<Session> {
    info!("pairing with \"{host_name}\" — PIN {pin:06}");
    if events
        .send(ConnectionEvent::PairingRequired {
            host: host_name.to_owned(),
            pin,
        })
        .is_err()
    {
        return None;
    }

    match decisions.recv() {
        Ok(true) => {
            info!("pairing with \"{host_name}\" approved");
            match handshake.confirm() {
                Ok(Action::Established(session)) => {
                    info!(
                        "session established with \"{}\" — protocol v{}, codec {}",
                        session.host.name,
                        session.protocol,
                        codec_name(session.codec),
                    );
                    Some(session)
                }
                Ok(_) => None,
                Err(err) => {
                    warn!("could not confirm pairing: {err}");
                    None
                }
            }
        }
        Ok(false) => {
            info!("pairing with \"{host_name}\" rejected by the user");
            None
        }
        Err(_) => None, // the UI is gone
    }
}

/// Decodes a received frame into the UI mailbox, dropping it on failure.
///
/// A codec we cannot decode is reported once, not on every frame.
fn publish_frame(frames: &LatestFrame, video: &VideoFrame) {
    match decode_rgba(video) {
        Ok(image) => frames.publish(DecodedFrame {
            sequence: video.header.sequence,
            width: image.width,
            height: image.height,
            pixels: image.pixels,
        }),
        Err(err) => {
            let mut reported = UNDECODABLE.lock().unwrap();
            if !reported.contains(&video.header.format) {
                reported.push(video.header.format);
                warn!("cannot decode {:?} frames yet: {err}", video.header.format);
            }
        }
    }
}

/// Codecs we have already complained about.
static UNDECODABLE: Mutex<Vec<vd_core::FrameFormat>> = Mutex::new(Vec::new());

/// A socket read timeout is reported as `TimedOut` on Windows and `WouldBlock`
/// on Unix; both simply mean "check the heartbeat and read again".
fn is_read_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn write_message(
    stream: &mut TcpStream,
    channel: &mut Channel,
    message: &SessionMessage,
) -> Result<(), String> {
    let frame = channel.encode(message).map_err(|err| err.to_string())?;
    stream.write_all(&frame).map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())
}

fn codec_name(codec: Option<VideoCodec>) -> String {
    codec
        .map(|codec| format!("{codec:?}"))
        .unwrap_or_else(|| "none".to_owned())
}

/// Identity this client presents to a host.
pub fn client_info() -> ClientInfo {
    ClientInfo {
        name: device_name(),
        platform: Platform::current(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// What this client can do. Video decode is not implemented yet, but every
/// Android device can decode H.264/HEVC in hardware, so that is what we ask for.
pub fn client_capabilities() -> Capabilities {
    Capabilities {
        codecs: vec![VideoCodec::H264, VideoCodec::Hevc],
        max_width: 1920,
        max_height: 1200,
        max_fps: 60,
        input: InputCapabilities {
            pointer: true,
            keyboard: true,
        },
    }
}

fn device_name() -> String {
    std::env::var("VD_CLIENT_NAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "vd-client".to_owned())
}
