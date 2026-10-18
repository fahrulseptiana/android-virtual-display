//! What this machine can be asked to show.
//!
//! Wraps the DXGI enumeration in the description a client is sent, adding the two
//! things DXGI does not report and the client's choice depends on: what the
//! monitor calls itself, and whether it is one a driver made up. The second is
//! what turns "here is a list of outputs" into "mirror, or extend".

use log::warn;
use vd_core::{DisplayInfo, DisplayKind};

/// Every display output, described for a client.
///
/// An empty list is not an error: a host with no capturable output can still be
/// connected to, and the client should be able to say what it is looking at.
pub fn available() -> Vec<DisplayInfo> {
    match crate::dxgi::enumerate_outputs() {
        Ok(outputs) => outputs
            .iter()
            .enumerate()
            .map(|(index, output)| DisplayInfo {
                index: index as u32,
                adapter: output.adapter_name.clone(),
                // Windows' placeholder says nothing and hides the hardware id
                // that does, so it is dropped rather than shown.
                monitor: if output.monitor.starts_with("Generic PnP") {
                    String::new()
                } else {
                    output.monitor.clone()
                },
                hardware_id: hardware_id(&output.monitor_id),
                device_name: output.device_name.clone(),
                width: output.width,
                height: output.height,
                attached: output.attached,
                primary: output.primary,
                kind: classify(&output.monitor, &output.monitor_id),
            })
            .collect(),
        Err(err) => {
            warn!("could not list displays: {err}");
            Vec::new()
        }
    }
}

/// Whether a monitor looks like one a driver made up.
///
/// There is no field on a DXGI output that says so, and the adapter does not give
/// it away either -- a virtual monitor renders on the ordinary GPU, so it appears
/// as an extra output of the real adapter. What does give it away is the monitor's
/// device path, which carries a hardware id naming its maker. The friendly name
/// is checked too, and is sometimes enough.
///
/// So this is a heuristic, and deliberately a cautious one. Getting it wrong in
/// the virtual direction would offer "extend" for a real screen, so only names
/// that are known are matched; an unrecognised monitor is called physical, which
/// at worst hides an extend option rather than pointing it at the wrong display.
/// `DISPLAYCONFIG`'s output technology would answer it properly.
fn classify(monitor: &str, monitor_id: &str) -> DisplayKind {
    if monitor.is_empty() && monitor_id.is_empty() {
        // Windows had nothing to say, so there is nothing to judge by.
        return DisplayKind::Unknown;
    }

    const VIRTUAL_MARKERS: [&str; 6] = [
        "vdd",
        "virtual display",
        "iddsample",
        "parsec",
        "indirect display",
        // The Virtual Display Driver's own hardware id, which is what appears in
        // the device path when the friendly name says nothing useful.
        "mtt1337",
    ];

    let name = format!("{monitor} {monitor_id}").to_lowercase();
    if VIRTUAL_MARKERS.iter().any(|marker| name.contains(marker)) {
        DisplayKind::Virtual
    } else {
        DisplayKind::Physical
    }
}

/// The hardware id inside a monitor's device interface path.
///
/// The middle segment of the path is the part worth keeping: it says who made the
/// monitor, and for a virtual display it is the only thing that does.
fn hardware_id(monitor_id: &str) -> String {
    monitor_id.split('#').nth(1).unwrap_or("").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hardware_id_comes_out_of_the_device_path() {
        assert_eq!(hardware_id("DISPLAY#MTT1337#5&UID281"), "MTT1337");
        assert_eq!(hardware_id(""), "");
    }

    #[test]
    fn the_device_path_alone_identifies_a_virtual_display() {
        // The case that matters: a VDD monitor reports the same friendly name a
        // television does, and only its device path says what it is.
        assert_eq!(
            classify("", "DISPLAY#MTT1337#5&UID281"),
            DisplayKind::Virtual
        );
    }

    #[test]
    fn the_virtual_display_driver_is_recognised() {
        assert_eq!(
            classify("Generic Monitor (VDD by MTT)", "\\\\.\\DISPLAY6"),
            DisplayKind::Virtual
        );
        assert_eq!(
            classify("Generic Monitor (ParsecVDD)", "\\\\.\\DISPLAY2"),
            DisplayKind::Virtual
        );
    }

    #[test]
    fn ordinary_monitors_are_not() {
        assert_eq!(
            classify("Generic Monitor (MTK LCDTV)", "\\\\.\\DISPLAY1"),
            DisplayKind::Physical
        );
        assert_eq!(
            classify("Mi Monitor", "\\\\.\\DISPLAY3"),
            DisplayKind::Physical
        );
    }

    #[test]
    fn a_monitor_with_nothing_known_is_unknown() {
        // Nothing to judge by, and guessing either way would be a lie.
        assert_eq!(classify("", ""), DisplayKind::Unknown);
    }
}
