//! Headless end-to-end probe: discover a host, pair, and report the video stream.
//!
//! ```text
//! cargo run -p vd-client --example connect           # hold for 10s
//! VD_HOLD=30 cargo run -p vd-client --example connect
//! ```
//!
//! Useful for verifying the whole pipeline without a GUI: it prints how many
//! frames arrive and at what rate.

use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use vd_client::connection::{self, ConnectionEvent};
use vd_client::frames::FrameQueue;
use vd_client::latency::Latency;
use vd_client::trust;
use vd_client::visibility::Visibility;
use vd_core::discovery::{Browser, DiscoveryConfig, DiscoveryEvent};

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_HOLD: Duration = Duration::from_secs(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let hold = std::env::var("VD_HOLD")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HOLD);

    let mut browser = Browser::bind(DiscoveryConfig::default())?;
    let deadline = Instant::now() + DISCOVER_TIMEOUT;

    let mut target = None;
    while Instant::now() < deadline && target.is_none() {
        for event in browser.poll(Instant::now())? {
            if let DiscoveryEvent::Found(host) = event {
                target = Some(host);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let Some(host) = target else {
        println!("no host discovered within {DISCOVER_TIMEOUT:?}");
        return Ok(());
    };

    println!(
        "discovered \"{}\" at {} — connecting for {hold:?}…",
        host.info.name,
        host.control_addr()
    );

    let (events_tx, events_rx) = channel();
    let frames = FrameQueue::new();
    let latency = Latency::new();
    let sinks = connection::Sinks::new(events_tx, frames.clone(), latency, Visibility::new());
    let handle = connection::connect(
        host.control_addr(),
        host.info.clone(),
        sinks,
        trust::default_path(),
    );

    let deadline = Instant::now() + hold;
    let mut last_sequence = None;
    let mut received = 0u64;

    while Instant::now() < deadline {
        match events_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ConnectionEvent::PairingRequired { host, pin, .. }) => {
                println!("  pairing with {host} — PIN {pin:06} (auto-approving)");
                handle.decide_pairing(true);
            }
            Ok(event) => println!("  {event:?}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(frame) = frames.latest() {
            if last_sequence != Some(frame.sequence) {
                last_sequence = Some(frame.sequence);
                received += 1;
                if received % 30 == 1 {
                    println!(
                        "  frame #{}: {}x{} ({} bytes decoded)",
                        frame.sequence,
                        frame.width,
                        frame.height,
                        frame.pixels.len()
                    );
                }
            }
        }
    }

    println!("received ~{received} distinct frames");
    Ok(())
}
