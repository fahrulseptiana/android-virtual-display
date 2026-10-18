//! The client UI: discover hosts, pair, and show the incoming video.

use std::net::SocketAddr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::load::SizedTexture;
use vd_core::discovery::{Browser, DiscoveredHost, DiscoveryConfig, DiscoveryEvent};
use vd_core::{HostInfo, RejectReason, Session};

use crate::connection::{self, ConnectionEvent, ConnectionHandle};
use crate::frames::LatestFrame;

/// How often the discovery socket is serviced while the UI is running.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often egui should repaint even without input (keeps video smooth).
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// State of the control connection.
enum Connection {
    Idle,
    Connecting {
        host: String,
        note: Option<String>,
    },
    /// Waiting for the user to approve the pairing PIN.
    Pairing {
        host: String,
        pin: u32,
    },
    Connected {
        session: Box<Session>,
    },
    Failed {
        host: String,
        reason: String,
    },
}

impl Connection {
    /// Whether a connection is in progress or already open.
    fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Connecting { .. } | Self::Pairing { .. } | Self::Connected { .. }
        )
    }
}

/// eframe application showing the discovered hosts and the video stream.
pub struct VdClientApp {
    browser: Option<Browser>,
    browser_error: Option<String>,
    last_poll: Instant,
    events_tx: Sender<ConnectionEvent>,
    events_rx: Receiver<ConnectionEvent>,
    connection: Connection,
    /// The live connection, used to answer pairing requests.
    handle: Option<ConnectionHandle>,
    /// Newest decoded frame, written by the connection worker.
    frames: LatestFrame,
    texture: Option<egui::TextureHandle>,
    uploaded_sequence: Option<u64>,
    frames_seen: u64,
    first_frame_at: Option<Instant>,
    /// Diagnostic counters: UI passes, and distinct frames the worker produced.
    repaints: u64,
    arrivals: u64,
    last_arrival_sequence: Option<u64>,
    last_report: Instant,
}

impl VdClientApp {
    /// Creates the app and binds the discovery socket.
    pub fn new() -> Self {
        let (events_tx, events_rx) = channel();
        let mut app = Self {
            browser: None,
            browser_error: None,
            last_poll: Instant::now(),
            events_tx,
            events_rx,
            connection: Connection::Idle,
            handle: None,
            frames: LatestFrame::new(),
            texture: None,
            uploaded_sequence: None,
            frames_seen: 0,
            first_frame_at: None,
            repaints: 0,
            arrivals: 0,
            last_arrival_sequence: None,
            last_report: Instant::now(),
        };

        match Browser::bind(DiscoveryConfig::default()) {
            Ok(browser) => {
                log::info!(
                    "discovery browsing on {}",
                    browser
                        .local_addr()
                        .map(|addr| addr.to_string())
                        .unwrap_or_default()
                );
                app.browser = Some(browser);
            }
            Err(err) => {
                log::error!("could not start discovery: {err}");
                app.browser_error = Some(err.to_string());
            }
        }
        app
    }

    /// Services the discovery socket, throttled to [`POLL_INTERVAL`].
    fn poll_discovery(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < POLL_INTERVAL {
            return;
        }
        self.last_poll = now;

        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        match browser.poll(now) {
            Ok(events) => {
                for event in events {
                    match event {
                        DiscoveryEvent::Found(host) => log::info!(
                            "found host \"{}\" at {} ({})",
                            host.info.name,
                            host.control_addr(),
                            host.info.platform
                        ),
                        DiscoveryEvent::Lost(id) => log::info!("lost host {id}"),
                    }
                }
            }
            Err(err) => log::warn!("discovery poll failed: {err}"),
        }
    }

    /// Drains connection events reported by worker threads.
    fn poll_connection(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.connection = match event {
                ConnectionEvent::Connecting { host, note } => Connection::Connecting { host, note },
                ConnectionEvent::PairingRequired { host, pin } => Connection::Pairing { host, pin },
                ConnectionEvent::Established(session) => {
                    self.first_frame_at = Some(Instant::now());
                    self.frames_seen = 0;
                    Connection::Connected { session }
                }
                ConnectionEvent::Rejected { host, reason } => Connection::Failed {
                    host,
                    reason: reject_text(reason),
                },
                // A failed attempt is not terminal: the worker retries. Keep the
                // spinner going and surface the reason as a note.
                ConnectionEvent::Failed { host, error } => Connection::Connecting {
                    host,
                    note: Some(error),
                },
            };
        }
    }

    /// Uploads the newest frame as a texture, if it is newer than the last one.
    fn refresh_texture(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.frames.latest() else {
            return;
        };
        if self.uploaded_sequence == Some(frame.sequence) {
            return;
        }
        let started = std::time::Instant::now();

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.pixels,
        );
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR));
            }
        }
        self.uploaded_sequence = Some(frame.sequence);
        self.frames_seen += 1;

        if self.frames_seen % 30 == 0 {
            log::info!(
                "upload: {} frames, {:.1} ms average",
                self.frames_seen,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    /// Counts frames the decoder has produced (not how often we draw them).
    fn note_frame_arrival(&mut self) {
        if let Some(frame) = self.frames.latest() {
            if self.last_arrival_sequence != Some(frame.sequence) {
                self.last_arrival_sequence = Some(frame.sequence);
                self.arrivals += 1;
            }
        }
    }

    /// Reports the two rates that matter while diagnosing the render path.
    fn maybe_report(&mut self, _ctx: &egui::Context) {
        if self.last_report.elapsed() < Duration::from_secs(5) {
            return;
        }
        let seconds = self.last_report.elapsed().as_secs_f32();
        log::info!(
            "rates: {:.1} repaints/s, {:.1} frames/s, {:.1} uploads/s",
            self.repaints as f32 / seconds,
            self.arrivals as f32 / seconds,
            self.frames_seen as f32 / seconds
        );
        self.repaints = 0;
        self.arrivals = 0;
        self.frames_seen = 0;
        self.last_report = Instant::now();
    }

    fn scan_now(&mut self) {
        if let Some(browser) = self.browser.as_mut() {
            // Errors are non-fatal: we simply try again on the next tick.
            let _ = browser.scan(Instant::now());
        }
    }

    fn start_connection(&mut self, addr: SocketAddr, host: HostInfo) {
        self.connection = Connection::Connecting {
            host: host.name.clone(),
            note: None,
        };
        self.frames.clear();
        self.texture = None;
        self.uploaded_sequence = None;
        self.handle = Some(connection::connect(
            addr,
            host,
            self.events_tx.clone(),
            self.frames.clone(),
        ));
    }

    /// Frames per second since the session was established.
    fn fps(&self) -> f32 {
        match self.first_frame_at {
            Some(start) => {
                let seconds = start.elapsed().as_secs_f32();
                if seconds > 0.0 {
                    self.frames_seen as f32 / seconds
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    }
}

impl Default for VdClientApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for VdClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.repaints += 1;
        self.note_frame_arrival();
        self.maybe_report(ui.ctx());
        self.poll_discovery();
        self.poll_connection();
        self.refresh_texture(ui.ctx());
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);

        let now = Instant::now();
        let mut hosts: Vec<DiscoveredHost> = self
            .browser
            .as_ref()
            .map(|browser| browser.hosts().cloned().collect())
            .unwrap_or_default();
        hosts.sort_by(|a, b| a.info.name.cmp(&b.info.name));

        let connection = &self.connection;
        let texture = self.texture.as_ref();
        let connect_enabled = !connection.is_busy();
        let fps = self.fps();
        let mut scan_requested = false;
        let mut connect_to: Option<(SocketAddr, HostInfo)> = None;
        let mut pairing_decision: Option<bool> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Virtual Display");
            ui.label("Hosts on your local network");

            ui.add_space(8.0);
            connection_status(ui, connection, fps);

            if let Some(texture) = texture {
                ui.add_space(8.0);
                show_video(ui, texture);
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Scan now").clicked() {
                    scan_requested = true;
                }
                ui.label(format!("{} found", hosts.len()));
            });
            ui.add_space(4.0);
            ui.separator();

            if let Some(error) = &self.browser_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Discovery unavailable: {error}"),
                );
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                if hosts.is_empty() {
                    ui.add_space(12.0);
                    ui.label("Searching for hosts…");
                    return;
                }
                for host in &hosts {
                    if host_card(ui, host, now, connect_enabled) {
                        connect_to = Some((host.control_addr(), host.info.clone()));
                    }
                }
            });
        });

        if let Connection::Pairing { host, pin } = connection {
            egui::Window::new("Pair device")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Pair with \"{host}\"?"));
                    ui.add_space(6.0);
                    ui.heading(format!("{pin:06}"));
                    ui.label("Confirm this code to start the session.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            pairing_decision = Some(false);
                        }
                        if ui.button("Pair").clicked() {
                            pairing_decision = Some(true);
                        }
                    });
                });
        }

        if let Some(approved) = pairing_decision {
            if let Some(handle) = self.handle.as_ref() {
                handle.decide_pairing(approved);
            }
            let host = match &self.connection {
                Connection::Pairing { host, .. } => Some(host.clone()),
                _ => None,
            };
            match (approved, host) {
                (true, Some(host)) => {
                    self.connection = Connection::Connecting { host, note: None };
                }
                (false, _) => {
                    self.connection = Connection::Idle;
                    self.handle = None;
                }
                (true, None) => {}
            }
        }

        if scan_requested {
            self.scan_now();
        }
        if let Some((addr, host)) = connect_to {
            self.start_connection(addr, host);
        }
    }
}

/// Draws the video, fitted to the space available for it.
fn show_video(ui: &mut egui::Ui, texture: &egui::TextureHandle) {
    let video = texture.size_vec2();
    // Keep room for the status card and the host list below.
    let available = egui::vec2(ui.available_width(), ui.available_height() * 0.65);
    let scale = (available.x / video.x).min(available.y / video.y);
    let size = video * scale.clamp(0.05, 1.0);

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.add(egui::Image::new(SizedTexture::new(texture.id(), size)));
    });
}

fn connection_status(ui: &mut egui::Ui, connection: &Connection, fps: f32) {
    egui::Frame::group(ui.style()).show(ui, |ui| match connection {
        Connection::Idle => {
            ui.label("Not connected");
        }
        Connection::Connecting { host, note } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Connecting to {host}…"));
            });
            if let Some(note) = note {
                ui.label(format!("last attempt: {note}"));
            }
        }
        Connection::Pairing { host, pin } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Waiting to pair with {host}…"));
            });
            ui.label(format!("PIN: {pin:06}"));
        }
        Connection::Connected { session } => {
            ui.colored_label(
                egui::Color32::from_rgb(0x6c, 0xcb, 0x5f),
                format!("Connected to {}", session.host.name),
            );
            ui.label(format!(
                "protocol v{} · codec {} · {fps:.1} fps",
                session.protocol,
                session
                    .codec
                    .map(|codec| format!("{codec:?}"))
                    .unwrap_or_else(|| "none".to_owned())
            ));
        }
        Connection::Failed { host, reason } => {
            ui.colored_label(ui.visuals().error_fg_color, format!("{host}: {reason}"));
        }
    });
}

fn host_card(
    ui: &mut egui::Ui,
    host: &DiscoveredHost,
    now: Instant,
    connect_enabled: bool,
) -> bool {
    let mut connect_clicked = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong(&host.info.name);
            ui.label(format!("· {}", host.info.platform));
            ui.add_space(8.0);
            if ui
                .add_enabled(connect_enabled, egui::Button::new("Connect"))
                .clicked()
            {
                connect_clicked = true;
            }
        });
        ui.label(format!("Address: {}", host.control_addr()));
        ui.label(format!(
            "Last seen: {:.1}s ago · v{}",
            host.age(now).as_secs_f32(),
            host.info.app_version
        ));
    });
    connect_clicked
}

fn reject_text(reason: RejectReason) -> String {
    match reason {
        RejectReason::ProtocolMismatch => "incompatible protocol version".to_owned(),
        RejectReason::Busy => "host is busy".to_owned(),
        RejectReason::Unauthorized => "not authorised".to_owned(),
    }
}
