//! Headless discovery probe.
//!
//! Prints every host found on the local network for a few seconds. Useful to
//! debug discovery without launching the GUI:
//!
//! ```text
//! cargo run -p vd-client --example discover
//! ```

use std::time::{Duration, Instant};

use vd_core::discovery::{Browser, DiscoveryConfig, DiscoveryEvent};

const DURATION: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut browser = Browser::bind(DiscoveryConfig::default())?;
    println!("browsing for {DURATION:?}…");

    let start = Instant::now();
    let mut found = 0usize;

    while start.elapsed() < DURATION {
        for event in browser.poll(Instant::now())? {
            match event {
                DiscoveryEvent::Found(host) => {
                    found += 1;
                    println!(
                        "found {} at {} ({} v{})",
                        host.info.name,
                        host.control_addr(),
                        host.info.platform,
                        host.info.app_version
                    );
                }
                DiscoveryEvent::Lost(id) => println!("lost {id}"),
            }
        }
        std::thread::sleep(TICK);
    }

    println!("done — {found} host(s)");
    Ok(())
}
