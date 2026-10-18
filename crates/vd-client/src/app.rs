//! The client UI.
//!
//! Two views: a browser (searching for hosts, then listing them) and a
//! full-screen stream. The Android back button returns from the stream to the
//! list.

use std::net::SocketAddr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use vd_core::discovery::{Browser, DiscoveredHost, DiscoveryConfig, DiscoveryEvent};
use vd_core::{HostInfo, RejectReason, Session};

use crate::connection::{self, ConnectionEvent, ConnectionHandle};
use crate::frames::FrameQueue;

/// How often the discovery socket is serviced while the UI is running.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often egui should repaint even without input: keeps the ripple animation
/// smooth and drains the frame queue promptly.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// A host seen more recently than this is treated as online.
const ONLINE_WINDOW: Duration = Duration::from_secs(3);

/// Height of a host row.
const ROW_HEIGHT: f32 = 60.0;

/// Which screen the app is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// Searching for hosts, or listing the ones found.
    Browser,
    /// Full-screen remote desktop.
    Stream,
}

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

/// eframe application.
pub struct VdClientApp {
    view: View,
    browser: Option<Browser>,
    browser_error: Option<String>,
    last_poll: Instant,
    events_tx: Sender<ConnectionEvent>,
    events_rx: Receiver<ConnectionEvent>,
    connection: Connection,
    /// The live connection, used to answer pairing requests and to disconnect.
    handle: Option<ConnectionHandle>,
    /// Decoded frames waiting to be drawn.
    frames: FrameQueue,
    texture: Option<egui::TextureHandle>,
    frames_seen: u64,
    measured_fps: f32,
    /// The host currently connected to, for the stream overlay.
    streaming_host: Option<String>,
}

impl VdClientApp {
    /// Creates the app and binds the discovery socket.
    pub fn new() -> Self {
        let (events_tx, events_rx) = channel();
        let mut app = Self {
            view: View::Browser,
            browser: None,
            browser_error: None,
            last_poll: Instant::now(),
            events_tx,
            events_rx,
            connection: Connection::Idle,
            handle: None,
            frames: FrameQueue::new(),
            texture: None,
            frames_seen: 0,
            measured_fps: 0.0,
            streaming_host: None,
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
                    self.streaming_host = Some(session.host.name.clone());
                    // Straight to full screen once the session is up.
                    self.view = View::Stream;
                    Connection::Connected { session }
                }
                ConnectionEvent::Rejected { host, reason } => Connection::Failed {
                    host,
                    reason: reject_text(reason),
                },
                // Not terminal: the worker retries, so keep the spinner going.
                ConnectionEvent::Failed { host, error } => Connection::Connecting {
                    host,
                    note: Some(error),
                },
            };
        }
    }

    /// Uploads one queued frame as a texture.
    fn refresh_texture(&mut self, ctx: &egui::Context) {
        // One frame per repaint: there is far more repaint headroom than frames,
        // so the queue stays empty and bursts are not collapsed.
        let Some(frame) = self.frames.next() else {
            return;
        };

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
        self.frames_seen += 1;
    }

    /// Leaves the stream and returns to the host list.
    fn leave_stream(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.disconnect();
        }
        self.handle = None;
        self.connection = Connection::Idle;
        self.streaming_host = None;
        self.view = View::Browser;
        self.frames.clear();
        self.texture = None;
    }

    fn start_connection(&mut self, addr: SocketAddr, host: HostInfo) {
        self.connection = Connection::Connecting {
            host: host.name.clone(),
            note: None,
        };
        self.frames.clear();
        self.texture = None;
        self.handle = Some(connection::connect(
            addr,
            host,
            self.events_tx.clone(),
            self.frames.clone(),
        ));
    }

    /// Frames per second, measured over the last reporting window.
    fn fps(&self) -> f32 {
        self.measured_fps
    }

    fn report_rates(&mut self) {
        static LAST: std::sync::OnceLock<std::sync::Mutex<Instant>> = std::sync::OnceLock::new();
        let last = LAST.get_or_init(|| std::sync::Mutex::new(Instant::now()));
        let mut last = last.lock().unwrap();
        if last.elapsed() < Duration::from_secs(5) {
            return;
        }
        let seconds = last.elapsed().as_secs_f32();
        self.measured_fps = self.frames_seen as f32 / seconds;
        log::info!(
            "rates: {:.1} frames/s drawn, {} queued",
            self.measured_fps,
            self.frames.len()
        );
        self.frames_seen = 0;
        *last = Instant::now();
    }

    /// The host list, newest information first.
    fn hosts(&self) -> Vec<DiscoveredHost> {
        let mut hosts: Vec<DiscoveredHost> = self
            .browser
            .as_ref()
            .map(|browser| browser.hosts().cloned().collect())
            .unwrap_or_default();
        hosts.sort_by(|a, b| a.info.name.cmp(&b.info.name));
        hosts
    }
}

impl Default for VdClientApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for VdClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_discovery();
        self.poll_connection();
        self.refresh_texture(ui.ctx());
        self.report_rates();
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);

        // Android's back button arrives as `BrowserBack`.
        if ui.ctx().input(|i| i.key_pressed(egui::Key::BrowserBack)) && self.view == View::Stream {
            log::info!("leaving the stream");
            self.leave_stream();
        }

        match self.view {
            View::Stream => self.show_stream(ui),
            View::Browser => self.show_browser(ui),
        }
    }
}

impl VdClientApp {
    /// The searching / host list screen.
    fn show_browser(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let hosts = self.hosts();
        let connection = &self.connection;
        let busy = connection.is_busy();
        let error = self.browser_error.clone();
        let mut connect_to: Option<(SocketAddr, HostInfo)> = None;
        let mut pairing_decision: Option<bool> = None;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(0x12, 0x12, 0x14)))
            .show(ui, |ui| {
                if let Some(error) = &error {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Discovery unavailable: {error}"),
                    );
                    return;
                }

                if hosts.is_empty() {
                    searching(ui);
                    return;
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.heading("Nearby devices");
                    });
                    ui.add_space(12.0);

                    for host in &hosts {
                        if host_row(ui, host, now, !busy) {
                            connect_to = Some((host.control_addr(), host.info.clone()));
                        }
                    }

                    ui.add_space(16.0);
                    status_line(ui, connection);
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

        if let Some((addr, host)) = connect_to {
            self.start_connection(addr, host);
        }
    }

    /// Full-screen video.
    fn show_stream(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_rgb(0x08, 0x08, 0x0a));

        match self.texture.as_ref() {
            Some(texture) => {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let target = egui::Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(
                    texture.id(),
                    target,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                // Connected but nothing decoded yet.
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Waiting for the first frame…",
                    egui::FontId::proportional(18.0),
                    egui::Color32::from_gray(180),
                );
            }
        }

        stream_overlay(
            ui,
            rect,
            self.streaming_host.as_deref(),
            self.fps(),
            &self.connection,
        );
    }
}

/// The "no hosts yet" screen: a rippling pulse plus a caption.
fn searching(ui: &mut egui::Ui) {
    // egui's clock drives the animation; the app repaints every 16 ms.
    let time = ui.input(|i| i.time) as f32;
    let available = ui.available_rect_before_wrap();
    let center = available.center();
    let painter = ui.painter();

    // Three rings, each a third of a cycle apart.
    for index in 0..3 {
        let phase = (time * 0.5 + index as f32 / 3.0) % 1.0;
        let radius = 18.0 + phase * 88.0;
        let alpha = ((1.0 - phase) * 140.0) as u8;
        painter.circle_stroke(
            center - egui::vec2(0.0, 20.0),
            radius,
            egui::Stroke::new(
                2.0,
                egui::Color32::from_rgba_unmultiplied(0x6c, 0xcb, 0x5f, alpha),
            ),
        );
    }

    painter.circle_filled(
        center - egui::vec2(0.0, 20.0),
        6.0,
        egui::Color32::from_rgb(0x6c, 0xcb, 0x5f),
    );
    painter.text(
        center + egui::vec2(0.0, 96.0),
        egui::Align2::CENTER_CENTER,
        "No Host Found",
        egui::FontId::proportional(22.0),
        egui::Color32::from_gray(210),
    );
    painter.text(
        center + egui::vec2(0.0, 124.0),
        egui::Align2::CENTER_CENTER,
        "Searching your network…",
        egui::FontId::proportional(14.0),
        egui::Color32::from_gray(130),
    );
}

/// One tappable host row. Returns true when tapped.
///
/// The row is an explicitly allocated rectangle rather than a group frame: a
/// frame's response does not reliably accept clicks when it contains labels.
fn host_row(ui: &mut egui::Ui, host: &DiscoveredHost, now: Instant, enabled: bool) -> bool {
    let age = host.age(now);
    let online = age < ONLINE_WINDOW;

    let desired = egui::vec2(ui.available_width(), ROW_HEIGHT);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(desired, sense);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let background = if response.hovered() && enabled {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().faint_bg_color
        };
        painter.rect_filled(rect, 8.0, background);

        let left = rect.left_center();
        painter.text(
            left + egui::vec2(16.0, -10.0),
            egui::Align2::LEFT_CENTER,
            &host.info.name,
            egui::FontId::proportional(16.0),
            egui::Color32::from_gray(235),
        );
        painter.text(
            left + egui::vec2(16.0, 12.0),
            egui::Align2::LEFT_CENTER,
            format!("{} · {}", host.info.platform, host.control_addr()),
            egui::FontId::proportional(12.0),
            egui::Color32::from_gray(145),
        );

        let right = rect.right_center();
        if online {
            painter.circle_filled(
                right - egui::vec2(18.0, 0.0),
                5.0,
                egui::Color32::from_rgb(0x6c, 0xcb, 0x5f),
            );
        } else {
            painter.text(
                right - egui::vec2(16.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                format!("{}s ago", age.as_secs()),
                egui::FontId::proportional(12.0),
                egui::Color32::from_gray(140),
            );
        }
    }

    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.add_space(8.0);
    enabled && response.clicked()
}

/// A one-line connection status under the list.
fn status_line(ui: &mut egui::Ui, connection: &Connection) {
    match connection {
        Connection::Idle => {}
        Connection::Connecting { host, note } => {
            ui.horizontal(|ui| {
                ui.spinner();
                match note {
                    Some(note) => ui.label(format!("Connecting to {host}… ({note})")),
                    None => ui.label(format!("Connecting to {host}…")),
                };
            });
        }
        Connection::Pairing { host, .. } => {
            ui.label(format!("Waiting to pair with {host}…"));
        }
        Connection::Connected { session } => {
            ui.label(format!("Connected to {}", session.host.name));
        }
        Connection::Failed { host, reason } => {
            ui.colored_label(ui.visuals().error_fg_color, format!("{host}: {reason}"));
        }
    }
}

/// Host name, frame rate and a way back, drawn over the video.
fn stream_overlay(
    ui: &egui::Ui,
    rect: egui::Rect,
    host: Option<&str>,
    fps: f32,
    connection: &Connection,
) {
    let text = match (host, connection) {
        (Some(host), _) => format!("{host} · {fps:.0} fps · back to exit"),
        (None, _) => "back to exit".to_owned(),
    };

    let padding = egui::vec2(10.0, 4.0);
    let galley = ui.painter().layout_no_wrap(
        text,
        egui::FontId::proportional(13.0),
        egui::Color32::from_gray(220),
    );
    let origin = rect.left_top() + egui::vec2(12.0, 12.0);
    let banner = egui::Rect::from_min_size(origin, galley.size() + padding * 2.0);

    ui.painter()
        .rect_filled(banner, 6.0, egui::Color32::from_black_alpha(160));
    ui.painter()
        .galley(origin + padding, galley, egui::Color32::WHITE);
}

fn reject_text(reason: RejectReason) -> String {
    match reason {
        RejectReason::ProtocolMismatch => "incompatible protocol version".to_owned(),
        RejectReason::Busy => "host is busy".to_owned(),
        RejectReason::Unauthorized => "not authorised".to_owned(),
    }
}
