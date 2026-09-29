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
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use russel_core::api::{AgentLifecycleResponse, AgentStatusResponse};
use russel_core::config::RuntimeKind;
use russel_ctrl::container::{ContainerRunner, is_trusted_container_name};
use russel_ctrl::metadata::load_service_disk_record_from;
use russel_ctrl::microvm::cloud_hypervisor_cmdline_matches;
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
/// A path that exists but cannot be read (directory, permissions) is 500,
/// not missing. Assuming microVM would take the wrong teardown path.
fn resolve_service(data_root: &Path, service_id: &str) -> Result<ResolvedService, LifecycleError> {
    let path = metadata_path(data_root, service_id);
    if let Err(e) = std::fs::read_to_string(&path) {
        return Err(if e.kind() == std::io::ErrorKind::NotFound {
            LifecycleError::not_found(format!(
                "service {service_id} not found (no metadata at {})",
                path.display()
            ))
        } else {
            LifecycleError::internal(format!("failed to read metadata for {service_id}: {e}"))
        });
    }
    // Corrupt / partially-written metadata is an unknown service to callers
    // (same contract as missing file), not a 500.
    let record = load_service_disk_record_from(&path).ok_or_else(|| {
        LifecycleError::not_found(format!(
            "service {service_id} not found (invalid metadata at {})",
            path.display()
        ))
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
    run_lifecycle_op(data_root, service_id, "stop", "stopped").await
}

/// `POST /agent/v1/destroy/{service_id}` — stop + remove all workload state.
pub async fn destroy(
    data_root: &Path,
    service_id: &str,
) -> Result<AgentLifecycleResponse, LifecycleError> {
    run_lifecycle_op(data_root, service_id, "destroy", "destroyed").await
}

/// Shared stop/destroy scaffolding: validate, resolve runtime, run the
/// in-process lifecycle op, and shape the response. `operation` is the RPC
/// verb/route segment; `status` is the response status and message verb.
async fn run_lifecycle_op(
    data_root: &Path,
    service_id: &str,
    operation: &str,
    status: &str,
) -> Result<AgentLifecycleResponse, LifecycleError> {
    russel_core::ids::validate_service_id(service_id).map_err(LifecycleError::bad_request)?;
    let resolved = resolve_service(data_root, service_id)?;
    let runtime = resolved.runtime;
    let result = match operation {
        "stop" => in_process_lifecycle(runtime).stop(service_id).await,
        "destroy" => in_process_lifecycle(runtime).destroy(service_id).await,
        other => {
            return Err(LifecycleError::internal(format!(
                "unknown lifecycle operation {other}"
            )));
        }
    };
    result.map_err(|e| {
        LifecycleError::internal(format!("failed to {operation} {service_id}: {e}"))
    })?;
    Ok(AgentLifecycleResponse {
        service_id: service_id.to_string(),
        operation: operation.to_string(),
        status: status.to_string(),
        message: format!("{status} {runtime} {service_id}"),
        runtime: Some(runtime),
    })
}

/// `GET /agent/v1/status/{service_id}` — local workload liveness.
pub async fn status(
    data_root: &Path,
    service_id: &str,
) -> Result<AgentStatusResponse, LifecycleError> {
    russel_core::ids::validate_service_id(service_id).map_err(LifecycleError::bad_request)?;
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
        RuntimeKind::Microvm => match record
            .vm_pid
            .filter(|pid| microvm_pid_is_alive(*pid, service_id))
        {
            Some(pid) => (true, uptime_seconds_from_pid(pid).unwrap_or(0)),
            None => (false, 0),
        },
        RuntimeKind::Container => {
            let target = resolve_status_container_ref(record, service_id);
            match container_state(&target).await {
                Some(state) if state.eq_ignore_ascii_case("running") => {
                    (true, container_pid_uptime(&target).await.unwrap_or(0))
                }
                _ => (false, 0),
            }
        }
    }
}

/// Prefer metadata `container_id`, then trusted `container_name` (incl. gen-scoped),
/// else canonical `russel-{service_id}`.
fn resolve_status_container_ref(
    record: &russel_ctrl::metadata::ServiceDiskRecord,
    service_id: &str,
) -> String {
    if let Some(id) = record
        .container_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return id.to_string();
    }
    if let Some(name) = record
        .container_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && is_trusted_container_name(service_id, name)
    {
        return name.to_string();
    }
    ContainerRunner::container_name(service_id)
}

/// True when `pid` is alive **and** is cloud-hypervisor for this service.
///
/// Guards against PID reuse after the original VMM exited (stale metadata).
fn microvm_pid_is_alive(pid: u32, service_id: &str) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) only checks existence/permissions; pid is a u32 process id.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&bytes).replace('\0', " ");
    cloud_hypervisor_cmdline_matches(&cmdline, service_id)
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

/// Bound for each `podman` subprocess in lifecycle status probes.
///
/// Mirrors the timeout+`kill_on_drop` pattern in [`crate::capacity`]: a wedged
/// `podman` must not hang `/agent/v1/status/{id}` indefinitely.
const PODMAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Run a single `podman` invocation bounded by [`PODMAN_TIMEOUT`].
///
/// `what` names the operation for diagnostics. Errors and timeouts return
/// `None`; the caller reports the workload as `stopped` (best-effort probe).
async fn podman_output(what: &str, args: &[&str]) -> Option<std::process::Output> {
    let child = tokio::process::Command::new("podman")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    match tokio::time::timeout(PODMAN_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, what, "podman subprocess wait error");
            None
        }
        Err(_) => {
            tracing::warn!(
                what,
                timeout_secs = PODMAN_TIMEOUT.as_secs(),
                "podman subprocess timed out"
            );
            None
        }
    }
}

/// Podman container state for an exact name **or** container id.
///
/// When `target` looks like a name (contains no `/` and is not a long hex id),
/// use an anchored `--filter name=` so `russel-api` does not match `russel-api-extra`.
/// Generation-scoped names (`russel-{id}_g…`) and raw container ids are probed
/// via `podman inspect` (exact key).
async fn container_state(target: &str) -> Option<String> {
    // Prefer inspect: works for id *and* exact name including gen-scoped.
    let output = podman_output(
        "inspect",
        &["inspect", "--format", "{{.State.Status}}", target],
    )
    .await?;
    if output.status.success() {
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !state.is_empty() {
            return Some(state);
        }
    }
    // Fallback: anchored name filter (canonical names only).
    let filter = format!("name=^{target}$");
    let output = podman_output(
        "ps -a",
        &[
            "ps",
            "-a",
            "--filter",
            filter.as_str(),
            "--format",
            "{{.State}}",
        ],
    )
    .await?;
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
async fn container_pid_uptime(target: &str) -> Option<u64> {
    let output = podman_output(
        "inspect pid",
        &["inspect", "--format", "{{.State.Pid}}", target],
    )
    .await?;
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
    async fn malformed_metadata_is_not_found() {
        let root = temp_root();
        write_metadata(&root, "bad-json", "{not valid json");
        let err = status(&root, "bad-json").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert!(err.message.contains("invalid metadata"));
        let err = stop(&root, "bad-json").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn directory_metadata_is_internal_error() {
        let root = temp_root();
        let dir = root.join("svc-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join("metadata.json")).unwrap();
        let err = stop(&root, "svc-dir").await.unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message.contains("failed to read metadata"));
        let err = destroy(&root, "svc-dir").await.unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
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
    async fn status_reports_stopped_for_live_but_non_ch_pid() {
        // Own pid is alive in /proc but is not cloud-hypervisor → must not
        // report running (PID-reuse / stale metadata guard).
        let root = temp_root();
        let pid = std::process::id();
        write_metadata(
            &root,
            "svc-m",
            &format!(
                r#"{{"service_id":"svc-m","runtime":"microvm","vm_pid":{pid},"host_port":8080}}"#
            ),
        );
        let resp = status(&root, "svc-m").await.unwrap();
        assert_eq!(resp.status, "stopped");
        assert_eq!(resp.vm_state, "stopped");
        assert_eq!(resp.runtime, Some(RuntimeKind::Microvm));
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
        // Fresh processes can report 0s; parse success is the contract.
        assert!(uptime.is_some());
    }

    #[test]
    fn resolve_status_prefers_container_id_then_trusted_name() {
        let mut rec = russel_ctrl::metadata::ServiceDiskRecord {
            container_id: Some("abc123deadbeef".into()),
            container_name: Some("russel-api_gdeadbeef".into()),
            ..Default::default()
        };
        assert_eq!(resolve_status_container_ref(&rec, "api"), "abc123deadbeef");
        rec.container_id = None;
        assert_eq!(
            resolve_status_container_ref(&rec, "api"),
            "russel-api_gdeadbeef"
        );
        rec.container_name = Some("evil-name".into());
        assert_eq!(resolve_status_container_ref(&rec, "api"), "russel-api");
        rec.container_name = None;
        assert_eq!(resolve_status_container_ref(&rec, "api"), "russel-api");
    }
}
