//! The Windows host.
//!
//! For now this only *announces* the machine on the local network so that the
//! Android client can discover it. The actual virtual-display video transport
//! is not implemented yet.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p vd-host
//! ```
//!
//! Set `RUST_LOG=debug` for verbose output and `VD_HOST_NAME=Foo` to override
//! the advertised name.

mod control;
mod identity;
mod video;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use vd_core::capture::TestPattern;
use vd_core::discovery::{DiscoveryConfig, Responder};
use vd_core::{Capabilities, HostInfo};

/// How often the responder is serviced.
const TICK: Duration = Duration::from_millis(100);

fn main() -> vd_core::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = DiscoveryConfig::default();
    let host = HostInfo::new(identity::load_or_create_host_id(), identity::host_name());
    let mut responder = Responder::bind(config.clone(), host.clone())?;

    // One capture thread feeds every client.
    let (width, height, fps) = video_settings();
    let broadcaster = video::Broadcaster::new();
    video::spawn_capture(
        broadcaster.clone(),
        Box::new(TestPattern::new(width, height)),
        fps,
    );

    let control =
        control::ControlServer::bind(host.clone(), host_capabilities(), broadcaster.clone())?;

    info!(
        "host \"{}\" ({}) ready — id {}, discovery udp/{}",
        host.name, host.platform, host.id, config.port
    );
    info!(
        "control channel listening on tcp/{}",
        control.local_addr()?.port()
    );
    info!("waiting for clients on the local network (Ctrl+C to stop)");

    let running = Arc::new(AtomicBool::new(true));
    install_ctrlc_handler(running.clone());

    // Tell the network we are here right away.
    responder.announce(Instant::now())?;

    while running.load(Ordering::Relaxed) {
        if let Err(err) = responder.poll(Instant::now()) {
            // A single bad datagram must not take the host down.
            warn!("discovery poll failed: {err}");
        }
        if let Err(err) = control.poll() {
            warn!("control accept failed: {err}");
        }
        std::thread::sleep(TICK);
    }

    if let Err(err) = responder.bye() {
        error!("failed to say goodbye: {err}");
    }
    debug!("bye sent; shutting down");
    Ok(())
}

/// What this host can do. Video capture/encode is not implemented yet, so no
/// codec is advertised; input forwarding is planned.
fn host_capabilities() -> Capabilities {
    Capabilities::default()
}

/// Stream geometry, overridable with `VD_VIDEO_SIZE=WxH` and `VD_VIDEO_FPS`.
fn video_settings() -> (u32, u32, u32) {
    let (mut width, mut height) = (video::DEFAULT_WIDTH, video::DEFAULT_HEIGHT);
    if let Ok(size) = std::env::var("VD_VIDEO_SIZE") {
        if let Some((w, h)) = size.split_once('x') {
            match (w.trim().parse(), h.trim().parse()) {
                (Ok(w), Ok(h)) => {
                    width = w;
                    height = h;
                }
                _ => warn!("ignoring malformed VD_VIDEO_SIZE={size:?}"),
            }
        }
    }

    let fps = std::env::var("VD_VIDEO_FPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(video::DEFAULT_FPS);

    (width, height, fps)
}

fn install_ctrlc_handler(running: Arc<AtomicBool>) {
    if let Err(err) = ctrlc::set_handler(move || {
        info!("shutdown requested");
        running.store(false, Ordering::Relaxed);
    }) {
        error!("could not install Ctrl+C handler: {err}");
    }
}
