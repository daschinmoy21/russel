//! Startup reconcile: rehydrate service state from on-disk metadata
//! after control plane restart so APIs return accurate status without
//! waiting for `GET /vms` lazy discovery (AUDIT BUG-07 / GitHub #10).

use std::path::Path;

use russel_core::config::RuntimeKind;

use crate::metadata::{self, ServiceDiskRecord};
use crate::network::PortAllocator;
use crate::state::AppState;

/// Outcome of reconciling a single service directory.
#[derive(Debug)]
enum ReconcileOutcome {
    Running,
    Stopped,
    Skipped,
}

/// Summary report produced by a startup reconcile pass.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReconcileReport {
    pub adopted_running: usize,
    pub stopped: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// Reconcile all services under `/var/lib/russel`.
pub async fn reconcile_startup(state: &AppState) -> ReconcileReport {
    reconcile_startup_in(state, Path::new("/var/lib/russel")).await
}

/// Reconcile all services under an arbitrary base directory (for tests).
pub async fn reconcile_startup_in(state: &AppState, base: &Path) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    let Ok(entries) = std::fs::read_dir(base) else {
        tracing::warn!(base = %base.display(), "cannot read base directory for reconcile");
        report.errors += 1;
        return report;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };

        // Skip backups and well-known non-service directories.
        if name.ends_with(".bak") || name == "traefik" {
            report.skipped += 1;
            continue;
        }

        if !entry.path().join("metadata.json").exists() {
            report.skipped += 1;
            continue;
        }

        let meta_path = entry.path().join("metadata.json");

        match reconcile_service(state, &name, &meta_path).await {
            Ok(ReconcileOutcome::Running) => report.adopted_running += 1,
            Ok(ReconcileOutcome::Stopped) => report.stopped += 1,
            Ok(ReconcileOutcome::Skipped) => report.skipped += 1,
            Err(e) => {
                tracing::warn!(service_id = %name, error = %e, "reconcile error");
                report.errors += 1;
            }
        }
    }

    // Write durable catalog after reconcile pass.
    if let Err(e) = state.write_catalog() {
        tracing::warn!(error = %e, "failed to write ctrl-catalog.json after reconcile");
    }

    tracing::info!(
        adopted_running = report.adopted_running,
        stopped = report.stopped,
        skipped = report.skipped,
        errors = report.errors,
        "startup reconcile complete"
    );

    report
}

async fn reconcile_service(
    state: &AppState,
    service_id: &str,
    metadata_path: &Path,
) -> anyhow::Result<ReconcileOutcome> {
    let record = match metadata::load_service_disk_record_from(metadata_path) {
        Some(r) => r,
        None => return Ok(ReconcileOutcome::Skipped),
    };

    let runtime = record.runtime.unwrap_or(RuntimeKind::Microvm);

    let alive = match runtime {
        RuntimeKind::Microvm => probe_microvm_alive(&record),
        RuntimeKind::Container => probe_container_alive(&record).await,
    };

    if alive {
        let host_port = record.host_port.unwrap_or(0);
        let guest_port = record.guest_port.unwrap_or(0);

        // Claim the port so the allocator knows it's taken.
        if host_port > 0
            && let Err(e) = PortAllocator::claim_existing(service_id, host_port)
        {
            tracing::warn!(
                service_id = %service_id,
                host_port,
                error = %e,
                "failed to claim existing port during reconcile"
            );
        }

        match runtime {
            RuntimeKind::Microvm => {
                state.adopt_running_microvm(service_id, host_port, guest_port, record.vm_pid);
            }
            RuntimeKind::Container => {
                if let Some(ref container_id) = record.container_id {
                    state.adopt_running_container(service_id, container_id, host_port, guest_port);
                } else {
                    // Container metadata without a container_id — treat as stopped.
                    state.mark_stopped_from_disk(
                        service_id,
                        runtime,
                        record.host_port,
                        record.guest_port,
                    );
                    return Ok(ReconcileOutcome::Stopped);
                }
            }
        }

        Ok(ReconcileOutcome::Running)
    } else {
        // Service on disk but processes are dead — ensure stopped entry exists
        // with port/runtime fields so status APIs work without /vms first.
        state.mark_stopped_from_disk(service_id, runtime, record.host_port, record.guest_port);
        Ok(ReconcileOutcome::Stopped)
    }
}

// ── Liveness probes ───────────────────────────────────────────────────────────

/// Check whether a microVM's constituent processes are alive *and* match the
/// expected identity (guards against PID reuse after ctrl restart).
fn probe_microvm_alive(record: &ServiceDiskRecord) -> bool {
    let service_id = record.service_id.as_deref().unwrap_or("");

    if let Some(pid) = record.vm_pid
        && pid_matches(pid, &["cloud-hypervisor"], service_id)
    {
        return true;
    }

    if let Some(pid) = record.socat_pid
        && pid_matches(pid, &["socat", "socat-russel"], service_id)
    {
        return true;
    }

    for &pid in &record.virtiofsd_pids {
        if pid_matches(pid, &["virtiofsd"], service_id) {
            return true;
        }
    }

    false
}

/// Check whether a container is still running via `podman inspect`.
async fn probe_container_alive(record: &ServiceDiskRecord) -> bool {
    if let Some(ref container_id) = record.container_id {
        if container_running(container_id).await {
            return true;
        }
        // Fallback: try the Russel naming convention `russel-{service_id}`.
        let name = format!(
            "russel-{}",
            record.service_id.as_deref().unwrap_or("unknown")
        );
        if container_running(&name).await {
            return true;
        }
    }
    false
}

/// Returns true if the PID exists *and* its cmdline matches expected identity.
///
/// `kill(pid, 0)` alone is subject to PID reuse; we also require that
/// `/proc/<pid>/cmdline` contains one of `needles` (and, when non-empty,
/// the service id) before treating the process as our workload.
fn pid_matches(pid: u32, needles: &[&str], service_id: &str) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs existence check without delivering a signal.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    let cmdline = match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).replace('\0', " "),
        Err(_) => return false,
    };
    let has_needle = needles.iter().any(|n| cmdline.contains(n));
    if !has_needle {
        return false;
    }
    // When we know the service id, require it appear (path, arg0, or similar)
    // so a recycled PID running the same binary for another service is rejected.
    if !service_id.is_empty() && !cmdline.contains(service_id) {
        // cloud-hypervisor may only embed the TAP name, not the service id;
        // accept if any needle already matched and service_id is empty-checked above.
        // For socat/virtiofsd the service id is always in the path or arg0.
        if needles.contains(&"cloud-hypervisor") {
            // CH cmdline has tap=rsl-... and path under /var/lib/russel/{id}/
            // If service_id not present, still require russel path fragment.
            return cmdline.contains("/var/lib/russel/") || cmdline.contains("tap=");
        }
        return false;
    }
    true
}

async fn container_running(container_id: &str) -> bool {
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
    String::from_utf8_lossy(&output.stdout).trim() == "true"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Write minimal microVM metadata into `{base}/{service_id}/metadata.json`.
    fn write_microvm_metadata(
        base: &Path,
        service_id: &str,
        vm_pid: Option<u32>,
        socat_pid: Option<u32>,
        host_port: u16,
    ) -> PathBuf {
        let dir = base.join(service_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "schema_version": 1,
            "service_id": service_id,
            "runtime": "microvm",
            "host_port": host_port,
            "guest_port": 3000,
            "vm_pid": vm_pid,
            "socat_pid": socat_pid,
            "virtiofsd_pids": []
        });
        let path = dir.join("metadata.json");
        std::fs::write(&path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        path
    }

    /// Write minimal container metadata.
    #[allow(dead_code)] // used by future container reconcile tests
    fn write_container_metadata(
        base: &Path,
        service_id: &str,
        container_id: &str,
        host_port: u16,
    ) -> PathBuf {
        let dir = base.join(service_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "schema_version": 1,
            "service_id": service_id,
            "runtime": "container",
            "host_port": host_port,
            "guest_port": 3000,
            "container_id": container_id
        });
        let path = dir.join("metadata.json");
        std::fs::write(&path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        path
    }

    /// Spawn a long-lived shell whose argv contains the socat identity for
    /// `service_id`. Using a system shell avoids parallel-test races around
    /// creating and executing temporary script files.
    fn spawn_fake_socat(service_id: &str) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0(format!("socat-russel-{service_id}"));
        }
        let child = command
            .args(["-c", "sleep 30; wait"])
            .spawn()
            .expect("spawn fake socat");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pid_matches(child.id(), &["socat", "socat-russel"], service_id)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(pid_matches(
            child.id(),
            &["socat", "socat-russel"],
            service_id
        ));
        child
    }

    #[test]
    fn pid_matches_rejects_unrelated_live_pid() {
        // Current test process is alive but is not cloud-hypervisor/socat.
        let me = std::process::id();
        assert!(!pid_matches(me, &["cloud-hypervisor"], "api"));
        assert!(!pid_matches(me, &["socat", "socat-russel"], "api"));
    }

    #[tokio::test]
    async fn reconcile_running_microvm_adopted() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let mut fake = spawn_fake_socat("api");
        let pid = fake.id();

        write_microvm_metadata(base, "api", None, Some(pid), 3100);

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.adopted_running, 1);
        assert_eq!(report.stopped, 0);
        let status = state.status("api").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
        assert_eq!(status.host_port, Some(3100));
        assert_eq!(status.runtime, Some(RuntimeKind::Microvm));
        let _ = fake.kill();
        let _ = fake.wait();
    }

    #[tokio::test]
    async fn reconcile_dead_microvm_stopped() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // Use a PID that certainly does not exist (large number, not 0 or 1).
        write_microvm_metadata(base, "dead-vm", Some(999_999_999), None, 3101);

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.adopted_running, 0);
        assert_eq!(report.stopped, 1);
        let status = state.status("dead-vm").unwrap();
        assert_eq!(status.status, "stopped");
        assert_eq!(status.vm_state, "none");
    }

    #[tokio::test]
    async fn reconcile_skips_backup_dirs() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        let bak_dir = base.join("api.bak");
        std::fs::create_dir_all(&bak_dir).unwrap();
        std::fs::write(
            bak_dir.join("metadata.json"),
            serde_json::to_string(&serde_json::json!({"service_id":"api"})).unwrap(),
        )
        .unwrap();

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.skipped, 1);
        assert_eq!(report.adopted_running, 0);
        assert_eq!(report.stopped, 0);
    }

    #[tokio::test]
    async fn reconcile_skips_traefik_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        let traefik_dir = base.join("traefik");
        std::fs::create_dir_all(&traefik_dir).unwrap();
        std::fs::write(
            traefik_dir.join("metadata.json"),
            serde_json::to_string(&serde_json::json!({"service_id":"traefik"})).unwrap(),
        )
        .unwrap();

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert!(report.skipped >= 1);
        assert_eq!(report.adopted_running, 0);
        assert_eq!(report.stopped, 0);
    }

    #[tokio::test]
    async fn reconcile_skips_non_dirs() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        std::fs::write(base.join("readme.txt"), "hello").unwrap();

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.adopted_running, 0);
    }

    #[tokio::test]
    async fn reconcile_skips_dirs_without_metadata() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        std::fs::create_dir_all(base.join("no-meta")).unwrap();

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.skipped, 1);
    }

    #[tokio::test]
    async fn reconcile_running_microvm_claims_port() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let mut fake = spawn_fake_socat("port-svc");
        let pid = fake.id();

        write_microvm_metadata(base, "port-svc", None, Some(pid), 9000);

        let state = AppState::default();
        let _report = reconcile_startup_in(&state, base).await;

        // Port claim should succeed for the adopted service.
        assert!(PortAllocator::claim_existing("port-svc", 9000).is_ok());
        // A different service should be rejected for the same port.
        let err = PortAllocator::claim_existing("other-svc", 9000).unwrap_err();
        assert!(err.to_string().contains("already claimed"));

        PortAllocator::release("port-svc");
        let _ = fake.kill();
        let _ = fake.wait();
    }

    #[tokio::test]
    async fn reconcile_does_not_overwrite_live_handles() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let mut fake = spawn_fake_socat("live-svc");
        let pid = fake.id();

        write_microvm_metadata(base, "live-svc", None, Some(pid), 3105);

        let state = AppState::default();

        // Pre-populate with a real (spawned) Child process.
        let child = tokio::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleep");
        state.mark_deployed_with_aux("live-svc", child, vec![]);

        let _report = reconcile_startup_in(&state, base).await;

        // The in-memory state still reflects the deployed status,
        // and vm_process is Some (not overwritten).
        let inner = state.lock_inner();
        let svc = inner.services.get("live-svc").unwrap();
        assert!(svc.vm_process.is_some(), "live Child handle preserved");
        assert_eq!(svc.status, "deployed");
        assert_eq!(svc.vm_state, "running");
        drop(inner);
        let _ = fake.kill();
        let _ = fake.wait();
    }

    #[tokio::test]
    async fn reconcile_rejects_pid_reuse_without_identity() {
        // Current process PID is live but not a russel workload binary.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        write_microvm_metadata(base, "reuse-svc", Some(std::process::id()), None, 3300);

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;
        assert_eq!(report.adopted_running, 0);
        assert_eq!(report.stopped, 1);
    }

    #[tokio::test]
    async fn reconcile_stopped_service_still_queryable() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        write_microvm_metadata(base, "stopped-svc", Some(999_999_999), None, 3200);

        let state = AppState::default();
        let _report = reconcile_startup_in(&state, base).await;

        // Status is available without calling /vms first.
        let status = state.status("stopped-svc").unwrap();
        assert_eq!(status.status, "stopped");
        assert_eq!(status.host_port, Some(3200));
    }

    #[tokio::test]
    async fn reconcile_multiple_services() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let mut a = spawn_fake_socat("live-a");
        let mut b = spawn_fake_socat("live-b");

        write_microvm_metadata(base, "live-a", None, Some(a.id()), 3110);
        write_microvm_metadata(base, "live-b", None, Some(b.id()), 3111);
        write_microvm_metadata(base, "dead-c", Some(999_999_999), None, 3112);

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.adopted_running, 2);
        assert_eq!(report.stopped, 1);
        let _ = a.kill();
        let _ = a.wait();
        let _ = b.kill();
        let _ = b.wait();
        assert_eq!(state.list_services().len(), 3);
    }

    #[tokio::test]
    async fn reconcile_with_socat_or_virtiofsd_pid_alive() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let mut fake = spawn_fake_socat("socat-only");

        let dir = base.join("socat-only");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "schema_version": 1,
            "service_id": "socat-only",
            "runtime": "microvm",
            "host_port": 3400,
            "guest_port": 3000,
            "socat_pid": fake.id(),
            "virtiofsd_pids": []
        });
        std::fs::write(
            dir.join("metadata.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let state = AppState::default();
        let report = reconcile_startup_in(&state, base).await;

        assert_eq!(report.adopted_running, 1);
        let status = state.status("socat-only").unwrap();
        assert_eq!(status.status, "deployed");
        let _ = fake.kill();
        let _ = fake.wait();
    }
}
