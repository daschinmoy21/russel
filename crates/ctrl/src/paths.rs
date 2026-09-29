//! Host state roots as ctrl sees them.
//!
//! Thin wrappers over [`russel_core::paths`]. In this crate's unit tests the
//! first call pins both roots to a per-process temp dir, so `cargo test` never
//! reads or writes the real `/var/lib/russel` or `/var/lib/microvms` (#412).
//! Integration tests call [`russel_core::paths::pin_temp_roots`] themselves.
//!
//! Use these instead of the core functions; `crates/ctrl/clippy.toml` bans
//! the direct calls everywhere else in this crate.

#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

/// State directory for services, the catalog, and managed volumes.
pub fn data_root() -> PathBuf {
    isolate_in_tests();
    russel_core::paths::data_root()
}

/// `<data_root>/<service_id>`.
pub fn service_dir(service_id: &str) -> PathBuf {
    data_root().join(service_id)
}

/// Parent of the per-service microVM marker dirs.
pub fn microvms_root() -> PathBuf {
    isolate_in_tests();
    russel_core::paths::microvms_root()
}

/// `<microvms_root>/<service_id>`.
pub fn microvm_dir(service_id: &str) -> PathBuf {
    microvms_root().join(service_id)
}

#[cfg(test)]
fn isolate_in_tests() {
    russel_core::paths::pin_temp_roots();
}

#[cfg(not(test))]
#[inline]
fn isolate_in_tests() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_tests_never_see_host_roots() {
        let base = russel_core::paths::pin_temp_roots();
        assert!(data_root().starts_with(base));
        assert!(microvms_root().starts_with(base));
        assert_ne!(
            data_root(),
            PathBuf::from(russel_core::paths::DEFAULT_DATA_ROOT)
        );
        assert_ne!(
            microvms_root(),
            PathBuf::from(russel_core::paths::DEFAULT_MICROVMS_ROOT)
        );
        assert_eq!(service_dir("api"), data_root().join("api"));
        assert_eq!(microvm_dir("api"), microvms_root().join("api"));
    }
}
