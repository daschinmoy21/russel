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

use std::path::{Path, PathBuf};

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

/// Longest Unix socket path bind(2) accepts: `sun_path` is 108 bytes on
/// Linux, one of them the terminating NUL.
pub const MAX_UNIX_SOCKET_PATH: usize = 107;

/// Fail when `socket` is too long to bind. passt, virtiofsd and
/// cloud-hypervisor put their sockets under the service dir, and with a long
/// `RUSSEL_DATA_DIR` they just exit with a status (passt's looked like a busy
/// host port). Check before spawning them so the real cause is reported.
pub fn check_unix_socket_path(socket: &Path) -> anyhow::Result<()> {
    let len = socket.as_os_str().len();
    if len > MAX_UNIX_SOCKET_PATH {
        anyhow::bail!(
            "Unix socket path {} is {len} bytes, over the {MAX_UNIX_SOCKET_PATH}-byte \
             limit for Unix sockets; set RUSSEL_DATA_DIR to a shorter directory \
             (currently {})",
            socket.display(),
            data_root().display()
        );
    }
    Ok(())
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

    #[test]
    #[allow(clippy::unwrap_used)]
    fn socket_path_length_limit() {
        let at_limit = format!("/{}", "a".repeat(MAX_UNIX_SOCKET_PATH - 1));
        assert!(check_unix_socket_path(Path::new(&at_limit)).is_ok());
        let over = format!("{at_limit}a");
        assert!(check_unix_socket_path(Path::new(&over)).is_err());

        // The 2026-09-28 repro: a scratchpad RUSSEL_DATA_DIR.
        let long = Path::new(
            "/tmp/claude-1000/-home-crimxnhaze-russel-dev/\
             a0d2b44f-6dac-4e79-a2a4-fafbc4b86d31/scratchpad/rb/data/api/passt.sock",
        );
        let len = long.as_os_str().len();
        let err = check_unix_socket_path(long).unwrap_err().to_string();
        assert!(err.contains(&long.display().to_string()), "{err}");
        assert!(err.contains(&format!("is {len} bytes")), "{err}");
        assert!(err.contains("107-byte limit"), "{err}");
        assert!(err.contains("RUSSEL_DATA_DIR"), "{err}");
    }
}
