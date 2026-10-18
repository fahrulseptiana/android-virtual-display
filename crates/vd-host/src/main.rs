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
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod gpu;
mod identity;
mod video;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use vd_core::capture::{FrameSource, TestPattern};
use vd_core::discovery::{DiscoveryConfig, Responder};
use vd_core::{Capabilities, HostInfo};

/// How often the responder is serviced.
const TICK: Duration = Duration::from_millis(100);

fn main() -> vd_core::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = DiscoveryConfig::default();
    let host = HostInfo::new(identity::load_or_create_host_id(), identity::host_name());
    let mut responder = Responder::bind(config.clone(), host.clone())?;

    // One capture thread feeds every client. H.264 is used when asked for and an
    // encoder can be built; otherwise the pipeline falls back to JPEG.
    let (width, height, fps) = video_settings();
    let broadcaster = video::Broadcaster::new();
    let source = capture_source(width, height);
    let encoder = hardware_encoder_factory();
    let capabilities = host_capabilities(encoder.is_some());
    video::spawn_capture(broadcaster.clone(), source, fps, (width, height), encoder);

    let control = control::ControlServer::bind(host.clone(), capabilities, broadcaster.clone())?;

    info!(
        "host \"{}\" ({}) ready — id {}, discovery udp/{}",
        host.name, host.platform, host.id, config.port
    );
    info!(
        "control channel listening on tcp/{}",
        control.local_addr()?.port()
    );
    info!("waiting for clients on the local network (Ctrl+C to stop)");

    #[cfg(windows)]
    log_encoders();

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

/// What this host can do.
///
/// H.264 is only advertised when an encoder is actually available, so the
/// negotiated codec matches what will really be sent. Input forwarding is
/// planned.
fn host_capabilities(advertise_h264: bool) -> Capabilities {
    let codecs = if advertise_h264 {
        vec![vd_core::VideoCodec::H264]
    } else {
        Vec::new()
    };
    Capabilities {
        codecs,
        ..Capabilities::default()
    }
}

/// Picks the best available capture source: DXGI on Windows, else a test pattern.
///
/// `VD_OUTPUT` selects the monitor index (default 0).
fn capture_source(width: u32, height: u32) -> Box<dyn FrameSource + Send> {
    #[cfg(windows)]
    {
        let output = std::env::var("VD_OUTPUT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        // Capture and encoding share one device so frames never cross GPU boundaries.
        match gpu::create_device() {
            Ok(gpu) => {
                if std::env::var_os("VD_ENCODER_CHECK").is_some() {
                    encoder_check(&gpu, width, height);
                    std::process::exit(0);
                }
                match dxgi::DxgiCapture::new(gpu.device.clone(), gpu.context.clone(), output) {
                    Ok(capture) => return Box::new(capture),
                    Err(err) => warn!("DXGI capture unavailable ({err}); using the test pattern"),
                }
            }
            Err(err) => warn!("{err}; using the test pattern"),
        }
    }
    Box::new(TestPattern::new(width, height))
}

/// Builds the H.264 encoder factory.
///
/// H.264 is the default; `VD_CODEC=jpeg` forces the interim JPEG path, and a
/// machine without a usable encoder falls back to it automatically.
#[cfg(windows)]
fn hardware_encoder_factory() -> Option<video::EncoderFactory> {
    if std::env::var("VD_CODEC").as_deref() == Ok("jpeg") {
        return None;
    }

    Some(Box::new(|width, height, fps| {
        let gpu = gpu::create_device().map_err(|err| err.to_string())?;
        let prefer_hardware = std::env::var("VD_ENCODER").as_deref() != Ok("software");
        let encoder = encoder::h264::H264Encoder::new(
            &gpu.device,
            width,
            height,
            fps,
            8_000_000,
            prefer_hardware,
        )
        .map_err(|err| err.to_string())?;
        Ok(Box::new(encoder) as Box<dyn video::VideoEncoder>)
    }))
}

#[cfg(not(windows))]
fn hardware_encoder_factory() -> Option<video::EncoderFactory> {
    None
}

/// Diagnostic: encode a few synthetic frames and report, so the hardware encoder
/// can be verified without a client.
#[cfg(windows)]
fn encoder_check(gpu: &gpu::GpuDevice, width: u32, height: u32) {
    // NV12 subsamples chroma 2x2, so the dimensions must be even.
    let (width, height) = (width & !1, height & !1);

    let prefer_hardware = std::env::var("VD_ENCODER").as_deref() != Ok("software");
    let mut encoder = match encoder::h264::H264Encoder::new(
        &gpu.device,
        width,
        height,
        30,
        8_000_000,
        prefer_hardware,
    ) {
        Ok(encoder) => encoder,
        Err(err) => {
            warn!("encoder check failed: {err}");
            return;
        }
    };
    info!("encoder check: {}", encoder.describe());

    let frames = 30u64;
    let mut units = 0usize;
    let mut bytes = 0usize;
    for index in 0..frames {
        // A slightly changing flat field, so the encoder has something to do.
        let luma = 16 + (index % 200) as u8;
        let frame = vec![luma; (width * height * 3 / 2) as usize];
        match encoder.encode(&frame, index * 33_333) {
            Ok(outputs) => {
                for unit in outputs {
                    units += 1;
                    bytes += unit.len();
                }
            }
            Err(err) => {
                warn!("encoder check failed while encoding: {err}");
                return;
            }
        }
    }

    info!(
        "encoder check: {units} access units, {bytes} bytes for {frames} frames, extradata {} bytes",
        encoder.sequence_header().len()
    );
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

/// Logs the H.264 encoders Media Foundation offers (hardware first).
#[cfg(windows)]
fn log_encoders() {
    match encoder::media_foundation::h264_encoders() {
        Ok(encoders) if encoders.is_empty() => warn!("no H.264 encoders found"),
        Ok(encoders) => {
            for info in &encoders {
                info!(
                    "H.264 encoder: {} ({})",
                    info.name,
                    if info.hardware {
                        "hardware"
                    } else {
                        "software"
                    }
                );
            }
        }
        Err(err) => warn!("could not enumerate H.264 encoders: {err}"),
    }
}

fn install_ctrlc_handler(running: Arc<AtomicBool>) {
    if let Err(err) = ctrlc::set_handler(move || {
        info!("shutdown requested");
        running.store(false, Ordering::Relaxed);
    }) {
        error!("could not install Ctrl+C handler: {err}");
    }
}
