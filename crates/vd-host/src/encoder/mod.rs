//! Hardware video encoding.
//!
//! Platform dispatch lives here; today only Windows/Media Foundation is
//! implemented.

#[cfg(windows)]
pub mod media_foundation;
