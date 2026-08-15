//! Shared helpers for the control-plane service state module.

use std::time::{Duration, Instant};

/// Maximum size of in-memory logs per service (64 KiB).
pub(super) const MAX_LOG_BYTES: usize = 64 * 1024;

/// Append to a log buffer, truncating from the front at a line boundary
/// when the buffer exceeds MAX_LOG_BYTES (issue #136).
pub(super) fn push_capped(buf: &mut String, s: &str) {
    buf.push_str(s);
    if buf.len() > MAX_LOG_BYTES {
        let excess = buf.len() - MAX_LOG_BYTES;
        if let Some(pos) = buf[excess..].find('\n') {
            let cut = excess + pos + 1;
            buf.drain(..cut);
        } else {
            buf.clear();
        }
    }
}

/// Parse RFC3339 timestamp (second precision UTC) to Instant for uptime back-dating.
/// Returns None if parsing fails or the timestamp is in the future.
pub(super) fn parse_rfc3339_to_instant(rfc3339: &str) -> Option<Instant> {
    let unix_secs = russel_core::timeutil::rfc3339_to_unix_secs(rfc3339)?;

    let now_sys = std::time::SystemTime::now();
    let now_secs = now_sys
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;

    if unix_secs > now_secs {
        return None;
    }

    let elapsed_secs = (now_secs - unix_secs) as u64;
    Some(Instant::now() - Duration::from_secs(elapsed_secs))
}

/// Check if a container is still running via `podman inspect`.
pub(crate) async fn check_container_running(container_id: &str) -> bool {
    let output = match crate::container::podman_command()
        .await
        .args(["inspect", container_id, "--format", "{{.State.Running}}"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim() == "true"
}

/// Check if a PID is still alive via /proc/{pid}/stat.
pub(super) fn pid_is_alive(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok()
}

/// Read the last `max_bytes` of a file, seeking from the end to avoid loading
/// huge log files fully into memory. Returns None on I/O error.
pub(super) fn read_tail_of_file(
    path: impl AsRef<std::path::Path>,
    max_bytes: u64,
) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    // If we didn't start at the beginning, skip to the next line boundary
    // so we don't return a partial first line.
    if start > 0
        && let Some(pos) = buf.find('\n')
    {
        buf = buf[pos + 1..].to_string();
    }
    Some(buf)
}
