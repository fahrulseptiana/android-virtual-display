//! Android Virtual Display — client.
//!
//! This crate is built twice:
//!
//! * as a **cdylib** for Android, exposing [`android_main`] which the
//!   `NativeActivity` glue calls (see the `android/` Gradle project), and
//! * as a plain **desktop binary** ([`run_desktop`]) so the UI and discovery
//!   logic can be iterated on without a device.

pub mod app;
pub mod connection;
pub mod frames;
pub mod logging;
#[cfg(target_os = "android")]
pub mod media_codec;

pub const APP_NAME: &str = "VD Client";

/// Runs the client as a desktop application.
///
/// Only available off Android; it exists purely as a development harness.
#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> eframe::Result<()> {
    logging::init();
    eframe::run_native(
        APP_NAME,
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app::VdClientApp::new()))),
    )
}

/// Entry point invoked by the Android `NativeActivity` glue.
///
/// `android-activity` requires an exported, unmangled **Rust** ABI function
/// with the signature `fn android_main(app: AndroidApp)`.
#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    use std::sync::OnceLock;

    // `android_main` runs again when the activity is recreated; the logger must
    // only be initialised once per process.
    static INIT_LOGGING: OnceLock<()> = OnceLock::new();
    INIT_LOGGING.get_or_init(logging::init);

    let options = eframe::NativeOptions {
        android_app: Some(android_app),
        ..Default::default()
    };

    if let Err(err) = eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_cc| Ok(Box::new(app::VdClientApp::new()))),
    ) {
        log::error!("eframe exited with an error: {err}");
    }
}
