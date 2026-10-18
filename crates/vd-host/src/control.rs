//! Control channel server.
//!
//! Accepts TCP connections on the host's [`control_port`](vd_core::HostInfo::control_port)
//! and runs the [`HostHandshake`] against each of them. Each connection is
//! handled on its own thread: the handshake is short, and a slow or hostile
//! client must not stall the discovery loop.
//!
//! Only the handshake exists today — after it succeeds the connection is kept
//! open and further messages are ignored until the heartbeat/input steps.

use std::io::{Error as IoError, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

use log::{debug, info, warn};
use vd_core::session::frame::{self, FrameDecoder};
use vd_core::session::handshake::{Action, HostHandshake, State};
use vd_core::{Capabilities, HostInfo, Session};

/// Read buffer for a single `recv`.
const READ_BUFFER: usize = 4096;

/// Listens for and handshakes control connections.
pub struct ControlServer {
    listener: TcpListener,
    host: HostInfo,
    capabilities: Capabilities,
}

impl ControlServer {
    /// Binds the control port advertised in discovery.
    pub fn bind(host: HostInfo, capabilities: Capabilities) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", host.control_port))?;
        // Non-blocking so the host's discovery loop is never held up.
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            host,
            capabilities,
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
                    thread::spawn(move || {
                        if let Err(err) = serve(stream, peer, host, capabilities) {
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

/// Runs the handshake for one connection, then keeps it open until the client leaves.
fn serve(
    mut stream: TcpStream,
    peer: SocketAddr,
    host: HostInfo,
    capabilities: Capabilities,
) -> Result<(), Box<dyn std::error::Error>> {
    // On Windows an accepted socket inherits the listener's non-blocking mode,
    // so the worker explicitly asks for blocking reads.
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    debug!("control connection from {peer}");

    let mut handshake = HostHandshake::new(host, capabilities);
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; READ_BUFFER];
    let mut announced = false;

    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) => {
                debug!("control connection from {peer} closed");
                return Ok(());
            }
            Ok(read) => read,
            Err(err) if is_disconnect(&err) => {
                debug!("control connection from {peer} went away: {err}");
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        decoder.push(&buffer[..read]);

        while let Some(message) = decoder.next_message()? {
            if handshake.state() == State::Established {
                // Later steps: heartbeats and input events are handled here.
                debug!("ignoring post-handshake message from {peer}: {message:?}");
                continue;
            }

            let Action::Reply(reply) = handshake.on_message(message)? else {
                return Err(IoError::new(ErrorKind::InvalidData, "unexpected host action").into());
            };
            if let Err(err) = stream.write_all(&frame::encode(&reply)?) {
                if is_disconnect(&err) {
                    return Ok(());
                }
                return Err(err.into());
            }

            match handshake.session() {
                Some(session) if !announced => {
                    announced = true;
                    log_session(session, peer);
                }
                None => {
                    info!("rejected control connection from {peer}");
                    return Ok(());
                }
                Some(_) => {}
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

fn log_session(session: &Session, peer: SocketAddr) {
    info!(
        "session established with \"{}\" ({}) from {peer} — protocol v{}, codec {}",
        session.client.name,
        session.client.platform,
        session.protocol,
        session
            .codec
            .map(|codec| format!("{codec:?}"))
            .unwrap_or_else(|| "none".to_owned()),
    );
}
