//! Control-plane logging: file always, stderr only for warnings unless `--debug`.

use std::fs::OpenOptions;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Log file name under the data root (`RUSSEL_DATA_DIR`, default `/var/lib/russel`).
pub const LOG_FILE_NAME: &str = "ctrl.log";

/// Filter written to the log file (same verbosity ctrl used to dump on stdout).
pub const FILE_FILTER: &str = "info,russel_ctrl=debug";

/// Stderr without `--debug`: warnings and errors only.
pub const QUIET_STDERR_FILTER: &str = "warn";

/// Where to write `ctrl.log` when the data root is not writable.
pub fn fallback_log_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state.join("russel/ctrl.log")
}

/// CLI `--log-file` wins; otherwise `<data root>/ctrl.log` when that dir
/// exists and is writable, else `$XDG_STATE_HOME/russel/ctrl.log`.
pub fn resolve_log_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    let preferred = crate::paths::data_root().join(LOG_FILE_NAME);
    if preferred
        .parent()
        .is_some_and(|parent| parent.is_dir() && dir_is_writable(parent))
    {
        preferred
    } else {
        fallback_log_path()
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    // Unix: check the directory's write bit for the current user. Avoids
    // creating a probe file in `/var/lib/russel` during resolve.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(dir) else {
            return false;
        };
        let mode = meta.mode();
        let uid = meta.uid();
        let gid = meta.gid();
        // Safety: geteuid/getegid are pure POSIX queries of this process.
        let euid = unsafe { libc::geteuid() };
        let egid = unsafe { libc::getegid() };
        if euid == uid {
            return mode & 0o200 != 0;
        }
        if egid == gid {
            return mode & 0o020 != 0;
        }
        mode & 0o002 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

/// Create parent dirs, open for append, mode 0600.
///
/// On Unix the create mode is set on the open so a new file is never briefly
/// world-readable. `set_permissions` then tightens a pre-existing file.
pub fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn env_or(default: &'static str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
}

/// Install tracing: file (always) + stderr (`warn`, or the file filter with `--debug`).
///
/// Keep the returned guard alive for the process lifetime or buffered lines drop.
pub fn init(debug: bool, log_path: &Path) -> Result<(PathBuf, WorkerGuard)> {
    match open_log_file(log_path) {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            install_subscriber(debug, writer);
            Ok((log_path.to_path_buf(), guard))
        }
        Err(e) => {
            let fallback = fallback_log_path();
            if fallback == log_path {
                anyhow::bail!(
                    "failed to open log file {}: {e}; fix permissions or pass --log-file",
                    log_path.display()
                );
            }
            let file = open_log_file(&fallback).with_context(|| {
                format!(
                    "failed to open log file {} (and fallback {})",
                    log_path.display(),
                    fallback.display()
                )
            })?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            install_subscriber(debug, writer);
            eprintln!(
                "warning: could not write {}: {e}; logging to {}",
                log_path.display(),
                fallback.display()
            );
            Ok((fallback, guard))
        }
    }
}

fn install_subscriber(debug: bool, file_writer: NonBlocking) {
    let file_layer = tracing_subscriber::fmt::layer()
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_target(false)
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(env_or(FILE_FILTER));

    // Quiet stderr ignores RUST_LOG. `--debug` streams the file verbosity
    // (or RUST_LOG, when set) to the terminal.
    let stderr_filter = if debug {
        env_or(FILE_FILTER)
    } else {
        EnvFilter::new(QUIET_STDERR_FILTER)
    };
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_target(false)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .with_filter(stderr_filter);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn explicit_log_path_wins() {
        let p = PathBuf::from("/tmp/russel-test-ctrl.log");
        assert_eq!(resolve_log_path(Some(p.clone())), p);
    }

    #[test]
    fn fallback_uses_xdg_or_home() {
        let path = fallback_log_path();
        assert!(path.ends_with("russel/ctrl.log"), "{}", path.display());
    }

    #[test]
    fn open_log_file_creates_parent_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/ctrl.log");
        {
            let mut f = open_log_file(&path).unwrap();
            use std::io::Write;
            f.write_all(b"a\n").unwrap();
        }
        {
            let mut f = open_log_file(&path).unwrap();
            use std::io::Write;
            f.write_all(b"b\n").unwrap();
        }
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "a\nb\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode={mode:o}");
        }
    }

    #[test]
    fn open_log_file_tightens_existing_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrl.log");
        std::fs::write(&path, b"old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        open_log_file(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode={mode:o}");
        }
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "old\n");
    }
}
