//! The client UI: a list of hosts discovered on the local network.
//!
//! The screen is intentionally read-only for now — selecting a host will open
//! the (future) display stream.

use std::time::{Duration, Instant};

use eframe::egui;
use vd_core::discovery::{Browser, DiscoveredHost, DiscoveryConfig, DiscoveryEvent};

/// How often the discovery socket is serviced while the UI is running.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often egui should repaint even without input (keeps the list fresh).
const REPAINT_INTERVAL: Duration = Duration::from_millis(250);

/// eframe application showing the discovered hosts.
pub struct VdClientApp {
    browser: Option<Browser>,
    browser_error: Option<String>,
    last_poll: Instant,
}

impl VdClientApp {
    /// Creates the app and binds the discovery socket.
    pub fn new() -> Self {
        let config = DiscoveryConfig::default();
        match Browser::bind(config) {
            Ok(browser) => {
                log::info!(
                    "discovery browsing on {}",
                    browser
                        .local_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default()
                );
                Self {
                    browser: Some(browser),
                    browser_error: None,
                    last_poll: Instant::now(),
                }
            }
            Err(err) => {
                log::error!("could not start discovery: {err}");
                Self {
                    browser: None,
                    browser_error: Some(err.to_string()),
                    last_poll: Instant::now(),
                }
            }
        }
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

    fn scan_now(&mut self) {
        if let Some(browser) = self.browser.as_mut() {
            // Errors are non-fatal: we simply try again on the next tick.
            let _ = browser.scan(Instant::now());
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
        self.poll_discovery();
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);

        let now = Instant::now();
        let mut hosts: Vec<DiscoveredHost> = self
            .browser
            .as_ref()
            .map(|browser| browser.hosts().cloned().collect())
            .unwrap_or_default();
        hosts.sort_by(|a, b| a.info.name.cmp(&b.info.name));

        let mut scan_requested = false;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading("Virtual Display");
            ui.label("Hosts on your local network");

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
                    host_card(ui, host, now);
                }
            });
        });

        if scan_requested {
            self.scan_now();
        }
    }
}

fn host_card(ui: &mut egui::Ui, host: &DiscoveredHost, now: Instant) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong(&host.info.name);
            ui.label(format!("· {}", host.info.platform));
        });
        ui.label(format!("Address: {}", host.control_addr()));
        ui.label(format!(
            "Last seen: {:.1}s ago · v{}",
            host.age(now).as_secs_f32(),
            host.info.app_version
        ));
    });
}
