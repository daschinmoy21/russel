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
    // Format: YYYY-MM-DDTHH:MM:SSZ
    let bytes = rfc3339.as_bytes();
    if bytes.len() < 20 || bytes[19] != b'Z' {
        return None;
    }

    let year: i64 = rfc3339[0..4].parse().ok()?;
    let month: u32 = rfc3339[5..7].parse().ok()?;
    let day: u32 = rfc3339[8..10].parse().ok()?;
    let hour: u32 = rfc3339[11..13].parse().ok()?;
    let minute: u32 = rfc3339[14..16].parse().ok()?;
    let second: u32 = rfc3339[17..19].parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let unix_secs = days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;

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

pub(super) fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    Some(days)
}

/// Check if a container is still running via `podman inspect`.
pub(super) async fn check_container_running(container_id: &str) -> bool {
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
