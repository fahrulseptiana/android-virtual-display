//! Trust on first use.
//!
//! The host publishes a long-lived identity key. The first time a client pairs
//! with a host it records that key; later connections are accepted silently
//! while the key matches, and are challenged again if it ever changes.
//!
//! Two things follow from this. Reconnecting — including after the host is
//! restarted — needs no PIN prompt, which is what makes it seamless. And the
//! session key is mixed with the host's identity, so a host that does not hold
//! the matching private key cannot complete the handshake at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use log::{debug, warn};
use vd_core::session::pairing::{key_from_hex, key_to_hex};
use vd_core::HostId;

const FILE_NAME: &str = "known-hosts";

/// A remembered set of host identities.
pub struct TrustStore {
    /// Where to persist, if anywhere.
    path: Option<PathBuf>,
    known: HashMap<HostId, [u8; 32]>,
}

impl TrustStore {
    /// Loads a store from `path`, or starts empty if there is nothing there.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            path,
            known: HashMap::new(),
        };
        store.read();
        store
    }

    /// An empty store that will not be persisted (used by the tests).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            known: HashMap::new(),
        }
    }

    /// Whether we have ever paired with this host.
    pub fn has(&self, host: &HostId) -> bool {
        self.known.contains_key(host)
    }

    /// Whether `identity` is the key we remember for this host.
    pub fn matches(&self, host: &HostId, identity: &[u8; 32]) -> bool {
        self.known.get(host) == Some(identity)
    }

    /// Records (or replaces) the identity for a host, and persists.
    pub fn remember(&mut self, host: HostId, identity: [u8; 32]) {
        self.known.insert(host, identity);
        self.write();
    }

    fn read(&mut self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };

        for line in contents.lines() {
            let mut parts = line.split_whitespace();
            let (Some(host), Some(key)) = (parts.next(), parts.next()) else {
                continue;
            };
            match key_from_hex(key) {
                Some(key) => {
                    self.known.insert(HostId::from(host), key);
                }
                None => warn!("ignoring malformed known-hosts entry for {host}"),
            }
        }
        debug!("remembered {} host identity(ies)", self.known.len());
    }

    fn write(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(dir) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(dir) {
                warn!("could not create {}: {err}", dir.display());
                return;
            }
        }

        let mut contents = String::new();
        for (host, key) in &self.known {
            contents.push_str(host.as_str());
            contents.push(' ');
            contents.push_str(&key_to_hex(key));
            contents.push('\n');
        }
        if let Err(err) = std::fs::write(path, contents) {
            warn!("could not persist known hosts: {err}");
        }
    }
}

/// Where this platform keeps the client's known hosts.
pub fn default_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(FILE_NAME))
}

#[cfg(target_os = "windows")]
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("id.fahrul.vd"))
}

#[cfg(not(target_os = "windows"))]
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|base| base.join("id.fahrul.vd"))
}

/// Whether `path` looks like a store this module owns (used by tests).
#[allow(dead_code)]
fn is_store_path(path: &Path) -> bool {
    path.file_name()
        .map(|name| name == FILE_NAME)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostId {
        HostId::from("host-1")
    }

    #[test]
    fn an_unknown_host_is_not_trusted() {
        let store = TrustStore::in_memory();
        assert!(!store.has(&host()));
        assert!(!store.matches(&host(), &[1u8; 32]));
    }

    #[test]
    fn a_remembered_host_is_trusted_for_that_key_only() {
        let mut store = TrustStore::in_memory();
        store.remember(host(), [1u8; 32]);

        assert!(store.has(&host()));
        assert!(store.matches(&host(), &[1u8; 32]));
        // A different key means a different host, or an impostor.
        assert!(!store.matches(&host(), &[2u8; 32]));
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join("vd-trust-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(FILE_NAME);

        let mut store = TrustStore::load(Some(path.clone()));
        store.remember(host(), [3u8; 32]);

        let reloaded = TrustStore::load(Some(path));
        assert!(reloaded.matches(&host(), &[3u8; 32]));
        assert!(!reloaded.matches(&host(), &[4u8; 32]));
    }
}
