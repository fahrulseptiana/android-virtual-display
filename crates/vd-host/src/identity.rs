//! Stable identity for this host.
//!
//! The host id is persisted under the per-user application data directory so
//! that clients recognise the same machine across restarts. If the directory
//! cannot be written we fall back to an ephemeral id rather than failing: a
//! flaky disk must not stop a host from being discoverable.

use std::fs;
use std::path::PathBuf;

use log::{debug, warn};
use uuid::Uuid;
use vd_core::HostId;

const APP_DIR: &str = "id.fahrul.vd";
const ID_FILE: &str = "host-id";

/// Loads the persisted host id, creating one on first run.
pub fn load_or_create_host_id() -> HostId {
    let Some(path) = id_file_path() else {
        warn!("no writable data directory; using an ephemeral host id");
        return HostId::new(Uuid::new_v4().to_string());
    };

    if let Ok(contents) = fs::read_to_string(&path) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            debug!("loaded host id from {}", path.display());
            return HostId::new(trimmed);
        }
    }

    let id = Uuid::new_v4().to_string();
    if let Some(dir) = path.parent() {
        if let Err(err) = fs::create_dir_all(dir) {
            warn!("could not create {}: {err}", dir.display());
            return HostId::new(id);
        }
    }
    match fs::write(&path, &id) {
        Ok(()) => debug!("persisted host id to {}", path.display()),
        Err(err) => warn!("could not persist host id: {err}"),
    }
    HostId::new(id)
}

/// Best-effort human readable name for this machine.
///
/// `VD_HOST_NAME` overrides the detected value.
pub fn host_name() -> String {
    if let Ok(name) = std::env::var("VD_HOST_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "virtual-display-host".to_owned())
}

fn id_file_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(ID_FILE))
}

fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(APP_DIR))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|base| base.join(APP_DIR))
    }
}
