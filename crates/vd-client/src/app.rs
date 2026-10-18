//! The client UI: discover hosts on the local network and open a control
//! connection to one of them.

use std::net::SocketAddr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use vd_core::discovery::{Browser, DiscoveredHost, DiscoveryConfig, DiscoveryEvent};
use vd_core::{HostInfo, RejectReason, Session};

use crate::connection::{self, ConnectionEvent};

/// How often the discovery socket is serviced while the UI is running.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often egui should repaint even without input (keeps the list fresh).
const REPAINT_INTERVAL: Duration = Duration::from_millis(250);

/// State of the control connection.
enum Connection {
    Idle,
    Connecting { host: String, note: Option<String> },
    Connected { session: Box<Session> },
    Failed { host: String, reason: String },
}

impl Connection {
    /// Whether a connection is in progress or already open.
    fn is_busy(&self) -> bool {
        matches!(self, Self::Connecting { .. } | Self::Connected { .. })
    }
}

/// eframe application showing the discovered hosts.
pub struct VdClientApp {
    browser: Option<Browser>,
    browser_error: Option<String>,
    last_poll: Instant,
    events_tx: Sender<ConnectionEvent>,
    events_rx: Receiver<ConnectionEvent>,
    connection: Connection,
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
                ConnectionEvent::Established(session) => Connection::Connected { session },
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
        connection::connect(addr, host, self.events_tx.clone());
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
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);

        let now = Instant::now();
        let mut hosts: Vec<DiscoveredHost> = self
            .browser
            .as_ref()
            .map(|browser| browser.hosts().cloned().collect())
            .unwrap_or_default();
        hosts.sort_by(|a, b| a.info.name.cmp(&b.info.name));

        let connection = &self.connection;
        let connect_enabled = !connection.is_busy();
        let mut scan_requested = false;
        let mut connect_to: Option<(SocketAddr, HostInfo)> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Virtual Display");
            ui.label("Hosts on your local network");

            ui.add_space(8.0);
            connection_status(ui, connection);

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

        if scan_requested {
            self.scan_now();
        }
        if let Some((addr, host)) = connect_to {
            self.start_connection(addr, host);
        }
    }
}

fn connection_status(ui: &mut egui::Ui, connection: &Connection) {
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
        Connection::Connected { session } => {
            ui.colored_label(
                egui::Color32::from_rgb(0x6c, 0xcb, 0x5f),
                format!("Connected to {}", session.host.name),
            );
            ui.label(format!(
                "protocol v{} · codec {}",
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
