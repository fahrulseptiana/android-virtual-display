//! Client-side control connection.
//!
//! Owns the whole lifecycle of a session: TCP connect, handshake, heartbeats,
//! and reconnection with exponential backoff. Everything runs on a dedicated
//! thread and reports progress to the UI over a channel, so the egui repaint
//! loop never blocks on network I/O.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use vd_core::session::frame::{self, FrameDecoder};
use vd_core::session::handshake::{Action, ClientHandshake};
use vd_core::{
    Capabilities, ClientInfo, Heartbeat, HeartbeatAction, HostInfo, InputCapabilities, Platform,
    RejectReason, Session, SessionMessage, VideoCodec,
};

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
    /// Handshake succeeded.
    Established(Box<Session>),
    /// An attempt failed; the worker will retry after a backoff.
    Failed { host: String, error: String },
    /// The host refused the session — terminal, the worker stops.
    Rejected { host: String, reason: RejectReason },
}

/// Spawns a thread that keeps a session to `host` alive, reconnecting as needed.
pub fn connect(addr: SocketAddr, host: HostInfo, events: Sender<ConnectionEvent>) {
    thread::spawn(move || supervise(addr, host, events));
}

/// Connects, and keeps reconnecting until the host rejects us or the UI is gone.
fn supervise(addr: SocketAddr, host: HostInfo, events: Sender<ConnectionEvent>) {
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

        match attempt(addr, &events) {
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
}

fn attempt(addr: SocketAddr, events: &Sender<ConnectionEvent>) -> Attempt {
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(err) => return Attempt::Failed(format!("could not connect: {err}")),
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_POLL));
    info!("control connection to {addr} established, sending hello");

    let mut handshake = ClientHandshake::new(client_info(), client_capabilities());
    if let Err(err) = write_message(&mut stream, &handshake.hello()) {
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
                    let message = match decoder.next_message() {
                        Ok(Some(message)) => message,
                        Ok(None) => break,
                        Err(err) => return Attempt::Disconnected(err.to_string()),
                    };

                    if let Some(beat) = heartbeat.as_mut() {
                        // Any traffic proves the host is alive.
                        beat.on_activity(now);
                    } else {
                        match handshake.on_message(message) {
                            Ok(Action::Established(session)) => {
                                info!(
                                    "session established with \"{}\" — protocol v{}, codec {}",
                                    session.host.name,
                                    session.protocol,
                                    codec_name(session.codec),
                                );
                                heartbeat = Some(Heartbeat::new(
                                    HEARTBEAT_INTERVAL,
                                    HEARTBEAT_TIMEOUT,
                                    now,
                                ));
                                if events
                                    .send(ConnectionEvent::Established(Box::new(session)))
                                    .is_err()
                                {
                                    return Attempt::Disconnected("ui closed".to_owned());
                                }
                            }
                            Ok(Action::Rejected(reason)) => return Attempt::Rejected(reason),
                            // A client never has to reply during the handshake.
                            Ok(Action::Reply(_)) => {}
                            Err(err) => return Attempt::Disconnected(err.to_string()),
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
                    if let Err(err) = write_message(&mut stream, &SessionMessage::Ping) {
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

/// A socket read timeout is reported as `TimedOut` on Windows and `WouldBlock`
/// on Unix; both simply mean "check the heartbeat and read again".
fn is_read_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn write_message(stream: &mut TcpStream, message: &SessionMessage) -> Result<(), String> {
    let frame = frame::encode(message).map_err(|err| err.to_string())?;
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
