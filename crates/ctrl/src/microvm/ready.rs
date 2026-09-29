//! MicroVM readiness (#493). The agent powers the guest off when the app
//! exits, so the `cloud-hypervisor` process exiting is the app-died signal:
//! readiness races the network probe against it, and a replacement for a live
//! generation is watched for a while after it answers (`watch`).

use std::future::Future;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;

/// Lines of guest console quoted in a failed deploy.
pub const CONSOLE_TAIL_LINES: usize = 80;

#[derive(Debug)]
pub enum ReadyOutcome {
    Ready,
    /// Cloud Hypervisor exited before the app answered or during the settle
    /// window: the app exited and the agent powered the guest off, or the
    /// VM itself failed.
    Exited(Option<ExitStatus>),
    /// The VM kept running, but nothing answered in time.
    TimedOut,
}

/// Wait for `probe` (the network readiness check, bounded by its own
/// timeout) while watching the VM process, then require the VM to stay up
/// for `settle` (zero: return at the first answer). A VM exit at any point
/// wins over an answer, and fails at once rather than after the probe's
/// timeout.
pub async fn wait_until_ready<P>(probe: P, vm: &mut Child, settle: Duration) -> ReadyOutcome
where
    P: Future<Output = bool>,
{
    tokio::select! {
        biased;
        status = vm.wait() => return ReadyOutcome::Exited(status.ok()),
        up = probe => {
            if !up {
                // The probe gave up; say so only if the VM is still running.
                return match vm.try_wait() {
                    Ok(Some(status)) => ReadyOutcome::Exited(Some(status)),
                    _ => ReadyOutcome::TimedOut,
                };
            }
        }
    }
    match watch(vm, settle).await {
        Some(status) => ReadyOutcome::Exited(status),
        None => ReadyOutcome::Ready,
    }
}

/// Watch a running VM for `window`. `Some` when it exited meanwhile (with its
/// status, if it could be read), `None` when it was still up at the end.
pub async fn watch(vm: &mut Child, window: Duration) -> Option<Option<ExitStatus>> {
    if let Ok(Some(status)) = vm.try_wait() {
        return Some(Some(status));
    }
    if window.is_zero() {
        return None;
    }
    tokio::select! {
        biased;
        status = vm.wait() => Some(status.ok()),
        () = tokio::time::sleep(window) => None,
    }
}

/// Last `lines` lines of a guest serial log.
pub fn console_tail(path: &Path, lines: usize) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = raw.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The app's exit status as the agent logged it (`app exited with status N`).
pub fn app_exit_status(console: &str) -> Option<i32> {
    console.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix("app exited with status ")?
            .split(';')
            .next()?
            .trim()
            .parse()
            .ok()
    })
}

/// Deploy error for a guest that exited during startup.
pub fn exited_error(status: Option<ExitStatus>, console_tail: &str) -> String {
    let head = match (app_exit_status(console_tail), status) {
        (Some(code), _) => format!("app exited during startup (exit code {code})"),
        (None, Some(s)) => format!("microVM exited during startup (cloud-hypervisor {s})"),
        (None, None) => "microVM exited during startup".to_string(),
    };
    if console_tail.trim().is_empty() {
        format!("{head}; the guest console is empty")
    } else {
        format!("{head}. Last guest console output:\n{console_tail}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn spawn(script: &str) -> Child {
        tokio::process::Command::new("sh")
            .args(["-c", script])
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn ready_when_app_answers_and_vm_stays_up() {
        let mut vm = spawn("sleep 30");
        let out = wait_until_ready(async { true }, &mut vm, Duration::from_millis(50)).await;
        assert!(matches!(out, ReadyOutcome::Ready), "{out:?}");
    }

    #[tokio::test]
    async fn vm_exit_during_settle_fails_after_probe_passed() {
        // The #493 case: the port answered, then the app died and the guest
        // powered off.
        let mut vm = spawn("sleep 0.2; exit 3");
        let out = wait_until_ready(async { true }, &mut vm, Duration::from_secs(5)).await;
        match out {
            ReadyOutcome::Exited(Some(s)) => assert_eq!(s.code(), Some(3)),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vm_exit_before_answer_fails_without_waiting_for_the_probe() {
        let mut vm = spawn("exit 1");
        let probe = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            false
        };
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            wait_until_ready(probe, &mut vm, Duration::from_secs(2)),
        )
        .await
        .expect("a VM exit must not wait out the probe");
        assert!(matches!(out, ReadyOutcome::Exited(Some(_))), "{out:?}");
    }

    #[tokio::test]
    async fn zero_settle_returns_at_the_first_answer() {
        // First deploys: nothing live to protect, so no wait after the answer.
        let mut vm = spawn("sleep 30");
        let out = tokio::time::timeout(
            Duration::from_millis(500),
            wait_until_ready(async { true }, &mut vm, Duration::ZERO),
        )
        .await
        .expect("zero settle must not wait");
        assert!(matches!(out, ReadyOutcome::Ready), "{out:?}");
    }

    #[tokio::test]
    async fn watch_reports_an_exit_inside_the_window_only() {
        let mut dies = spawn("sleep 0.1; exit 4");
        let status = watch(&mut dies, Duration::from_secs(5)).await;
        assert_eq!(status.flatten().and_then(|s| s.code()), Some(4));
        let mut lives = spawn("sleep 30");
        assert!(
            watch(&mut lives, Duration::from_millis(100))
                .await
                .is_none()
        );
        // Already gone before the window starts.
        let mut gone = spawn("exit 2");
        let _ = gone.wait().await;
        assert!(watch(&mut gone, Duration::ZERO).await.is_some());
    }

    #[tokio::test]
    async fn timed_out_when_vm_runs_but_nothing_answers() {
        let mut vm = spawn("sleep 30");
        let out = wait_until_ready(async { false }, &mut vm, Duration::from_secs(2)).await;
        assert!(matches!(out, ReadyOutcome::TimedOut), "{out:?}");
    }

    #[test]
    fn reads_app_exit_status_from_console() {
        let console = "starting /app/bin/postgres (3 args)\n\
                       FATAL: could not open shared memory segment\n\
                       app exited with status 1; powering off\n";
        assert_eq!(app_exit_status(console), Some(1));
        assert_eq!(app_exit_status("booting\n"), None);
        let msg = exited_error(None, console);
        assert!(
            msg.starts_with("app exited during startup (exit code 1)"),
            "{msg}"
        );
        assert!(msg.contains("shared memory segment"), "{msg}");
        assert!(exited_error(None, "").contains("guest console is empty"));
    }

    #[test]
    fn console_tail_keeps_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(console_tail(&path, 2), "b\nc");
        assert_eq!(console_tail(&dir.path().join("missing"), 2), "");
    }
}
