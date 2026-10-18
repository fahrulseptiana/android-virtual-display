//! Client-side control connection.
//!
//! Opens a TCP connection to a chosen host, runs the [`ClientHandshake`] and
//! reports the outcome back to the UI over a channel. Each connection lives on
//! its own thread so the egui repaint loop is never blocked by network I/O.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use log::{info, warn};
use vd_core::session::frame::{self, FrameDecoder};
use vd_core::session::handshake::{Action, ClientHandshake, State};
use vd_core::{
    Capabilities, ClientInfo, HostInfo, InputCapabilities, Platform, RejectReason, Session,
    VideoCodec,
};

/// Connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BUFFER: usize = 4096;

/// What the connection thread reports back to the UI.
#[derive(Debug)]
pub enum ConnectionEvent {
    /// Handshake succeeded.
    Established(Box<Session>),
    /// The host refused the session.
    Rejected { host: String, reason: RejectReason },
    /// Something went wrong before or during the handshake.
    Failed { host: String, error: String },
    /// The host closed the connection.
    Closed { host: String },
}

/// Spawns a thread that connects to `addr` and handshakes with `host`.
pub fn connect(addr: SocketAddr, host: HostInfo, events: Sender<ConnectionEvent>) {
    thread::spawn(move || run(addr, host, events));
}

fn run(addr: SocketAddr, host: HostInfo, events: Sender<ConnectionEvent>) {
    let name = host.name.clone();

    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(err) => {
            let _ = events.send(ConnectionEvent::Failed {
                host: name,
                error: err.to_string(),
            });
            return;
        }
    };
    let _ = stream.set_nodelay(true);
    info!("control connection to {addr} established, sending hello");

    let mut handshake = ClientHandshake::new(client_info(), client_capabilities());
    if let Err(err) = write_frame(&mut stream, &handshake.hello()) {
        let _ = events.send(ConnectionEvent::Failed {
            host: name,
            error: err.to_string(),
        });
        return;
    }

    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; READ_BUFFER];

    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = events.send(ConnectionEvent::Failed {
                    host: name,
                    error: err.to_string(),
                });
                return;
            }
        };
        decoder.push(&buffer[..read]);

        loop {
            let message = match decoder.next_message() {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(err) => {
                    let _ = events.send(ConnectionEvent::Failed {
                        host: name,
                        error: err.to_string(),
                    });
                    return;
                }
            };

            // After the handshake the channel is idle until the heartbeat step.
            if handshake.state() == State::Established {
                continue;
            }

            match handshake.on_message(message) {
                Ok(Action::Established(session)) => {
                    info!(
                        "session established with \"{}\" — protocol v{}, codec {}",
                        session.host.name,
                        session.protocol,
                        session
                            .codec
                            .map(|codec| format!("{codec:?}"))
                            .unwrap_or_else(|| "none".to_owned()),
                    );
                    if events
                        .send(ConnectionEvent::Established(Box::new(session)))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Action::Rejected(reason)) => {
                    warn!("host {name} rejected the session: {reason:?}");
                    let _ = events.send(ConnectionEvent::Rejected { host: name, reason });
                    return;
                }
                Ok(Action::Reply(_)) => {
                    // A client never has to reply during the handshake.
                }
                Err(err) => {
                    let _ = events.send(ConnectionEvent::Failed {
                        host: name,
                        error: err.to_string(),
                    });
                    return;
                }
            }
        }
    }

    let _ = events.send(ConnectionEvent::Closed { host: name });
}

fn write_frame(stream: &mut TcpStream, message: &vd_core::SessionMessage) -> std::io::Result<()> {
    let frame = frame::encode(message)
        .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
    stream.write_all(&frame)?;
    stream.flush()
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
