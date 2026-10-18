//! The client UI.
//!
//! Two views: a browser (searching for hosts, then listing them) and a
//! full-screen stream. The Android back button returns from the stream to the
//! list.

use std::net::SocketAddr;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use log::info;
use vd_core::discovery::{Browser, DiscoveredHost, DiscoveryConfig, DiscoveryEvent};
use vd_core::{DisplayInfo, DisplayKind, HostId, HostInfo, RejectReason, Session};

use crate::connection::{self, ConnectionEvent, ConnectionHandle};
use crate::frames::FrameQueue;
use crate::latency::Latency;
use crate::visibility::Visibility;

/// How often the discovery socket is serviced while the UI is running.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often egui should repaint even without input: keeps the ripple animation
/// smooth and drains the frame queue promptly.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);
/// How often the accumulated frame timings are written to the log.
const LATENCY_REPORT_INTERVAL: Duration = Duration::from_secs(5);

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
        /// The host was known but presented a different identity key.
        identity_changed: bool,
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
    /// Events from the current connection.
    ///
    /// A fresh channel per connection means dropping this drops any stale
    /// events along with it: a worker that is still winding down cannot put the
    /// UI back into a "busy" state after the user has left.
    events_rx: Option<Receiver<ConnectionEvent>>,
    connection: Connection,
    /// The live connection, used to answer pairing requests and to disconnect.
    handle: Option<ConnectionHandle>,
    /// Where known host identities are remembered.
    trust_path: Option<std::path::PathBuf>,
    /// The host we are currently trying to reach, so discovery can tell us the
    /// moment it comes back.
    retry_target: Option<HostId>,
    /// How long frames are taking, measured by the connection and shown here.
    latency: Latency,
    /// What the host says it could show, and which of those it is showing.
    displays: Vec<DisplayInfo>,
    current_display: u32,
    /// When to write the next latency summary to the log.
    next_latency_report: Instant,
    /// When the drawn-frame count was last reset, for the rate in that summary.
    last_rate_report: Instant,
    /// Frames dropped as of the last report, so the delta can be shown.
    last_dropped: u64,
    /// Repaints since the last report: the ceiling on frames that can be drawn.
    repaints: u64,
    /// When the UI last drew, so video can be paused when it stops.
    visibility: Visibility,
    /// Decoded frames waiting to be drawn.
    frames: FrameQueue,
    texture: Option<egui::TextureHandle>,
    frames_seen: u64,
}

impl VdClientApp {
    /// Creates the app and binds the discovery socket.
    pub fn new(trust_path: Option<std::path::PathBuf>) -> Self {
        let mut app = Self {
            trust_path,
            retry_target: None,
            latency: Latency::new(),
            displays: Vec::new(),
            current_display: 0,
            next_latency_report: Instant::now() + LATENCY_REPORT_INTERVAL,
            last_rate_report: Instant::now(),
            last_dropped: 0,
            repaints: 0,
            visibility: Visibility::new(),
            view: View::Browser,
            browser: None,
            browser_error: None,
            last_poll: Instant::now(),
            events_rx: None,
            connection: Connection::Idle,
            handle: None,
            frames: FrameQueue::new(),
            texture: None,
            frames_seen: 0,
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

    /// Drains connection events reported by the current worker.
    fn poll_connection(&mut self) {
        let Some(events) = self.events_rx.as_ref() else {
            return;
        };
        while let Ok(event) = events.try_recv() {
            self.connection = match event {
                ConnectionEvent::Connecting { host, note } => Connection::Connecting { host, note },
                ConnectionEvent::PairingRequired {
                    host,
                    pin,
                    identity_changed,
                } => Connection::Pairing {
                    host,
                    pin,
                    identity_changed,
                },
                ConnectionEvent::Established(session) => {
                    self.retry_target = None;
                    // Straight to full screen once the session is up.
                    self.view = View::Stream;
                    Connection::Connected { session }
                }
                ConnectionEvent::Displays { current, outputs } => {
                    for output in &outputs {
                        log::info!("host display {}: {}", output.index, output.summary());
                    }
                    self.current_display = current;
                    self.displays = outputs;
                    continue;
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

    /// Uploads the newest queued frame as a texture.
    ///
    /// Newest rather than oldest: anything else on the queue has already been
    /// overtaken, so drawing it would show the past for no gain. The frames
    /// skipped over are counted, and a visible rise in that count is the signal
    /// that this device cannot keep up with the stream.
    fn refresh_texture(&mut self, ctx: &egui::Context) {
        // The ages are needed to tell a decoder's burst from a backlog, and
        // only this side knows how the host's clock relates to ours.
        let latency = self.latency.clone();
        let Some(frame) = self
            .frames
            .take_for_display(|timestamp_us| latency.age_us(timestamp_us))
        else {
            return;
        };
        // Age at the moment it is handed over, which is as close to "on screen"
        // as software can measure.
        self.latency.record_display(frame.timestamp_us);

        let started = Instant::now();
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
        self.latency.record_render(started.elapsed());
        self.frames_seen += 1;
    }

    /// Offers mirror or extend, when the host has a screen worth extending onto.
    ///
    /// Mirror means the screen the host is already using, extend means the one a
    /// virtual display driver added, and which is which is the host's answer
    /// rather than a guess here. Offered over the stream rather than before it
    /// because the host's list only arrives with the session.
    fn show_display_choice(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        if !matches!(self.connection, Connection::Connected { .. }) || self.displays.is_empty() {
            return;
        }

        let mirror = self.displays.iter().find(|d| d.primary).map(|d| d.index);
        let extend = self
            .displays
            .iter()
            .find(|d| d.kind == DisplayKind::Virtual)
            .map(|d| d.index);
        let current = self.current_display;

        let mut chosen = None;
        egui::Area::new(egui::Id::unique("display-choice"))
            .fixed_pos(rect.right_top() + egui::vec2(-232.0, 10.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(index) = mirror {
                            let active = current == index;
                            if ui.selectable_label(active, "Mirror").clicked() && !active {
                                chosen = Some(index);
                            }
                        }
                        match extend {
                            Some(index) => {
                                let active = current == index;
                                if ui.selectable_label(active, "Extend").clicked() && !active {
                                    chosen = Some(index);
                                }
                            }
                            None => {
                                ui.add_enabled(false, egui::Button::new("Extend"))
                                    .on_disabled_hover_text(
                                        "This host has no virtual display to extend onto",
                                    );
                            }
                        }
                    });
                });
            });

        if let Some(index) = chosen {
            if let Some(handle) = self.handle.as_ref() {
                info!("asking the host for display {index}");
                handle.select_display(index);
            }
        }
    }

    /// Writes the timings measured so far to the log, every few seconds.
    ///
    /// Reported from here rather than from the connection because the whole
    /// picture — including how many frames were dropped before they could be
    /// drawn — only exists on this side of the frame queue.
    fn report_latency(&mut self) {
        let now = Instant::now();
        if now < self.next_latency_report {
            return;
        }
        self.next_latency_report = now + LATENCY_REPORT_INTERVAL;

        let seconds = (now - self.last_rate_report).as_secs_f32();
        let drawn = std::mem::take(&mut self.frames_seen);
        let repaints = std::mem::take(&mut self.repaints);
        self.last_rate_report = now;

        // The queue's counter is for the whole session; what matters here is how
        // many were dropped during this period.
        let total_dropped = self.frames.dropped();
        let dropped = total_dropped.saturating_sub(self.last_dropped);
        self.last_dropped = total_dropped;

        let Some(readout) = self.latency.summarise(dropped) else {
            return;
        };
        let per_second = |count: u64| {
            if seconds > 0.0 {
                count as f32 / seconds
            } else {
                0.0
            }
        };
        info!(
            "frame latency: arrived {:.1} ms, drawn {:.1} ms (p95 {:.1} ms),              decode {:.1} ms, render {:.1} ms — {:.1} fps drawn, {:.1} dropped,              {:.1} repaints/s",
            readout.arrival.mean_ms(),
            readout.display.mean_ms(),
            readout.display.p95_ms(),
            readout.decode.mean_ms(),
            readout.render.mean_ms(),
            per_second(drawn),
            per_second(dropped),
            per_second(repaints),
        );
    }

    /// Leaves the stream and returns to the host list.
    fn leave_stream(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.disconnect();
        }
        self.handle = None;
        // Dropping the receiver also tells the worker to stop: its next send
        // fails, and it exits rather than retrying behind the user's back.
        self.events_rx = None;
        self.retry_target = None;
        self.displays.clear();
        self.current_display = 0;
        self.connection = Connection::Idle;
        self.view = View::Browser;
        self.frames.clear();
        self.texture = None;
    }

    fn start_connection(&mut self, addr: SocketAddr, host: HostInfo) {
        self.connection = Connection::Connecting {
            host: host.name.clone(),
            note: None,
        };
        self.retry_target = Some(host.id.clone());
        self.frames.clear();
        self.texture = None;

        // A channel of its own: whatever the previous worker still wants to say
        // reaches nobody, because we replace the receiver wholesale.
        let (events_tx, events_rx) = channel();
        self.events_rx = Some(events_rx);
        // A fresh measurement for a fresh connection: the clock offset from the
        // last one means nothing now.
        self.latency = Latency::new();
        self.next_latency_report = Instant::now() + LATENCY_REPORT_INTERVAL;
        self.last_dropped = 0;
        self.visibility.tick();
        let sinks = connection::Sinks::new(
            events_tx,
            self.frames.clone(),
            self.latency.clone(),
            self.visibility.clone(),
        );
        self.handle = Some(connection::connect(
            addr,
            host,
            sinks,
            self.trust_path.clone(),
        ));
    }

    /// Skips the rest of a retry delay when discovery shows the host is back.
    ///
    /// The client keeps probing for hosts whatever view it is on, so it usually
    /// learns that a restarted host is up within a couple of seconds — well
    /// before a backoff would have expired. There is no reason to wait.
    fn nudge_reconnect(&mut self) {
        if !matches!(self.connection, Connection::Connecting { .. }) {
            return;
        }
        let Some(target) = self.retry_target.clone() else {
            return;
        };
        let Some(browser) = self.browser.as_ref() else {
            return;
        };

        let now = Instant::now();
        let back_online = browser
            .hosts()
            .any(|host| host.info.id == target && host.age(now) < ONLINE_WINDOW);
        if back_online {
            if let Some(handle) = self.handle.as_ref() {
                handle.retry_now();
            }
        }
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
        Self::new(None)
    }
}

impl eframe::App for VdClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.repaints += 1;
        // Tells the connection that this app still has a screen to draw on.
        self.visibility.tick();
        self.poll_discovery();
        self.nudge_reconnect();
        self.poll_connection();
        self.refresh_texture(ui.ctx());
        self.report_latency();
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

        // Works in either view, so a mid-stream reconnect can still be approved.
        self.show_pairing(ui);
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

        if let Some((addr, host)) = connect_to {
            self.start_connection(addr, host);
        }
    }

    /// The pairing prompt, shown whichever view is up.
    ///
    /// It has to work while streaming too: a reconnect gives a fresh PIN, and
    /// without this the client would sit waiting for a decision it never asks
    /// for.
    fn show_pairing(&mut self, ui: &mut egui::Ui) {
        let (host, pin, identity_changed) = match &self.connection {
            Connection::Pairing {
                host,
                pin,
                identity_changed,
            } => (host.clone(), *pin, *identity_changed),
            _ => return,
        };

        let mut decision: Option<bool> = None;
        egui::Window::new("Pair device")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!("Pair with \"{host}\"?"));
                ui.add_space(6.0);
                ui.heading(format!("{pin:06}"));
                ui.label("Confirm this code to start the session.");
                if identity_changed {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "This host's identity has changed since you last paired.",
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                    if ui.button("Pair").clicked() {
                        decision = Some(true);
                    }
                });
            });

        if let Some(approved) = decision {
            if let Some(handle) = self.handle.as_ref() {
                handle.decide_pairing(approved);
            }
            if approved {
                self.connection = Connection::Connecting { host, note: None };
            } else {
                self.connection = Connection::Idle;
                self.handle = None;
            }
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

        // How old frames are when they arrive, shown while the session is live.
        // The mean says how it feels, the p95 says how bad it gets.
        if matches!(self.connection, Connection::Connected { .. }) {
            if let Some(readout) = self.latency.latest() {
                let text = format!(
                    "{:.0} ms · p95 {:.0}",
                    readout.display.mean_ms(),
                    readout.display.p95_ms()
                );
                let anchor = rect.left_top() + egui::vec2(10.0, 8.0);
                let painter = ui.painter();
                let size = painter.layout_no_wrap(
                    text.clone(),
                    egui::FontId::monospace(12.0),
                    egui::Color32::WHITE,
                );
                let box_rect =
                    egui::Rect::from_min_size(anchor, size.size()).expand2(egui::vec2(7.0, 4.0));
                painter.rect_filled(
                    box_rect,
                    5.0,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 130),
                );
                painter.text(
                    anchor,
                    egui::Align2::LEFT_TOP,
                    text,
                    egui::FontId::monospace(12.0),
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 200),
                );
            }
        }

        self.show_display_choice(ui, rect);

        // Anything but a live session is veiled, so a dropped connection can
        // never be mistaken for a frozen desktop.
        if !matches!(self.connection, Connection::Connected { .. }) {
            let (title, detail) = disconnected_text(&self.connection);
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(200));
            ui.painter().text(
                rect.center() + egui::vec2(0.0, -14.0),
                egui::Align2::CENTER_CENTER,
                title,
                egui::FontId::proportional(30.0),
                egui::Color32::from_gray(240),
            );
            ui.painter().text(
                rect.center() + egui::vec2(0.0, 24.0),
                egui::Align2::CENTER_CENTER,
                detail,
                egui::FontId::proportional(15.0),
                egui::Color32::from_gray(165),
            );
        }
    }
}

/// Headline and detail for the disconnected veil.
fn disconnected_text(connection: &Connection) -> (&'static str, String) {
    match connection {
        Connection::Connected { .. } => ("", String::new()),
        Connection::Idle => ("Disconnected", "The session has ended.".to_owned()),
        Connection::Connecting { host, note } => (
            "Disconnected",
            match note {
                Some(note) => format!("Reconnecting to {host}… ({note})"),
                None => format!("Reconnecting to {host}…"),
            },
        ),
        Connection::Pairing { host, .. } => ("Disconnected", format!("Pairing with {host}…")),
        Connection::Failed { host, reason } => ("Disconnected", format!("{host}: {reason}")),
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
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Pairing with {host}…"));
            });
        }
        Connection::Connected { session } => {
            ui.label(format!("Connected to {}", session.host.name));
        }
        Connection::Failed { host, reason } => {
            ui.colored_label(ui.visuals().error_fg_color, format!("{host}: {reason}"));
        }
    }
}

fn reject_text(reason: RejectReason) -> String {
    match reason {
        RejectReason::ProtocolMismatch => "incompatible protocol version".to_owned(),
        RejectReason::Busy => "host is busy".to_owned(),
        RejectReason::Unauthorized => "not authorised".to_owned(),
    }
}
