//! Container readiness: the app answered, and the container did not exit on
//! the way (#462). The port forwarder alone proves neither.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use super::podman_command;

/// Upper bound on waiting for a new container's app to accept connections.
pub const CONTAINER_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Lines of container output quoted in a failed deploy.
pub const LOG_TAIL_LINES: usize = 40;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What `podman inspect` says about a container's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    /// Podman `State.Status`: created, running, exited, stopped, …
    pub status: String,
    pub exit_code: i32,
    /// Podman `RestartCount`; a restart policy hides a crash behind `running`.
    pub restarts: u32,
}

impl ContainerState {
    /// Parse `{{.State.Status}} {{.State.ExitCode}} {{.RestartCount}}`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        Some(Self {
            status: parts.next()?.to_string(),
            exit_code: parts.next()?.parse().ok()?,
            restarts: parts.next()?.parse().ok()?,
        })
    }

    /// The process exited (or crashed and was restarted) since it started.
    pub fn has_died(&self) -> bool {
        self.restarts > 0 || self.has_exited()
    }

    /// The process is down now (and no restart policy has relaunched it yet).
    pub fn has_exited(&self) -> bool {
        matches!(self.status.as_str(), "exited" | "stopped" | "dead")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyOutcome {
    Ready,
    /// The container exited or restarted before the app answered.
    Died(ContainerState),
    /// The container never died, but nothing accepted connections in time.
    TimedOut,
}

/// Poll until `probe` reports the app answering, `state` reports the
/// container dead, or `timeout` passes. `state` returning `None` (inspect
/// failed or the container is gone) counts as unknown, not dead; a removed
/// container shows up as a probe that never passes.
pub async fn wait_until_ready<P, PF, S, SF>(
    mut probe: P,
    mut state: S,
    timeout: Duration,
) -> ReadyOutcome
where
    P: FnMut() -> PF,
    PF: Future<Output = bool>,
    S: FnMut() -> SF,
    SF: Future<Output = Option<ContainerState>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        // Inspect while the probe holds its connection open, so a ready
        // container costs one probe window, not window + inspect. A container
        // that dies during the window also closes the connection, failing the
        // probe. Death wins over an answer: a crash loop under a restart
        // policy can be briefly up between restarts.
        let (state, answered) = tokio::join!(state(), probe());
        if let Some(s) = state.filter(ContainerState::has_died) {
            return ReadyOutcome::Died(s);
        }
        if answered {
            return ReadyOutcome::Ready;
        }
        if Instant::now() >= deadline {
            return ReadyOutcome::TimedOut;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Watch a container that already answered for `window` (#493): its state
/// when it exited or restarted meanwhile, `None` when it stayed up. An app
/// can listen and then crash, which the readiness probe alone misses.
pub async fn watch_container(container_name: &str, window: Duration) -> Option<ContainerState> {
    watch_state(|| inspect_state(container_name), window).await
}

async fn watch_state<S, SF>(mut state: S, window: Duration) -> Option<ContainerState>
where
    S: FnMut() -> SF,
    SF: Future<Output = Option<ContainerState>>,
{
    let deadline = Instant::now() + window;
    loop {
        if let Some(s) = state().await.filter(ContainerState::has_died) {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Bound on one `podman inspect`, so a hung Podman cannot stall a poll loop.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(2);

/// One `podman inspect` of a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    State(ContainerState),
    /// Podman has no such container: it was removed outside Russel.
    Gone,
    /// Inspect failed, timed out, or printed something unexpected.
    Unknown,
}

/// `podman inspect` a container, telling a removed container apart from an
/// inspect that could not run.
pub async fn observe(container_name: &str) -> Observed {
    let mut cmd = podman_command().await;
    cmd.kill_on_drop(true).args([
        "inspect",
        container_name,
        "--format",
        "{{.State.Status}} {{.State.ExitCode}} {{.RestartCount}}",
    ]);
    let Ok(Ok(output)) = tokio::time::timeout(INSPECT_TIMEOUT, cmd.output()).await else {
        return Observed::Unknown;
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        return if stderr.contains("no such object") || stderr.contains("no such container") {
            Observed::Gone
        } else {
            Observed::Unknown
        };
    }
    ContainerState::parse(&String::from_utf8_lossy(&output.stdout))
        .map_or(Observed::Unknown, Observed::State)
}

/// `podman inspect` state for a container, or `None` when it cannot be read.
pub async fn inspect_state(container_name: &str) -> Option<ContainerState> {
    match observe(container_name).await {
        Observed::State(s) => Some(s),
        Observed::Gone | Observed::Unknown => None,
    }
}

/// Last `lines` lines of a Podman `k8s-file` log, message text only.
pub fn log_tail(path: &Path, lines: usize) -> String {
    const MAX_BYTES: u64 = 64 * 1024;
    let Ok(bytes) = read_last_bytes(path, MAX_BYTES) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let messages: Vec<&str> = text.lines().filter_map(k8s_file_message).collect();
    messages[messages.len().saturating_sub(lines)..].join("\n")
}

/// Message of one `k8s-file` line: `<RFC3339> <stream> <P|F> <message>`.
fn k8s_file_message(line: &str) -> Option<&str> {
    let mut parts = line.splitn(4, ' ');
    let (_ts, stream, tag) = (parts.next()?, parts.next()?, parts.next()?);
    if !matches!(stream, "stdout" | "stderr") || !matches!(tag, "P" | "F") {
        return None;
    }
    Some(parts.next().unwrap_or(""))
}

fn read_last_bytes(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Deploy error for a container that did not become ready.
pub fn not_ready_error(
    outcome: &ReadyOutcome,
    host_port: u16,
    guest_port: u16,
    timeout: Duration,
    log_tail: &str,
) -> String {
    let head = match outcome {
        ReadyOutcome::Ready => return String::new(),
        ReadyOutcome::Died(s) if s.restarts > 0 => format!(
            "container crashed during startup (restarted {} time(s), last exit code {})",
            s.restarts, s.exit_code
        ),
        ReadyOutcome::Died(s) => format!(
            "container exited during startup ({}, exit code {})",
            s.status, s.exit_code
        ),
        ReadyOutcome::TimedOut => format!(
            "app did not accept connections on port {guest_port} within {}s \
             (the container is running, but nothing answered through host port {host_port}). \
             Make sure it listens on 0.0.0.0:{guest_port}, not 127.0.0.1",
            timeout.as_secs()
        ),
    };
    if log_tail.trim().is_empty() {
        format!("{head}; the container printed no output")
    } else {
        format!("{head}. Last container output:\n{log_tail}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn running() -> ContainerState {
        ContainerState::parse("running 0 0").unwrap()
    }

    #[test]
    fn parses_inspect_state() {
        let s = ContainerState::parse("exited 3 0\n").unwrap();
        assert_eq!(
            (s.status.as_str(), s.exit_code, s.restarts),
            ("exited", 3, 0)
        );
        assert!(s.has_died());
        assert!(!running().has_died());
        // A restart policy relaunches a crashed app; the count gives it away.
        assert!(ContainerState::parse("running 4 2").unwrap().has_died());
        assert!(ContainerState::parse("stopped 4 10").unwrap().has_died());
        assert!(!ContainerState::parse("created 0 0").unwrap().has_died());
        assert!(ContainerState::parse("running").is_none());
        assert!(ContainerState::parse("running x 0").is_none());
    }

    #[tokio::test]
    async fn ready_when_app_answers_and_container_lives() {
        let out = wait_until_ready(
            || async { true },
            || async { Some(running()) },
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(out, ReadyOutcome::Ready);
    }

    #[tokio::test]
    async fn watch_catches_an_exit_after_the_app_answered() {
        // #493: up for the probe, gone a moment later.
        let mut polls = 0;
        let died = watch_state(
            || {
                polls += 1;
                let line = if polls < 3 {
                    "running 0 0"
                } else {
                    "exited 1 0"
                };
                async move { ContainerState::parse(line) }
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(died.map(|s| s.exit_code), Some(1));
        let stayed = watch_state(|| async { Some(running()) }, Duration::from_millis(250)).await;
        assert!(stayed.is_none());
    }

    #[tokio::test]
    async fn container_that_exits_fails_at_once() {
        let started = Instant::now();
        let out = wait_until_ready(
            || async { false },
            || async { ContainerState::parse("exited 1 0") },
            Duration::from_secs(10),
        )
        .await;
        assert!(
            matches!(out, ReadyOutcome::Died(ref s) if s.exit_code == 1),
            "{out:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn forwarder_without_app_times_out() {
        // The forwarder accepts but the app never does: probe stays false.
        let out = wait_until_ready(
            || async { false },
            || async { Some(running()) },
            Duration::from_millis(250),
        )
        .await;
        assert_eq!(out, ReadyOutcome::TimedOut);
    }

    #[tokio::test]
    async fn death_seen_during_the_probe_wins() {
        // A crash loop can answer between restarts; the restart count wins.
        let out = wait_until_ready(
            || async { true },
            || async { ContainerState::parse("running 139 2") },
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(out, ReadyOutcome::Died(ref s) if s.restarts == 2),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn inspect_overlaps_the_probe_window() {
        // Ready costs max(probe, inspect), not their sum.
        let slow = Duration::from_millis(300);
        let started = Instant::now();
        let out = wait_until_ready(
            || async move {
                tokio::time::sleep(slow).await;
                true
            },
            || async move {
                tokio::time::sleep(slow).await;
                Some(running())
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(out, ReadyOutcome::Ready);
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn unknown_state_does_not_fail_readiness() {
        let out =
            wait_until_ready(|| async { true }, || async { None }, Duration::from_secs(1)).await;
        assert_eq!(out, ReadyOutcome::Ready);
    }

    #[test]
    fn log_tail_strips_k8s_file_prefix_and_keeps_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.log");
        let mut log = String::new();
        for i in 0..50 {
            log.push_str(&format!(
                "2026-09-26T15:36:23.195532122+05:30 stderr F line {i}\n"
            ));
        }
        log.push_str("2026-09-26T15:36:24.000000000+05:30 stdout F Error: Read-only file system\n");
        std::fs::write(&path, log).unwrap();

        let tail = log_tail(&path, 3);
        assert_eq!(tail, "line 48\nline 49\nError: Read-only file system");
        assert_eq!(log_tail(&dir.path().join("missing.log"), 3), "");
    }

    #[test]
    fn not_ready_errors_say_why() {
        let t = Duration::from_secs(30);
        let exited = ReadyOutcome::Died(ContainerState::parse("exited 1 0").unwrap());
        let msg = not_ready_error(&exited, 3100, 3000, t, "boom");
        assert!(
            msg.contains("exit code 1") && msg.ends_with("boom"),
            "{msg}"
        );

        let looped = ReadyOutcome::Died(ContainerState::parse("running 4 3").unwrap());
        assert!(not_ready_error(&looped, 3100, 3000, t, "").contains("restarted 3 time(s)"));

        let msg = not_ready_error(&ReadyOutcome::TimedOut, 3100, 3000, t, "");
        assert!(
            msg.contains("0.0.0.0:3000") && msg.contains("printed no output"),
            "{msg}"
        );
    }
}
