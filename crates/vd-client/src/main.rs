//! Desktop binary used to exercise the client UI and discovery logic locally.

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    vd_client::run_desktop()
}

#[cfg(target_os = "android")]
fn main() {}
