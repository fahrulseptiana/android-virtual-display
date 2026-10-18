//! Headless end-to-end probe: discover a host, open a control session and keep
//! it alive (watch heartbeats and reconnection happen).
//!
//! ```text
//! cargo run -p vd-client --example connect           # hold for 10s
//! VD_HOLD=30 cargo run -p vd-client --example connect
//! ```
//!
//! Useful for verifying the control channel without a GUI or a device.

use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use vd_client::connection::{self, ConnectionEvent};
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
    let handle = connection::connect(host.control_addr(), host.info.clone(), events_tx);

    let deadline = Instant::now() + hold;
    while Instant::now() < deadline {
        match events_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ConnectionEvent::PairingRequired { host, pin }) => {
                println!("  pairing with {host} — PIN {pin:06} (auto-approving)");
                handle.decide_pairing(true);
            }
            Ok(event) => println!("  {event:?}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}
