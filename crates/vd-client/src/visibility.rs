//! Whether the UI is actually drawing.
//!
//! Android offers no dependable "you are in the background" signal here: winit
//! never reports occlusion on that platform, and eframe only calls
//! [`App::ui`](eframe::App::ui) while there is a surface to draw on. What *is*
//! dependable is that the calls stop.
//!
//! So the UI says when it last drew, and going quiet for long enough is taken as
//! being invisible. That is self-correcting in both directions — a resume starts
//! the calls again — and it needs no platform lifecycle plumbing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the UI may go without drawing before it counts as invisible.
///
/// Repaints were measured at roughly 50 per second, so a stall of two seconds
/// means the surface is gone rather than merely busy.
const INVISIBLE_AFTER: Duration = Duration::from_secs(2);

/// Tracks when the UI last drew.
#[derive(Clone)]
pub struct Visibility {
    last: Arc<Mutex<Instant>>,
}

impl Visibility {
    /// Starts off considered visible, since the UI is about to draw.
    pub fn new() -> Self {
        Self {
            last: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Records that the UI has just drawn. Called on every repaint.
    pub fn tick(&self) {
        *self.lock() = Instant::now();
    }

    /// Whether the UI has drawn recently enough to be on screen.
    pub fn visible(&self) -> bool {
        self.visible_within(INVISIBLE_AFTER)
    }

    /// Whether the UI drew within the given window. The window is a parameter
    /// so tests need not wait two seconds to prove anything.
    pub fn visible_within(&self, window: Duration) -> bool {
        self.lock().elapsed() < window
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Instant> {
        // A panic elsewhere is no reason to stop reporting visibility.
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for Visibility {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn starts_visible() {
        let visibility = Visibility::new();
        assert!(visibility.visible_within(Duration::from_millis(50)));
    }

    #[test]
    fn going_quiet_means_invisible() {
        let visibility = Visibility::new();
        thread::sleep(Duration::from_millis(30));
        // Nothing drew in the last 10 ms, so as far as the client knows the
        // surface is gone.
        assert!(!visibility.visible_within(Duration::from_millis(10)));
    }

    #[test]
    fn drawing_again_makes_it_visible() {
        let visibility = Visibility::new();
        thread::sleep(Duration::from_millis(30));
        assert!(!visibility.visible_within(Duration::from_millis(10)));

        visibility.tick();
        assert!(visibility.visible_within(Duration::from_millis(10)));
    }

    #[test]
    fn clones_share_the_state() {
        let visibility = Visibility::new();
        let other = visibility.clone();
        other.tick();
        assert!(visibility.visible_within(Duration::from_millis(50)));
    }
}
