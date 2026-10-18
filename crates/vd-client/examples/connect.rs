//! Headless end-to-end probe: discover a host, then open a control session.
//!
//! ```text
//! cargo run -p vd-client --example connect
//! ```
//!
//! Useful for verifying the control channel without a GUI or a device.

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use vd_client::connection::{self, ConnectionEvent};
use vd_core::discovery::{Browser, DiscoveryConfig, DiscoveryEvent};

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
        "discovered \"{}\" at {} — connecting…",
        host.info.name,
        host.control_addr()
    );

    let (events_tx, events_rx) = channel();
    connection::connect(host.control_addr(), host.info.clone(), events_tx);

    match events_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(ConnectionEvent::Established(session)) => println!(
            "established: protocol v{}, codec {}",
            session.protocol,
            session
                .codec
                .map(|codec| format!("{codec:?}"))
                .unwrap_or_else(|| "none".to_owned())
        ),
        Ok(other) => println!("connection ended: {other:?}"),
        Err(err) => println!("no response: {err}"),
    }

    Ok(())
}
