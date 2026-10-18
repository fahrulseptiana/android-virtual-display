//! What a host can capture.
//!
//! A host usually has more than one display output: the machine's own screens,
//! and any virtual monitor an indirect display driver has added. Which one the
//! client wants is the client's decision, so the description travels over the
//! control channel rather than being configured on the host.
//!
//! Deliberately platform-neutral. The host has richer notions of an output --
//! adapters, DXGI handles, whether duplication is available -- but none of that
//! means anything at the other end.

use serde::{Deserialize, Serialize};

/// What kind of screen an output is.
///
/// The client's two choices are mirror and extend, and this is what tells them
/// apart: mirroring means the screen the host is already using, extending means a
/// monitor a driver added. `Unknown` exists because telling them apart is a
/// heuristic, and a wrong guess in either direction is worse than admitting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayKind {
    /// A screen with something physically behind it.
    Physical,
    /// A monitor created by an indirect display driver.
    Virtual,
    /// Could not be told which.
    Unknown,
}

/// One display output a host could capture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// What the client sends back to select this output.
    pub index: u32,
    /// The adapter driving it, as the host names it.
    pub adapter: String,
    /// The monitor's own name, e.g. `Generic Monitor (VDD by MTT)`. Empty when
    /// Windows only offers its placeholder, which says nothing.
    pub monitor: String,
    /// The hardware id from the monitor's device path, e.g. `MTT1337`.
    ///
    /// Worth having because it is often the only place an indirect display names
    /// itself: the friendly name Windows reports for one is the same
    /// "Generic PnP Monitor" it reports for a television.
    pub hardware_id: String,
    /// The GDI device name, e.g. `\\.\DISPLAY2`.
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    /// Whether the output is part of the desktop. One that is not has nothing to
    /// capture yet, and is listed so the client can say so rather than showing a
    /// failure.
    pub attached: bool,
    /// Whether this is the screen the host is already using, which is what
    /// mirroring means. Taken from where the desktop starts rather than from a
    /// display API, since the primary monitor is the one at the origin.
    pub primary: bool,
    pub kind: DisplayKind,
}

impl DisplayInfo {
    /// A one-line description for a list the user reads.
    pub fn summary(&self) -> String {
        format!("{}x{} ({})", self.width, self.height, self.label())
    }

    /// What to call this display, preferring the monitor's own name.
    pub fn label(&self) -> &str {
        if !self.monitor.is_empty() {
            self.monitor.as_str()
        } else if !self.hardware_id.is_empty() {
            self.hardware_id.as_str()
        } else if !self.device_name.is_empty() {
            self.device_name.as_str()
        } else {
            self.adapter.as_str()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(monitor: &str, device_name: &str, adapter: &str) -> DisplayInfo {
        DisplayInfo {
            index: 0,
            adapter: adapter.to_owned(),
            monitor: monitor.to_owned(),
            hardware_id: String::new(),
            device_name: device_name.to_owned(),
            width: 1920,
            height: 1200,
            attached: true,
            primary: false,
            kind: DisplayKind::Physical,
        }
    }

    #[test]
    fn labels_prefer_the_monitor_name() {
        let output = display(
            "Generic Monitor (VDD by MTT)",
            "\\\\.\\DISPLAY6",
            "AMD Radeon",
        );
        assert_eq!(output.label(), "Generic Monitor (VDD by MTT)");
        assert_eq!(output.summary(), "1920x1200 (Generic Monitor (VDD by MTT))");
    }

    #[test]
    fn labels_fall_back_through_device_then_adapter() {
        assert_eq!(
            display("", "\\\\.\\DISPLAY1", "AMD").label(),
            "\\\\.\\DISPLAY1"
        );
        assert_eq!(display("", "", "AMD").label(), "AMD");
    }

    #[test]
    fn labels_use_the_hardware_id_when_the_name_says_nothing() {
        // Which is what a virtual display looks like: Windows calls it a generic
        // monitor and only the device path says who made it.
        let mut output = display("", "", "AMD");
        output.hardware_id = "MTT1337".to_owned();
        assert_eq!(output.label(), "MTT1337");
    }

    #[test]
    fn round_trips_through_json() {
        let output = display("Generic Monitor", "\\\\.\\DISPLAY1", "AMD Radeon");
        let json = serde_json::to_string(&output).unwrap();
        assert_eq!(serde_json::from_str::<DisplayInfo>(&json).unwrap(), output);
    }

    #[test]
    fn kinds_are_snake_case_on_the_wire() {
        let json = serde_json::to_string(&DisplayKind::Virtual).unwrap();
        assert_eq!(json, "\"virtual\"");
    }
}
