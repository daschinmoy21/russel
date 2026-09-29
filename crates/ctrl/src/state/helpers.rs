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
///
/// Returns None if parsing fails, the timestamp is in the future, or the age
/// predates the monotonic clock (adoption then uses `Instant::now`).
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
    // `Instant` is monotonic since boot. `Instant::now() - duration` panics
    // when `duration` exceeds that (suspend, clock step, or `deployed_at`
    // from before boot). `checked_sub` returns `None` so adoption can fall
    // back to `Instant::now`.
    Instant::now().checked_sub(Duration::from_secs(elapsed_secs))
}

/// Result of `podman inspect` for `.State.Running`.
///
/// [`ContainerProbe::Unknown`] means the probe did not run or podman failed
/// unexpectedly; startup reconcile must not treat that as stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerProbe {
    Running,
    NotRunning,
    Unknown,
}

/// Classify `podman inspect` stdout/stderr for `.State.Running` without spawning.
///
/// Spawn/IO errors are [`ContainerProbe::Unknown`] and are not passed here.
/// Podman exit 125 means the binary could not be invoked (daemon down, connect
/// failure). That is unknown, not missing. A missing container is recognized
/// only from stderr (`no such object` / `no such container`).
pub(crate) fn classify_podman_inspect(
    status_success: bool,
    _code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> ContainerProbe {
    if status_success {
        return if stdout.trim() == "true" {
            ContainerProbe::Running
        } else {
            ContainerProbe::NotRunning
        };
    }
    let stderr_l = stderr.to_ascii_lowercase();
    if stderr_l.contains("no such object") || stderr_l.contains("no such container") {
        ContainerProbe::NotRunning
    } else {
        ContainerProbe::Unknown
    }
}

/// Bound for `podman inspect` so a hung daemon cannot stall reconcile.
const PODMAN_INSPECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe whether a container is running.
///
/// A spawn/IO error, timeout, or unexpected inspect failure is
/// [`ContainerProbe::Unknown`], not stopped. A missing container (stderr
/// "no such object" / "no such container") is [`ContainerProbe::NotRunning`].
pub(crate) async fn probe_container(container_id: &str) -> ContainerProbe {
    let mut cmd = crate::container::podman_command().await;
    cmd.args(["inspect", container_id, "--format", "{{.State.Running}}"]);
    let output = match tokio::time::timeout(PODMAN_INSPECT_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) | Err(_) => return ContainerProbe::Unknown,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_podman_inspect(
        output.status.success(),
        output.status.code(),
        &stdout,
        &stderr,
    )
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::parse_rfc3339_to_instant;

    fn rfc3339_secs_from_now(delta_secs: i64) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_secs() as i64;
        let secs = now.checked_add(delta_secs).expect("timestamp overflow");
        assert!(secs >= 0, "timestamp before unix epoch");
        russel_core::timeutil::rfc3339_from_unix(secs as u64)
    }

    #[test]
    fn far_past_timestamp_does_not_panic() {
        // ~10 years ago is longer than monotonic time since boot on any
        // realistic host. `None` is expected; `Some` is also fine if uptime
        // exceeds that age. The bug was a panic.
        let ts = rfc3339_secs_from_now(-(10 * 365 * 24 * 60 * 60));
        if let Some(started) = parse_rfc3339_to_instant(&ts) {
            assert!(started <= Instant::now());
        }
    }

    #[test]
    fn recent_timestamp_returns_some() {
        let ts = rfc3339_secs_from_now(-5);
        assert!(parse_rfc3339_to_instant(&ts).is_some());
    }

    #[test]
    fn future_timestamp_returns_none() {
        let ts = rfc3339_secs_from_now(3_600);
        assert!(parse_rfc3339_to_instant(&ts).is_none());
    }

    #[test]
    fn classify_podman_inspect_states() {
        use super::{ContainerProbe, classify_podman_inspect};

        assert_eq!(
            classify_podman_inspect(true, Some(0), "true\n", ""),
            ContainerProbe::Running
        );
        assert_eq!(
            classify_podman_inspect(true, Some(0), "false\n", ""),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(true, Some(0), "  \n", ""),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, Some(125), "", "Error: cannot connect"),
            ContainerProbe::Unknown
        );
        assert_eq!(
            classify_podman_inspect(false, Some(125), "", ""),
            ContainerProbe::Unknown
        );
        assert_eq!(
            classify_podman_inspect(false, Some(125), "", "Error: no such object: abc"),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, Some(1), "", "Error: no such object: abc"),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, Some(1), "", "No such container: abc"),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, Some(125), "", "No such container: abc"),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, None, "", "No such container: abc"),
            ContainerProbe::NotRunning
        );
        assert_eq!(
            classify_podman_inspect(false, Some(1), "", "cannot connect to Podman socket"),
            ContainerProbe::Unknown
        );
        assert_eq!(
            classify_podman_inspect(false, None, "", "killed"),
            ContainerProbe::Unknown
        );
    }
}
