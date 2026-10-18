//! Keeps a single host instance alive per machine.
//!
//! A named mutex is the usual Windows answer: the first process to create it
//! owns it, and any later process sees `ERROR_ALREADY_EXISTS`. The handle is
//! held for the lifetime of the program, so the name stays taken.

use log::debug;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// Holds the single-instance mutex.
///
/// Dropping it releases the name, so it must outlive the run loop.
pub struct SingleInstance {
    _handle: HANDLE,
}

impl SingleInstance {
    /// Takes `name`, or reports `None` when another process already holds it.
    pub fn acquire(name: &str) -> Result<Option<Self>, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: `wide` is a NUL-terminated string that outlives the call.
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(wide.as_ptr())) }
            .map_err(|err| format!("CreateMutexW: {err}"))?;

        // CreateMutexW reports "already exists" through the thread's last error,
        // not through its return value, so it has to be read immediately.
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_running {
            debug!("another instance already holds {name}");
            return Ok(None);
        }

        Ok(Some(Self { _handle: handle }))
    }
}
