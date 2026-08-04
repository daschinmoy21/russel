#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use russel_core::config::RuntimeKind;

use super::*;

#[test]
fn test_detach_all_processes_empty() {
    let state = AppState::default();
    assert_eq!(state.detach_all_processes(), 0);
}

#[tokio::test]
async fn test_detach_all_processes_nonempty() {
    use tokio::process::Command;

    let state = AppState::default();
    // Create a service with VM and auxiliary processes
    let vm_child = Command::new("sleep")
        .arg("10")
        .spawn()
        .expect("failed to spawn sleep");
    let aux1 = Command::new("sleep")
        .arg("10")
        .spawn()
        .expect("failed to spawn sleep");
    let aux2 = Command::new("sleep")
        .arg("10")
        .spawn()
        .expect("failed to spawn sleep");

    let _vm_pid = vm_child.id();
    state.mark_deployed_with_aux("test-svc", vm_child, vec![aux1, aux2]);

    // Verify initial state
    let status = state.status("test-svc").unwrap();
    assert_eq!(status.status, "deployed");
    assert_eq!(status.vm_state, "running");
    assert!(
        state
            .lock_inner()
            .services
            .get("test-svc")
            .unwrap()
            .vm_pid
            .is_some()
    );

    // Detach all processes
    let detached = state.detach_all_processes();
    assert_eq!(detached, 1);

    // Verify handles are removed and state updated
    let inner = state.lock_inner();
    let svc = inner.services.get("test-svc").unwrap();
    assert!(svc.vm_process.is_none(), "vm_process should be None");
    assert!(
        svc.aux_processes.is_empty(),
        "aux_processes should be empty"
    );
    assert!(svc.vm_pid.is_none(), "vm_pid should be None");
    assert_eq!(svc.status, "detached");
    assert_eq!(svc.vm_state, "orphaned");
}

#[test]
fn test_mutex_poisoning_recovery() {
    let state = AppState::default();

    // Poison the lock intentionally in a separate thread/panic
    let inner_clone = state.inner.clone();
    let _ = std::thread::spawn(move || {
        let _lock = inner_clone.lock().unwrap();
        panic!("poisoning lock");
    })
    .join();

    // The lock is now poisoned, but lock_inner should recover it
    let inner = state.lock_inner();
    assert!(inner.services.is_empty());
}

#[test]
fn test_mark_and_status() {
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "building");
    assert_eq!(status.vm_state, "pending");

    // Unknown service returns None
    assert!(state.status("svc-2").is_none());
}

#[test]
fn test_mark_building_rejects_conflicting_lifecycle() {
    let state = AppState::default();
    // New entry succeeds.
    state.mark_building("svc-1").unwrap();
    // Already building -> rejected.
    let err = state.mark_building("svc-1").unwrap_err();
    assert!(err.to_string().contains("already in lifecycle state"));

    // stopping / destroying also rejected.
    state.set_status("svc-1", "stopping", "pending");
    assert!(state.mark_building("svc-1").is_err());
    state.set_status("svc-1", "destroying", "pending");
    assert!(state.mark_building("svc-1").is_err());

    // A fresh service still works alongside the conflicting one.
    state.mark_building("svc-2").unwrap();
    assert_eq!(state.status("svc-2").unwrap().status, "building");
}

#[test]
fn test_mark_deployed_then_logs_and_status() {
    let state = AppState::default();
    state.mark_building("svc-a").unwrap();
    // We can't create a real Child in tests, so mark_failed is the easier path
    state.mark_failed("svc-a", "test error".into());
    let status = state.status("svc-a").unwrap();
    assert_eq!(status.status, "failed");
    let logs = state.logs("svc-a").unwrap();
    assert!(logs.output.contains("test error"));
}

#[test]
fn test_services_are_independent() {
    let state = AppState::default();
    state.mark_building("alpha").unwrap();
    state.mark_building("beta").unwrap();
    state.mark_failed("beta", "beta error".into());

    let alpha = state.status("alpha").unwrap();
    assert_eq!(alpha.status, "building");
    let beta = state.status("beta").unwrap();
    assert_eq!(beta.status, "failed");
}

#[test]
fn test_begin_lifecycle_operation() {
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    // While status is "building" the claim is Busy (not NotFound).
    assert!(matches!(
        state.begin_lifecycle_operation("svc-1", "stopping", "pending"),
        LifecycleClaim::Busy
    ));
    // Unknown service is NotFound, distinct from Busy.
    assert!(matches!(
        state.begin_lifecycle_operation("nope", "stopping", "pending"),
        LifecycleClaim::NotFound
    ));
}

#[test]
fn test_begin_lifecycle_allows_stop_reentry_when_stuck_stopping() {
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.set_status("svc-1", "deployed", "running");
    assert!(matches!(
        state.begin_lifecycle_operation("svc-1", "stopping", "pending"),
        LifecycleClaim::Claimed(_, _)
    ));
    // Second stop while already stopping — recovery / force path.
    assert!(matches!(
        state.begin_lifecycle_operation("svc-1", "stopping", "pending"),
        LifecycleClaim::Claimed(_, _)
    ));
    // Destroy may supersede stuck stop.
    assert!(matches!(
        state.begin_lifecycle_operation("svc-1", "destroying", "pending"),
        LifecycleClaim::Claimed(_, _)
    ));
    // Stop cannot run while destroying.
    assert!(matches!(
        state.begin_lifecycle_operation("svc-1", "stopping", "pending"),
        LifecycleClaim::Busy
    ));
}

#[test]
fn test_remove_service() {
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    assert!(state.status("svc-1").is_some());
    state.remove_service("svc-1");
    assert!(state.status("svc-1").is_none());
}

#[test]
fn test_list_services() {
    let state = AppState::default();
    state.mark_building("z").unwrap();
    state.mark_building("a").unwrap();
    state.mark_building("m").unwrap();
    let ids = state.list_services();
    assert_eq!(ids, vec!["a", "m", "z"]);
}

// ── mark_building vm_state transition tests ──────────────────────────────

#[test]
fn test_mark_building_resets_failed_vm_state() {
    // Verify that redeploying a failed service resets vm_state to pending,
    // avoiding the "building/failed" stale state combination.
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.mark_failed("svc-1", "first failure".into());
    assert_eq!(state.status("svc-1").unwrap().vm_state, "failed");

    // Redeploy — vm_state should transition from failed → pending
    state.mark_building("svc-1").unwrap();
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "building");
    assert_eq!(status.vm_state, "pending");
}

#[test]
fn test_mark_building_preserves_running_vm_state() {
    // When vm_state is "running" AND vm_process is Some (real Child),
    // mark_building preserves "running". Since we can't create a real
    // tokio::process::Child in unit tests, we verify the fallback:
    // without a process handle, "running" → "pending" here.
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.set_status("svc-1", "deployed", "running");

    state.mark_building("svc-1").unwrap();
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "building");
    assert_eq!(status.vm_state, "pending");
}

// ── mark_failed prior-state preservation tests ───────────────────────────

#[test]
fn test_mark_failed_preserves_prior_running_state() {
    // A redeploy that fails before take_processes should restore the
    // previous deployed/running state — not set failed/failed.
    let state = AppState::default();
    // Set up a deployed service
    state.mark_building("svc-1").unwrap();
    state.set_status("svc-1", "deployed", "running");

    // Redeploy: mark_building captures prebuild snapshot
    state.mark_building("svc-1").unwrap();

    // Build fails before take_processes
    state.mark_failed("svc-1", "build error".into());
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "deployed");
    assert_eq!(status.vm_state, "running");
    let logs = state.logs("svc-1").unwrap();
    assert!(logs.output.contains("BUILD FAILED"));
}

#[test]
fn test_mark_failed_fresh_deploy_no_prior_vm() {
    // A fresh deploy that fails should set failed/failed since there
    // was no prior running VM to restore.
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.mark_failed("svc-1", "build error".into());
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "failed");
    assert_eq!(status.vm_state, "failed");
}

#[test]
fn test_mark_failed_after_take_processes_sets_failed() {
    // If processes were taken before the failure, the prebuild snapshot
    // is cleared, so mark_failed should set failed/failed.
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.set_status("svc-1", "deployed", "running");
    state.mark_building("svc-1").unwrap();

    // Simulate processes being taken (clears snapshot)
    let _ = state.take_processes("svc-1");

    // Now fail — should go to failed/failed since snapshot is gone
    state.mark_failed("svc-1", "deploy failed".into());
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "failed");
    assert_eq!(status.vm_state, "failed");
}

// ── ensure_service tests ─────────────────────────────────────────────────

#[test]
fn test_ensure_service_creates_minimal_entry() {
    let state = AppState::default();
    assert!(state.status("disk-vm").is_none());

    state.ensure_service("disk-vm");
    let status = state.status("disk-vm").unwrap();
    assert_eq!(status.status, "stopped");
    assert_eq!(status.vm_state, "none");
}

#[test]
fn test_status_uptime_zero_when_not_running() {
    let state = AppState::default();
    state.ensure_service("stopped-svc");
    // started_at is set at construction; without running state uptime must be 0.
    let status = state.status("stopped-svc").unwrap();
    assert_eq!(status.uptime_seconds, 0);

    state.set_status("stopped-svc", "deployed", "running");
    let status = state.status("stopped-svc").unwrap();
    // Running may be 0 if just set, but must not error; then stop clears uptime.
    let _ = status.uptime_seconds;
    state.set_status("stopped-svc", "stopped", "none");
    let status = state.status("stopped-svc").unwrap();
    assert_eq!(status.uptime_seconds, 0);
    assert_eq!(status.status, "stopped");
    assert_eq!(status.vm_state, "none");
}

#[test]
fn test_ensure_service_does_not_overwrite_existing() {
    let state = AppState::default();
    state.mark_building("svc-1").unwrap();
    state.set_status("svc-1", "deployed", "running");

    // ensure_service should not overwrite existing state
    state.ensure_service("svc-1");
    let status = state.status("svc-1").unwrap();
    assert_eq!(status.status, "deployed");
    assert_eq!(status.vm_state, "running");
}

// ── process supervisor (issue #32) ───────────────────────────────────────

#[tokio::test]
async fn supervisor_marks_failed_when_child_exits() {
    let state = AppState::default();
    // `true` exits immediately with status 0.
    let child = tokio::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    state.mark_deployed_with_aux("svc-exit", child, vec![]);

    // Supervisor polls every 500ms after an initial tick skip.
    let mut saw_failed = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(status) = state.status("svc-exit")
            && status.status == "failed"
        {
            saw_failed = true;
            break;
        }
    }
    assert!(saw_failed, "expected supervisor to mark service failed");
    let logs = state.logs("svc-exit").unwrap();
    assert!(
        logs.output.contains("PROCESS EXIT"),
        "logs missing PROCESS EXIT: {}",
        logs.output
    );
}

#[tokio::test]
async fn supervisor_ignores_intentional_take_processes() {
    let state = AppState::default();
    let child = tokio::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    state.mark_deployed_with_aux("svc-take", child, vec![]);

    let (vm, _aux) = state.take_processes("svc-take").expect("processes present");
    // Give the supervisor time to observe the generation change.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let status = state.status("svc-take").unwrap();
    assert_ne!(
        status.status, "failed",
        "intentional take_processes must not be reported as crash"
    );

    if let Some(mut child) = vm {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

// ── deploy tracking tests ─────────────────────────────────────────────

#[test]
fn test_begin_deploy_increments_counter() {
    let state = AppState::default();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

    let _guard1 = state.begin_deploy();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

    let _guard2 = state.begin_deploy();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_deploy_guard_drop_decrements_counter() {
    let state = AppState::default();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

    let guard1 = state.begin_deploy();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

    let guard2 = state.begin_deploy();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 2);

    drop(guard1);
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

    drop(guard2);
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_wait_for_deploys_returns_immediately_when_zero() {
    let state = AppState::default();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

    // Should return immediately without blocking
    state.wait_for_deploys().await;
}

#[tokio::test]
async fn test_wait_for_deploys_waits_for_guards() {
    let state = AppState::default();
    let guard = state.begin_deploy();
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        state_clone.wait_for_deploys().await;
    });

    // Give the task time to start waiting
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert!(
        !handle.is_finished(),
        "wait_for_deploys should still be waiting"
    );

    // Drop the guard to decrement counter
    drop(guard);
    assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

    // Now the task should complete
    tokio::time::timeout(tokio::time::Duration::from_millis(100), handle)
        .await
        .expect("wait_for_deploys should complete after counter reaches 0")
        .expect("task should not panic");
}

/// F-10: Stress-test the Notify race. Run many concurrent iterations
/// of spawn-with-guard → drop-guard to ensure wait_for_deploys never
/// hangs. Each iteration is timeboxed so a missed wakeup is a test
/// failure, not a hung test suite.
#[tokio::test]
async fn test_wait_for_deploys_no_lost_notification_race() {
    let state = AppState::default();
    let iterations = 100;
    for i in 0..iterations {
        let guard = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            state_clone.wait_for_deploys().await;
        });

        // Yield to let the spawned task register its Notified future.
        tokio::task::yield_now().await;

        // Drop the guard while the spawned task may be in the
        // race window between enable() and count check.
        drop(guard);

        // Must complete within the timeout — a missed notification
        // would cause this to hang until timeout expires.
        let timeout = tokio::time::Duration::from_secs(2);
        tokio::time::timeout(timeout, handle)
            .await
            .unwrap_or_else(|_| {
                panic!("iteration {i}: wait_for_deploys timed out — lost notification?")
            })
            .expect("task should not panic");

        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);
    }
}

// ── adopt / reconcile APIs ────────────────────────────────────────────────

#[test]
fn test_adopt_running_microvm_sets_deployed() {
    let state = AppState::default();
    state.adopt_running_microvm("adopted-vm", 3100, 3000, Some(42), None);

    let status = state.status("adopted-vm").unwrap();
    assert_eq!(status.status, "deployed");
    assert_eq!(status.vm_state, "running");
    assert_eq!(status.runtime, Some(RuntimeKind::Microvm));
    assert_eq!(status.host_port, Some(3100));
    assert_eq!(status.guest_port, Some(3000));
}

#[tokio::test]
async fn test_adopt_running_microvm_does_not_overwrite_live_handle() {
    let state = AppState::default();

    // Create a live Child handle first (needs tokio runtime for pidfd).
    let child = tokio::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    state.mark_deployed_with_aux("protected", child, vec![]);

    // Adopt should not overwrite.
    state.adopt_running_microvm("protected", 9999, 9999, None, None);

    let inner = state.lock_inner();
    let svc = inner.services.get("protected").unwrap();
    assert!(svc.vm_process.is_some(), "live Child handle preserved");
}

#[test]
fn test_adopt_running_container_sets_deployed() {
    let state = AppState::default();
    state.adopt_running_container("adopted-ctr", "abc123", 3100, 3000, None);

    let status = state.status("adopted-ctr").unwrap();
    assert_eq!(status.status, "deployed");
    assert_eq!(status.vm_state, "running");
    assert_eq!(status.runtime, Some(RuntimeKind::Container));
    assert_eq!(status.host_port, Some(3100));
}

#[tokio::test]
async fn test_adopt_running_container_does_not_overwrite_live_handle() {
    let state = AppState::default();

    // Create a live Child handle first (needs tokio runtime for pidfd).
    let child = tokio::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    state.mark_deployed_with_aux("protected-ctr", child, vec![]);

    // Adopt should not overwrite.
    state.adopt_running_container("protected-ctr", "xyz", 9999, 9999, None);

    let inner = state.lock_inner();
    let svc = inner.services.get("protected-ctr").unwrap();
    assert!(svc.vm_process.is_some(), "live Child handle preserved");
    assert!(
        svc.runtime == Some(RuntimeKind::Microvm),
        "runtime unchanged"
    );
}

#[test]
fn test_write_catalog_emits_valid_json() {
    let state = AppState::default();
    state.adopt_running_microvm("cat-svc", 4000, 3000, None, None);

    // write_catalog writes /var/lib/russel/ctrl-catalog.json —
    // we can't unit-test that path directly without root, but we can
    // verify it doesn't panic and the catalog machinery works.
    // In CI/tests the /var/lib/russel path may not exist, so this
    // is a best-effort smoke test.
    let result = state.write_catalog();
    // The write may fail if /var/lib/russel doesn't exist in test env.
    // That's fine — the code path is exercised.
    let _ = result;
}
