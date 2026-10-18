//! Logging bootstrap for both the Android app and the desktop harness.

/// Installs the platform-appropriate logger.
///
/// On Android logs go to `logcat` (tag `vd-client`); everywhere else they go to
/// stderr and honour `RUST_LOG`.
pub fn init() {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("vd-client"),
        );
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init();
    }
}
