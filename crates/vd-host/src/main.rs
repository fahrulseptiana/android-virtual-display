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
mod displays;
mod dxgi;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod gpu;
mod identity;
#[cfg(windows)]
mod single_instance;
#[cfg(windows)]
mod tray;
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

/// Name of the mutex that keeps one host per machine.
#[cfg(windows)]
const INSTANCE_MUTEX: &str = "id.fahrul.vd.host";

fn main() -> vd_core::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Before anything binds a port or takes the single-instance guard: this is a
    // question about the machine, not a session, and it should answer even while
    // a host is already running.
    #[cfg(windows)]
    if std::env::var_os("VD_LIST_OUTPUTS").is_some() {
        list_outputs();
        return Ok(());
    }

    // Diagnostic: encode a few synthetic frames and report, so the hardware
    // encoder can be verified without a client.
    #[cfg(windows)]
    if std::env::var_os("VD_ENCODER_CHECK").is_some() {
        let (width, height, _) = video_settings();
        match gpu::create_device() {
            Ok(gpu) => encoder_check(&gpu, width, height),
            Err(err) => eprintln!("{err}"),
        }
        return Ok(());
    }

    // A second host would fight the first over the discovery and control ports,
    // and would announce the same machine twice.
    #[cfg(windows)]
    let _instance = match single_instance::SingleInstance::acquire(INSTANCE_MUTEX) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            error!("VD Host is already running; only one instance is allowed");
            return Ok(());
        }
        // Not being able to create the mutex is not a reason to refuse to run.
        Err(err) => {
            warn!("could not check for another instance: {err}");
            None
        }
    };

    let config = DiscoveryConfig::default();
    let host = HostInfo::new(identity::load_or_create_host_id(), identity::host_name());
    let mut responder = Responder::bind(config.clone(), host.clone())?;

    // One capture thread feeds every client. H.264 is used when asked for and an
    // encoder can be built; otherwise the pipeline falls back to JPEG.
    let (width, height, fps) = video_settings();
    let broadcaster = video::Broadcaster::new();
    // One clock for the whole host: frame timestamps and heartbeat replies must
    // be comparable, or a client cannot tell how old a frame is.
    let clock = vd_core::Clock::new();
    // The starting display, and then whichever one a client asks for. `VD_OUTPUT`
    // is where a host driven by hand begins, not what it is stuck with.
    let first_output = initial_output();
    let make_source = source_factory(width, height, clock.clone());
    let source = make_source(first_output);
    let encoder = hardware_encoder_factory();
    let capabilities = host_capabilities(encoder.is_some());
    let capture = video::spawn_capture(
        broadcaster.clone(),
        source,
        first_output,
        make_source,
        fps,
        (width, height),
        encoder,
    );

    let identity_key = identity::load_or_create_identity_key()?;
    let control = control::ControlServer::bind(
        host.clone(),
        capabilities,
        broadcaster.clone(),
        identity_key,
        clock,
        capture,
    )?;

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

    let control_addr = control.local_addr().map(|addr| addr.port()).unwrap_or(0);

    // The services run on their own thread so the main thread can own the tray
    // icon's message loop.
    let services_running = running.clone();
    let services = std::thread::spawn(move || {
        let mut responder = responder;
        let control = control;
        while services_running.load(Ordering::Relaxed) {
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
    });

    run_until_stopped(&running, control_addr);

    running.store(false, Ordering::Relaxed);
    let _ = services.join();
    Ok(())
}

/// Prints every display output, and whether Desktop Duplication works on it.
///
/// A diagnostic rather than a feature, and the answer to a question that matters
/// more than any other for virtual displays: DXGI enumerates outputs *per
/// adapter*, and an indirect display driver reports an adapter of its own, so a
/// monitor added by one is invisible to anything that only looks at the GPU.
#[cfg(windows)]
fn list_outputs() {
    match dxgi::enumerate_adapters() {
        Ok(adapters) => {
            for adapter in &adapters {
                println!(
                    "adapter {} \"{}\" vendor={:#06x} device={:#06x} software={} outputs={}",
                    adapter.index,
                    adapter.name,
                    adapter.vendor_id,
                    adapter.device_id,
                    adapter.software,
                    adapter.outputs,
                );
            }
        }
        Err(err) => eprintln!("could not enumerate adapters: {err}"),
    }

    match dxgi::enumerate_outputs() {
        Ok(outputs) if outputs.is_empty() => println!("no outputs found"),
        Ok(outputs) => {
            for (index, output) in outputs.iter().enumerate() {
                let verdict = match dxgi::probe_duplication(output.adapter, output.output) {
                    Ok((width, height)) => format!("duplication ok, {width}x{height}"),
                    Err(err) => format!("duplication FAILED: {err}"),
                };
                println!(
                    "[{index}] adapter {} ({}) output {} ({}) {}x{} attached={} -> {verdict}",
                    output.adapter,
                    output.adapter_name,
                    output.output,
                    output.device_name,
                    output.width,
                    output.height,
                    output.attached,
                );
            }
        }
        Err(err) => eprintln!("could not enumerate outputs: {err}"),
    }
}

/// Shows the tray icon and waits until the host is asked to stop.
#[cfg(windows)]
fn run_until_stopped(running: &Arc<AtomicBool>, control_port: u16) {
    let tooltip = format!("VD Host — running, control port {control_port}");
    if let Err(err) = tray::run(running.clone(), &tooltip) {
        warn!("could not show the tray icon ({err}); falling back to Ctrl+C");
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
        }
    }
}

/// Waits until the host is asked to stop.
#[cfg(not(windows))]
fn run_until_stopped(running: &Arc<AtomicBool>, _control_port: u16) {
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
    }
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
/// The display output to capture before any client has an opinion, from
/// `VD_OUTPUT`.
fn initial_output() -> u32 {
    std::env::var("VD_OUTPUT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Builds capture sources, one per display output.
///
/// One device for all of them, so switching display does not cross GPU
/// boundaries, and one clock, so frame timestamps stay comparable with the ones
/// the control channel hands out.
///
/// Never fails: a display that cannot be duplicated falls back to the test
/// pattern and says so. A client that asked for a display and silently got a
/// pattern would be a worse outcome than a log line explaining it.
fn source_factory(width: u32, height: u32, clock: vd_core::Clock) -> video::SourceFactory {
    #[cfg(windows)]
    {
        let gpu = match gpu::create_device() {
            Ok(gpu) => Some(gpu),
            Err(err) => {
                warn!("{err}; using the test pattern");
                None
            }
        };

        Box::new(move |output: u32| -> Box<dyn FrameSource + Send> {
            if let Some(gpu) = gpu.as_ref() {
                match dxgi::DxgiCapture::new(
                    gpu.device.clone(),
                    gpu.context.clone(),
                    output,
                    clock.clone(),
                ) {
                    Ok(capture) => return Box::new(capture),
                    Err(err) => warn!("display {output}: {err}; using the test pattern"),
                }
            }
            Box::new(TestPattern::new(width, height, clock.clone()))
        })
    }

    #[cfg(not(windows))]
    {
        Box::new(move |_output: u32| -> Box<dyn FrameSource + Send> {
            Box::new(TestPattern::new(width, height, clock.clone()))
        })
    }
}

fn hardware_encoder_factory() -> Option<video::EncoderFactory> {
    if std::env::var("VD_CODEC").as_deref() == Ok("jpeg") {
        return None;
    }

    Some(std::sync::Arc::new(|width, height, fps| {
        let gpu = gpu::create_device().map_err(|err| err.to_string())?;
        let preference = encoder_preference();
        let encoder = encoder::h264::H264Encoder::new(
            &gpu.device,
            width,
            height,
            fps,
            video_bitrate(),
            &preference,
        )
        .map_err(|err| err.to_string())?;
        Ok(Box::new(encoder) as Box<dyn video::VideoEncoder>)
    }))
}

#[cfg(not(windows))]
fn hardware_encoder_factory() -> Option<video::EncoderFactory> {
    None
}

/// Encoder choice from `VD_ENCODER`: `hardware` (default), `software`, or a
/// friendly-name substring such as `dx12`.
#[cfg(windows)]
fn encoder_preference() -> encoder::media_foundation::EncoderPreference {
    use encoder::media_foundation::EncoderPreference;
    match std::env::var("VD_ENCODER").as_deref() {
        Ok("software") => EncoderPreference::SoftwareFirst,
        Ok("hardware") | Err(_) => EncoderPreference::HardwareFirst,
        Ok(name) => EncoderPreference::Named(name.to_owned()),
    }
}

/// Diagnostic: encode a few synthetic frames and report, so the hardware encoder
/// can be verified without a client.
#[cfg(windows)]
fn encoder_check(gpu: &gpu::GpuDevice, width: u32, height: u32) {
    // NV12 subsamples chroma 2x2, so the dimensions must be even.
    let (width, height) = (width & !1, height & !1);

    let preference = encoder_preference();
    let mut encoder = match encoder::h264::H264Encoder::new(
        &gpu.device,
        width,
        height,
        30,
        8_000_000,
        &preference,
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

/// Encoder target bitrate, overridable with `VD_VIDEO_BITRATE` (bits per second).
#[cfg(windows)]
fn video_bitrate() -> u32 {
    std::env::var("VD_VIDEO_BITRATE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|bitrate| *bitrate >= 100_000)
        .unwrap_or(8_000_000)
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
