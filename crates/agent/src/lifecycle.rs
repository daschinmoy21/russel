//! Agent-side lifecycle execution for ctrl RPC (#214).
//!
//! Reuses `russel-ctrl`'s in-process runners — the same teardown logic the
//! monolithic control plane uses — after resolving the runtime kind from
//! on-disk metadata. The agent performs the actual process teardown; ingress
//! and in-memory state bookkeeping stay with the control plane (single writer
//! for proxy config / service state).
//!
//! These handlers deliberately call `in_process_lifecycle`, never
//! `lifecycle_for`, so an agent process that happens to inherit
//! `RUSSEL_AGENT_URL` cannot recurse back into itself over HTTP.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use russel_core::api::{AgentLifecycleResponse, AgentStatusResponse};
use russel_core::config::RuntimeKind;
use russel_ctrl::metadata::load_service_disk_record_from;
use russel_ctrl::microvm::MicrovmRunner;
use russel_ctrl::runtime::in_process_lifecycle;

/// Error carrying an HTTP status for route mapping.
#[derive(Debug)]
pub struct LifecycleError {
    pub status: StatusCode,
    pub message: String,
}

impl LifecycleError {
    pub fn bad_request(message: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.to_string(),
        }
    }
}

/// A service resolved from on-disk metadata: runtime kind + full record.
struct ResolvedService {
    runtime: RuntimeKind,
    record: russel_ctrl::metadata::ServiceDiskRecord,
}

fn metadata_path(data_root: &Path, service_id: &str) -> PathBuf {
    data_root.join(service_id).join("metadata.json")
}

/// Resolve a service's runtime kind from on-disk metadata.
///
/// Mirrors `russel_ctrl::metadata::resolve_lifecycle_runtime`: valid metadata
/// missing the `runtime` key falls back to `Microvm` with a warning. Missing
/// or unparseable metadata → 404 (the control plane treats that as unknown).
fn resolve_service(data_root: &Path, service_id: &str) -> Result<ResolvedService, LifecycleError> {
    let path = metadata_path(data_root, service_id);
    if !path.is_file() {
        return Err(LifecycleError::not_found(format!(
            "service {service_id} not found (no metadata at {})",
            path.display()
        )));
    }
    let record = load_service_disk_record_from(&path).ok_or_else(|| {
        LifecycleError::internal(format!("failed to parse metadata at {}", path.display()))
    })?;
    let runtime = match record.runtime {
        Some(rt) => rt,
        None => {
            tracing::warn!(
                service_id,
                "metadata.json missing 'runtime' key — defaulting to microvm"
            );
            RuntimeKind::Microvm
        }
    };
    Ok(ResolvedService { runtime, record })
}

/// `POST /agent/v1/stop/{service_id}` — graceful workload stop.
pub async fn stop(
    data_root: &Path,
    service_id: &str,
) -> Result<AgentLifecycleResponse, LifecycleError> {
    MicrovmRunner::validate_service_id(service_id).map_err(LifecycleError::bad_request)?;
    let resolved = resolve_service(data_root, service_id)?;
    let runtime = resolved.runtime;
    in_process_lifecycle(runtime)
        .stop(service_id)
        .await
        .map_err(|e| LifecycleError::internal(format!("failed to stop {service_id}: {e}")))?;
    Ok(AgentLifecycleResponse {
        service_id: service_id.to_string(),
        operation: "stop".into(),
        status: "stopped".into(),
        message: format!("stopped {runtime} {service_id}"),
        runtime: Some(runtime),
    })
}

/// `POST /agent/v1/destroy/{service_id}` — stop + remove all workload state.
pub async fn destroy(
    data_root: &Path,
    service_id: &str,
) -> Result<AgentLifecycleResponse, LifecycleError> {
    MicrovmRunner::validate_service_id(service_id).map_err(LifecycleError::bad_request)?;
    let resolved = resolve_service(data_root, service_id)?;
    let runtime = resolved.runtime;
    in_process_lifecycle(runtime)
        .destroy(service_id)
        .await
        .map_err(|e| LifecycleError::internal(format!("failed to destroy {service_id}: {e}")))?;
    Ok(AgentLifecycleResponse {
        service_id: service_id.to_string(),
        operation: "destroy".into(),
        status: "destroyed".into(),
        message: format!("destroyed {runtime} {service_id}"),
        runtime: Some(runtime),
    })
}

/// `GET /agent/v1/status/{service_id}` — local workload liveness.
pub async fn status(
    data_root: &Path,
    service_id: &str,
) -> Result<AgentStatusResponse, LifecycleError> {
    MicrovmRunner::validate_service_id(service_id).map_err(LifecycleError::bad_request)?;
    let resolved = resolve_service(data_root, service_id)?;
    let runtime = resolved.runtime;
    let (running, uptime) = probe_runtime(&resolved.record, runtime, service_id).await;
    Ok(build_status(
        service_id,
        runtime,
        running,
        uptime,
        resolved.record.host_port,
        resolved.record.guest_port,
    ))
}

/// Pure response shaping (unit-tested without probing the host).
pub fn build_status(
    service_id: &str,
    runtime: RuntimeKind,
    running: bool,
    uptime_seconds: u64,
    host_port: Option<u16>,
    guest_port: Option<u16>,
) -> AgentStatusResponse {
    let state = if running { "running" } else { "stopped" };
    AgentStatusResponse {
        service_id: service_id.to_string(),
        status: state.into(),
        vm_state: state.into(),
        uptime_seconds,
        runtime: Some(runtime),
        host_port,
        guest_port,
    }
}

/// Best-effort workload liveness. Probe failures report `stopped` — a
/// missing runtime binary or unreadable procfs must not fail the request.
async fn probe_runtime(
    record: &russel_ctrl::metadata::ServiceDiskRecord,
    runtime: RuntimeKind,
    service_id: &str,
) -> (bool, u64) {
    match runtime {
        RuntimeKind::Microvm => match record.vm_pid.filter(|pid| process_alive(*pid)) {
            Some(pid) => (true, uptime_seconds_from_pid(pid).unwrap_or(0)),
            None => (false, 0),
        },
        RuntimeKind::Container => match container_state(service_id).await {
            Some(state) if state == "running" => {
                (true, container_pid_uptime(service_id).await.unwrap_or(0))
            }
            _ => (false, 0),
        },
    }
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Wall-clock seconds a process has been running, from `/proc`:
/// `starttime` (field 22 of `stat`) is in USER_HZ ticks since boot; `btime`
/// (in `/proc/stat`) is the boot instant in epoch seconds. The kernel reports
/// proc timestamps in USER_HZ regardless of `CONFIG_HZ` (see proc_pid_stat(5)).
fn uptime_seconds_from_pid(pid: u32) -> Option<u64> {
    const USER_HZ: u64 = 100;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field may contain spaces and ends with ')'; starttime is the
    // 20th whitespace token after it.
    let after_comm = stat.rsplit_once(')')?.1;
    let starttime: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let btime = boot_time_epoch_secs()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(btime.saturating_add(starttime / USER_HZ)))
}

fn boot_time_epoch_secs() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())
}

/// Podman container state for the canonical `russel-{service_id}` name.
///
/// `--filter name=` takes a regex; anchoring prevents `russel-api` matching
/// `russel-api-extra`. Generation-scoped names (`russel-{id}_g…`) are not
/// probed yet — documented follow-up.
async fn container_state(service_id: &str) -> Option<String> {
    let output = tokio::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^russel-{service_id}$"),
            "--format",
            "{{.State}}",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let state = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if state.is_empty() { None } else { Some(state) }
}

/// Uptime of the podman container's main process (`{{.State.Pid}}`).
async fn container_pid_uptime(service_id: &str) -> Option<u64> {
    let output = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.State.Pid}}",
            &format!("russel-{service_id}"),
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if pid == 0 {
        return None;
    }
    uptime_seconds_from_pid(pid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "russel-agent-lifecycle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_metadata(root: &Path, service_id: &str, content: &str) {
        let dir = root.join(service_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("metadata.json")).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn stop_unknown_service_is_not_found() {
        let root = temp_root();
        let err = stop(&root, "missing-svc").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert!(err.message.contains("missing-svc"));
    }

    #[tokio::test]
    async fn stop_invalid_service_id_is_bad_request() {
        let root = temp_root();
        let err = stop(&root, "../etc/passwd").await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn destroy_unknown_service_is_not_found() {
        let root = temp_root();
        let err = destroy(&root, "ghost").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_reports_stopped_for_container_without_running_container() {
        let root = temp_root();
        write_metadata(
            &root,
            "svc-c",
            r#"{"service_id":"svc-c","runtime":"container","host_port":8080,"guest_port":3000}"#,
        );
        let resp = status(&root, "svc-c").await.unwrap();
        assert_eq!(resp.status, "stopped");
        assert_eq!(resp.runtime, Some(RuntimeKind::Container));
        assert_eq!(resp.host_port, Some(8080));
        assert_eq!(resp.guest_port, Some(3000));
    }

    #[tokio::test]
    async fn status_reports_running_for_live_pid() {
        let root = temp_root();
        // Own pid is always alive in /proc → deterministic "running" probe.
        let pid = std::process::id();
        write_metadata(
            &root,
            "svc-m",
            &format!(
                r#"{{"service_id":"svc-m","runtime":"microvm","vm_pid":{pid},"host_port":8080}}"#
            ),
        );
        let resp = status(&root, "svc-m").await.unwrap();
        assert_eq!(resp.status, "running");
        assert_eq!(resp.vm_state, "running");
        assert_eq!(resp.runtime, Some(RuntimeKind::Microvm));
        assert!(resp.uptime_seconds > 0);
    }

    #[tokio::test]
    async fn status_unknown_service_is_not_found() {
        let root = temp_root();
        let err = status(&root, "nope").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn build_status_shapes_running_and_stopped() {
        let running = build_status("svc-a", RuntimeKind::Container, true, 42, Some(8080), None);
        assert_eq!(running.status, "running");
        assert_eq!(running.uptime_seconds, 42);
        let stopped = build_status("svc-a", RuntimeKind::Microvm, false, 0, None, None);
        assert_eq!(stopped.status, "stopped");
        assert_eq!(stopped.vm_state, "stopped");
        assert_eq!(stopped.uptime_seconds, 0);
    }

    #[test]
    fn uptime_from_proc_parses_own_pid() {
        let uptime = uptime_seconds_from_pid(std::process::id());
        assert!(uptime.is_some_and(|s| s > 0));
    }
}
