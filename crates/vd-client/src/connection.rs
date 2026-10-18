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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
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
    Capabilities, ClientInfo, FrameFormat, Heartbeat, HeartbeatAction, HostInfo, InputCapabilities,
    Platform, RejectReason, Session, SessionMessage, VideoCodec,
};

use crate::frames::{DecodedFrame, FrameQueue};
use crate::trust::TrustStore;

#[cfg(target_os = "android")]
pub use crate::media_codec::H264Decoder;

/// A hardware H.264 decoder, or a stub where there is none.
#[cfg(target_os = "android")]
type Decoder = H264Decoder;

/// Stand-in on platforms without Android's media stack.
#[cfg(not(target_os = "android"))]
pub struct FallbackDecoder;

#[cfg(not(target_os = "android"))]
impl FallbackDecoder {
    fn new(_width: u32, _height: u32, _extradata: &[u8]) -> Result<Self, String> {
        Err("H.264 decoding is only implemented on Android".to_owned())
    }

    fn decode(
        &mut self,
        _au: &[u8],
        _timestamp_us: u64,
    ) -> Result<Vec<vd_core::codec::RgbaImage>, String> {
        Err("H.264 decoding is only implemented on Android".to_owned())
    }
}

#[cfg(not(target_os = "android"))]
type Decoder = FallbackDecoder;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `read` blocks before we get a chance to poll the heartbeat.
const READ_POLL: Duration = Duration::from_millis(250);
/// Frequent enough that a host which has gone away is noticed in about this
/// long, rather than only when a write finally fails.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
/// A LAN connection either succeeds at once or is refused at once, so the
/// backoff exists only to avoid spinning while a host is down — it does not
/// need to grow into tens of seconds.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const READ_BUFFER: usize = 4096;

/// What the connection thread reports back to the UI.
#[derive(Debug)]
pub enum ConnectionEvent {
    /// An attempt is starting; `note` carries the previous failure, if any.
    Connecting { host: String, note: Option<String> },
    /// The host proposed a pairing: the user must approve the PIN.
    PairingRequired {
        host: String,
        pin: u32,
        /// True when this host was already known but presented a different
        /// identity key — worth showing the user in the strongest terms.
        identity_changed: bool,
    },
    /// Handshake and pairing succeeded.
    Established(Box<Session>),
    /// An attempt failed; the worker will retry after a backoff.
    Failed { host: String, error: String },
    /// The host refused the session — terminal, the worker stops.
    Rejected { host: String, reason: RejectReason },
}

/// The two signals the UI can send a running worker.
#[derive(Clone)]
struct WorkerControl {
    /// Stop the connection and any reconnection attempts.
    cancelled: Arc<AtomicBool>,
    /// Skip the rest of the current backoff.
    retry: Arc<AtomicBool>,
}

/// The UI's end of a connection: answers pairing requests and can stop the
/// worker.
#[derive(Clone, Debug)]
pub struct ConnectionHandle {
    decisions: Sender<bool>,
    cancelled: Arc<AtomicBool>,
    retry: Arc<AtomicBool>,
}

impl ConnectionHandle {
    /// Approves or rejects the pending pairing request.
    pub fn decide_pairing(&self, approved: bool) {
        let _ = self.decisions.send(approved);
    }

    /// Stops the connection and any reconnection attempts.
    pub fn disconnect(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Skips whatever is left of the current backoff.
    ///
    /// The UI calls this when discovery shows the host is back: there is no
    /// reason to sit out a delay that exists only because the host was down.
    pub fn retry_now(&self) {
        self.retry.store(true, Ordering::Relaxed);
    }
}

/// Spawns a thread that keeps a session to `host` alive, reconnecting as needed.
pub fn connect(
    addr: SocketAddr,
    host: HostInfo,
    events: Sender<ConnectionEvent>,
    frames: FrameQueue,
    trust_path: Option<std::path::PathBuf>,
) -> ConnectionHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let retry = Arc::new(AtomicBool::new(false));
    let (decisions_tx, decisions_rx) = channel();
    let control = WorkerControl {
        cancelled: cancelled.clone(),
        retry: retry.clone(),
    };
    thread::spawn(move || {
        supervise(
            addr,
            host,
            events,
            decisions_rx,
            frames,
            control,
            trust_path,
        )
    });
    ConnectionHandle {
        decisions: decisions_tx,
        cancelled,
        retry,
    }
}

/// Connects, and keeps reconnecting until the host rejects us, the user cancels
/// pairing, or the UI is gone.
fn supervise(
    addr: SocketAddr,
    host: HostInfo,
    events: Sender<ConnectionEvent>,
    decisions: Receiver<bool>,
    frames: FrameQueue,
    control: WorkerControl,
    trust_path: Option<std::path::PathBuf>,
) {
    // Loaded once for the lifetime of the worker: this store is what makes a
    // reconnect to a known host silent.
    let mut trust = TrustStore::load(trust_path);
    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut previous_error: Option<String> = None;

    loop {
        if control.cancelled.load(Ordering::Relaxed) {
            return;
        }
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

        match attempt(
            addr, &host.name, &events, &decisions, &frames, &control, &mut trust,
        ) {
            Attempt::Disconnected(error) => {
                // We had a session, so try again immediately: the usual reason
                // for losing one is that the host restarted, and it is often
                // already listening again by the time we get here.
                warn!("session with {} ended: {error}", host.name);
                delay = Duration::ZERO;
                previous_error = Some(error);
            }
            Attempt::Failed(error) => {
                // A refused connection means the host really is down, so wait —
                // but starting from a short delay, and never for long.
                let next = if delay.is_zero() {
                    INITIAL_RECONNECT_DELAY
                } else {
                    (delay * 2).min(MAX_RECONNECT_DELAY)
                };
                warn!(
                    "connection to {} failed: {error}; retrying in {next:?}",
                    host.name
                );
                delay = next;
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
        // Wait out the backoff, but stay responsive to a disconnect — and to
        // the host reappearing, which makes the rest of the wait pointless.
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if control.cancelled.load(Ordering::Relaxed) {
                return;
            }
            if control.retry.swap(false, Ordering::Relaxed) {
                debug!("retrying now: discovery says the host is back");
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
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
    frames: &FrameQueue,
    control: &WorkerControl,
    trust: &mut TrustStore,
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
    // Decoding runs on its own thread so it never blocks the socket.
    let decode_worker = DecoderWorker::spawn(frames.clone());

    loop {
        if control.cancelled.load(Ordering::Relaxed) {
            return Attempt::Cancelled;
        }
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
                        handle_stream(frames, &decode_worker, frame);
                        continue;
                    }

                    let Frame::Control(message) = frame else {
                        return Attempt::Disconnected("video before the handshake".to_owned());
                    };
                    let message = *message;

                    let action = match handshake.on_message(message) {
                        Ok(action) => action,
                        Err(err) => return Attempt::Disconnected(err.to_string()),
                    };

                    if let Action::AwaitingConfirmation { pin } = action {
                        let identity = handshake.host_identity().copied();
                        let Some(host_id) = handshake.host_id().cloned() else {
                            return Attempt::Disconnected(
                                "the host welcomed us without an identity".to_owned(),
                            );
                        };
                        let Some(session) = confirm_pairing(
                            host_name,
                            &host_id,
                            pin,
                            identity,
                            events,
                            decisions,
                            &mut handshake,
                            trust,
                            control,
                        ) else {
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
#[allow(clippy::too_many_arguments)]
fn confirm_pairing(
    host_name: &str,
    host_id: &vd_core::HostId,
    pin: u32,
    identity: Option<[u8; 32]>,
    events: &Sender<ConnectionEvent>,
    decisions: &Receiver<bool>,
    handshake: &mut ClientHandshake,
    trust: &mut TrustStore,
    control: &WorkerControl,
) -> Option<Session> {
    let trusted = identity
        .as_ref()
        .map(|key| trust.matches(host_id, key))
        .unwrap_or(false);

    if trusted {
        // A host we have paired with before, presenting the same identity.
        // No prompt — this is what makes reconnecting seamless.
        info!("{host_name} is a known host; pairing without a prompt");
    } else {
        let identity_changed = trust.has(host_id);
        if identity_changed {
            warn!(
                "{host_name} presented a different identity key than the one we remember; \
                 this is either a re-installed host or something impersonating it"
            );
        }
        info!("pairing with \"{host_name}\" — PIN {pin:06}");
        if events
            .send(ConnectionEvent::PairingRequired {
                host: host_name.to_owned(),
                pin,
                identity_changed,
            })
            .is_err()
        {
            return None;
        }

        let decision = loop {
            if control.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            match decisions.recv_timeout(Duration::from_millis(200)) {
                Ok(approved) => break approved,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        };

        if !decision {
            info!("pairing with \"{host_name}\" rejected by the user");
            return None;
        }

        // Trust on first use: remember the identity the user just accepted.
        if let Some(key) = identity {
            info!("remembering {host_name}'s identity");
            trust.remember(host_id.clone(), key);
        }
    }

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

/// Handles a post-handshake message: codec configuration or a video frame.
///
/// Decoding happens on its own thread. Doing it here would stop us reading the
/// socket for ~18 ms per frame, the TCP window would fill, the host's write
/// would block, and the broadcaster would drop frames — which is exactly what
/// the first version of this did.
fn handle_stream(frames: &FrameQueue, worker: &DecoderWorker, frame: Frame) {
    match frame {
        Frame::Control(message) if matches!(*message, SessionMessage::VideoConfig { .. }) => {
            let SessionMessage::VideoConfig {
                format,
                width,
                height,
                extradata,
            } = *message
            else {
                unreachable!("guarded above")
            };
            if format == FrameFormat::H264 {
                worker.configure(width, height, extradata);
            }
        }
        // Pongs and anything else need no action.
        Frame::Control(_) => {}
        Frame::Video(video) if video.header.format == FrameFormat::H264 => {
            report_wire_sequence(video.header.sequence);
            worker.submit(
                video.payload.clone(),
                video.header.sequence,
                video.header.timestamp_us,
            );
        }
        // Anything else (JPEG, raw) is decoded straight into the mailbox.
        Frame::Video(video) => publish_frame(frames, &video),
    }
}

/// Work handed to the decoder thread.
enum DecodeJob {
    Configure {
        width: u32,
        height: u32,
        extradata: Vec<u8>,
    },
    Frame {
        payload: Vec<u8>,
        sequence: u64,
        timestamp_us: u64,
    },
}

/// Number of access units to hold while the decoder is busy.
const DECODE_QUEUE: usize = 4;

/// Owns the decoder thread. Dropping it stops the thread.
struct DecoderWorker {
    jobs: Option<SyncSender<DecodeJob>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DecoderWorker {
    fn spawn(frames: FrameQueue) -> Self {
        let (jobs, receiver) = sync_channel(DECODE_QUEUE);
        let handle = thread::spawn(move || decode_loop(receiver, frames));
        Self {
            jobs: Some(jobs),
            handle: Some(handle),
        }
    }

    fn configure(&self, width: u32, height: u32, extradata: Vec<u8>) {
        self.send(DecodeJob::Configure {
            width,
            height,
            extradata,
        });
    }

    fn submit(&self, payload: Vec<u8>, sequence: u64, timestamp_us: u64) {
        self.send(DecodeJob::Frame {
            payload,
            sequence,
            timestamp_us,
        });
    }

    /// Drops the job when the decoder is behind, rather than blocking the reader.
    fn send(&self, job: DecodeJob) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        if let Err(TrySendError::Full(_)) = jobs.try_send(job) {
            debug!("dropped a frame: the decoder is behind");
        }
    }
}

impl Drop for DecoderWorker {
    fn drop(&mut self) {
        // Closing the channel ends the loop.
        self.jobs = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Watches the sequence numbers arriving on the wire, so duplicates and gaps are
/// visible independently of anything the decoder or the UI do.
fn report_wire_sequence(sequence: u64) {
    use std::sync::Mutex;

    /// Last sequence seen, counts of [duplicate, in-order, gap], window start.
    type WireState = (Option<u64>, [u32; 3], Instant);
    static STATE: std::sync::OnceLock<Mutex<WireState>> = std::sync::OnceLock::new();

    let mut state = STATE
        .get_or_init(|| Mutex::new((None, [0; 3], Instant::now())))
        .lock()
        .unwrap();

    if let Some(previous) = state.0 {
        let delta = sequence.wrapping_sub(previous);
        // 0 = duplicate, 1 = next, anything else = a gap.
        match delta {
            0 => state.1[0] += 1,
            1 => state.1[1] += 1,
            _ => state.1[2] += 1,
        }
    }
    state.0 = Some(sequence);

    if state.2.elapsed() >= Duration::from_secs(5) {
        let seconds = state.2.elapsed().as_secs_f32();
        let total: u32 = state.1.iter().sum();
        info!(
            "wire: {:.1} frames/s ({:.1}/s duplicated, {:.1}/s in order, {:.1}/s with gaps)",
            total as f32 / seconds,
            state.1[0] as f32 / seconds,
            state.1[1] as f32 / seconds,
            state.1[2] as f32 / seconds,
        );
        state.1 = [0; 3];
        state.2 = Instant::now();
    }
}

/// Counters shared by the decoder thread, so its own throughput is visible
/// independently of how often the UI happens to redraw.
mod decode_stats {
    use std::sync::atomic::AtomicU64;

    pub static INPUTS: AtomicU64 = AtomicU64::new(0);
    pub static PICTURES: AtomicU64 = AtomicU64::new(0);
    pub static DISTINCT: AtomicU64 = AtomicU64::new(0);
    pub static LAST_SEQUENCE: AtomicU64 = AtomicU64::new(u64::MAX);
    pub static LAST_REPORT: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> =
        std::sync::OnceLock::new();
}

/// Records one access unit going in and the pictures coming out.
fn record_decode(sequence: u64, produced: usize) {
    use std::sync::atomic::Ordering;
    decode_stats::INPUTS.fetch_add(1, Ordering::Relaxed);
    decode_stats::PICTURES.fetch_add(produced as u64, Ordering::Relaxed);
    if produced > 0 {
        let previous = decode_stats::LAST_SEQUENCE.swap(sequence, Ordering::Relaxed);
        if previous != sequence {
            decode_stats::DISTINCT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Prints the decoder's throughput every few seconds.
fn report_decode_throughput(now: Instant) {
    use std::sync::atomic::Ordering;

    let last = decode_stats::LAST_REPORT.get_or_init(|| std::sync::Mutex::new(now));
    let mut last = last.lock().unwrap();
    if last.elapsed() < Duration::from_secs(5) {
        return;
    }

    let inputs = decode_stats::INPUTS.swap(0, Ordering::Relaxed);
    let pictures = decode_stats::PICTURES.swap(0, Ordering::Relaxed);
    let distinct = decode_stats::DISTINCT.swap(0, Ordering::Relaxed);
    let seconds = last.elapsed().as_secs_f32();
    *last = now;
    if seconds <= 0.0 {
        return;
    }

    info!(
        "decoder: {:.1} access units/s in, {:.1} pictures/s out, {:.1} distinct sequences/s",
        inputs as f32 / seconds,
        pictures as f32 / seconds,
        distinct as f32 / seconds,
    );
}

fn decode_loop(jobs: Receiver<DecodeJob>, frames: FrameQueue) {
    let mut decoder: Option<Decoder> = None;
    // One input can yield several pictures (the decoder catching up after a
    // stall). They must not share a sequence, or the UI's "is it new?" check
    // collapses them and silently drops all but the last.
    let mut picture_index: u64 = 0;

    while let Ok(job) = jobs.recv() {
        match job {
            DecodeJob::Configure {
                width,
                height,
                extradata,
            } => match Decoder::new(width, height, &extradata) {
                Ok(created) => {
                    info!(
                        "H.264 decoder ready ({width}x{height}, {} byte extradata)",
                        extradata.len()
                    );
                    decoder = Some(created);
                }
                Err(err) => {
                    warn!("could not start the H.264 decoder: {err}");
                    decoder = None;
                }
            },
            DecodeJob::Frame {
                payload,
                sequence,
                timestamp_us,
            } => {
                let Some(decoder) = decoder.as_mut() else {
                    continue;
                };
                match decode_timed(decoder, &payload, timestamp_us) {
                    Ok(images) => {
                        record_decode(sequence, images.len());
                        for image in images {
                            frames.publish(DecodedFrame {
                                sequence: picture_index,
                                width: image.width,
                                height: image.height,
                                pixels: image.pixels,
                            });
                            picture_index += 1;
                        }
                        report_decode_throughput(Instant::now());
                    }
                    Err(err) => warn!("H.264 decode failed: {err}"),
                }
            }
        }
    }
}

/// Decodes one access unit, reporting the average cost every 30 frames.
fn decode_timed(
    decoder: &mut Decoder,
    payload: &[u8],
    timestamp_us: u64,
) -> Result<Vec<vd_core::codec::RgbaImage>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FRAMES: AtomicU64 = AtomicU64::new(0);
    static MICROS: AtomicU64 = AtomicU64::new(0);

    let started = Instant::now();
    let result = decoder
        .decode(payload, timestamp_us)
        .map_err(|err| err.to_string());
    let elapsed = started.elapsed().as_micros() as u64;

    let frames = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    let micros = MICROS.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
    if frames % 30 == 0 {
        info!(
            "decode: {frames} frames, {:.1} ms average",
            micros as f64 / frames as f64 / 1000.0
        );
    }
    result
}

/// Complains about a codec we cannot decode, once.
fn report_undecodable(format: FrameFormat) {
    let mut reported = UNDECODABLE.lock().unwrap();
    if !reported.contains(&format) {
        reported.push(format);
        warn!("no decoder for {format:?} frames");
    }
}

/// Decodes a received frame into the UI mailbox, dropping it on failure.
///
/// A codec we cannot decode is reported once, not on every frame.
fn publish_frame(frames: &FrameQueue, video: &VideoFrame) {
    match decode_rgba(video) {
        Ok(image) => frames.publish(DecodedFrame {
            sequence: video.header.sequence,
            width: image.width,
            height: image.height,
            pixels: image.pixels,
        }),
        Err(err) => {
            let _ = err;
            report_undecodable(video.header.format);
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
