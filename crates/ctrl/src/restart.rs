//! Bringing services back: `restart = "unless-stopped"` for microVMs (#467),
//! and every service at ctrl start (#450).
//!
//! Containers get restart-on-exit from Podman's restart policy. A microVM is
//! relaunched by ctrl from its recorded generation (same build and config,
//! nothing is rebuilt) when its VM exits without a stop or destroy. Crash
//! loops back off (1s, 2s, 4s, … 30s) and read `failed` in status meanwhile.
//!
//! At ctrl start (after a host reboot, for example) every service that
//! reconcile found down is started again, whatever its restart policy,
//! unless an operator stopped it: a container with `podman start` of the
//! container it last ran, a microVM from its recorded generation. Neither
//! rebuilds or adds a deployment. An operator `stop` is recorded in metadata
//! so neither path brings the service back; the next deploy clears it.

use std::time::Duration;

use serde_json::Value;

use crate::container::{
    CONTAINER_READY_TIMEOUT, LOG_TAIL_LINES, ReadyOutcome, container_log_path, inspect_state,
    log_tail, not_ready_error, podman_command, resolve_container_name, wait_until_ready,
};
use crate::deploy::{FailedLaunch, RecordedMicrovm};
use crate::network::{
    MicrovmNet, MicrovmNetMode, PortAllocator, TapForwarder, app_accepts, lookup_subnet,
};
use crate::state::AppState;

/// Metadata flag set by an operator stop; the next deploy's metadata drops it.
const USER_STOPPED_KEY: &str = "user_stopped";

fn read_metadata(service_id: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(crate::metadata::metadata_path(service_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// An operator stop. Container stops from before #450 only cleared
/// `container_running`, which nothing but an operator stop clears.
fn user_stopped(meta: &Value) -> bool {
    meta.get(USER_STOPPED_KEY).and_then(Value::as_bool) == Some(true)
        || (is_container(meta)
            && meta.get("container_running").and_then(Value::as_bool) == Some(false))
}

fn is_container(meta: &Value) -> bool {
    meta.get("runtime").and_then(Value::as_str) == Some("container")
}

/// A microVM whose recorded deploy asks for `restart = "unless-stopped"` and
/// that no one stopped on purpose.
fn wants_restart(meta: &Value) -> bool {
    meta.get("runtime").and_then(Value::as_str) == Some("microvm")
        && meta
            .pointer("/desired_state/restart")
            .and_then(Value::as_str)
            == Some("unless-stopped")
        && !user_stopped(meta)
}

/// A service ctrl starts again at ctrl start: one with a recorded runtime
/// that no one stopped on purpose (#450).
fn wants_start_at_boot(meta: &Value) -> bool {
    matches!(
        meta.get("runtime").and_then(Value::as_str),
        Some("container" | "microvm")
    ) && !user_stopped(meta)
}

fn service_wants_restart(service_id: &str) -> bool {
    read_metadata(service_id).is_some_and(|meta| wants_restart(&meta))
}

/// Record an operator stop so restart-on-exit and the ctrl-start pass leave
/// the service down.
pub(crate) fn note_user_stop(service_id: &str) {
    let Some(mut meta) = read_metadata(service_id) else {
        return;
    };
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    obj.insert(USER_STOPPED_KEY.into(), Value::Bool(true));
    let path = crate::metadata::metadata_path(service_id)
        .display()
        .to_string();
    if let Err(e) = crate::metadata::write_metadata(&path, &meta) {
        tracing::warn!(service_id, error = %e, "could not record operator stop in metadata");
    }
}

/// Process-supervisor hook: the workload exited on its own and the service
/// is now `failed`.
pub(crate) fn on_unexpected_exit(state: &AppState, service_id: &str) {
    if service_wants_restart(service_id) {
        schedule(state, service_id);
    }
}

/// After startup reconcile: start every service it found down (host reboot,
/// or the workload died while ctrl was not running), unless an operator
/// stopped it (#450). Services reconcile adopted as running are not
/// restartable, so `schedule` leaves them alone.
pub fn relaunch_at_startup(state: &AppState) {
    for service_id in state.list_services() {
        if read_metadata(&service_id).is_some_and(|meta| wants_start_at_boot(&meta)) {
            schedule_after(state, &service_id, Some(Duration::ZERO));
        }
    }
}

fn schedule(state: &AppState, service_id: &str) {
    schedule_after(state, service_id, None);
}

/// Start a relaunch loop. `first_delay` replaces the crash backoff for the
/// first attempt: nothing crashed at ctrl start, so it starts at once.
fn schedule_after(state: &AppState, service_id: &str, first_delay: Option<Duration>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Some((generation, backoff)) = state.note_crash_for_restart(service_id) else {
        return;
    };
    let delay = first_delay.unwrap_or(backoff);
    tokio::spawn(relaunch_loop(
        state.clone(),
        service_id.to_string(),
        generation,
        delay,
    ));
}

async fn relaunch_loop(
    state: AppState,
    service_id: String,
    mut generation: u64,
    mut delay: Duration,
) {
    loop {
        tracing::info!(
            service_id,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "relaunching service after backoff"
        );
        tokio::time::sleep(delay).await;
        if !state.claim_restart(&service_id, generation) {
            tracing::info!(
                service_id,
                "restart skipped: the service was deployed, stopped, or destroyed meanwhile"
            );
            return;
        }
        let _guard = state.begin_deploy();
        match relaunch(&state, &service_id).await {
            Ok(()) => {
                state.count_restart(&service_id);
                tracing::info!(service_id, "service relaunched");
                return;
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(service_id, error = %error, "service relaunch failed");
                state.fail_restart(&service_id, &error);
                if !service_wants_restart(&service_id) {
                    return;
                }
                let Some((g, d)) = state.note_crash_for_restart(&service_id) else {
                    return;
                };
                generation = g;
                delay = d;
            }
        }
    }
}

async fn relaunch(state: &AppState, service_id: &str) -> anyhow::Result<()> {
    let meta = read_metadata(service_id)
        .ok_or_else(|| anyhow::anyhow!("no readable metadata.json for {service_id}"))?;
    if is_container(&meta) {
        return start_container(state, service_id, &meta).await;
    }
    let runner = crate::microvm::shared_runner();
    let recorded = RecordedMicrovm::from_metadata(service_id, meta, &runner).await?;

    // Clear what the dead VM left: an adopted VM's helpers are not ctrl's
    // children, and a TAP device outlives its VM.
    if let Err(e) = runner.stop(service_id).await {
        tracing::warn!(service_id, error = %e, "cleanup before relaunch failed");
    }
    if let Some(alloc) = lookup_subnet(service_id) {
        let _ = MicrovmNet::teardown(MicrovmNetMode::for_host()?, &alloc).await;
    }

    // The relaunched VM's serial log replaces console.log. Keep the crashed
    // run's output, which says why it exited, next to it.
    let dir = crate::paths::service_dir(service_id);
    let _ = tokio::fs::rename(dir.join("console.log"), dir.join("console.log.1")).await;

    let dir = dir.display().to_string();
    recorded
        .launch(service_id, &dir, &runner, state, FailedLaunch::Keep)
        .await
}

fn port_field(meta: &Value, key: &str) -> u16 {
    meta.get(key)
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(0)
}

/// Start the container a service last ran (#450). Rootless Podman keeps
/// stopped containers across a reboot, so this is the same build, ports,
/// mounts, and secrets, with no rebuild and no new deployment.
async fn start_container(state: &AppState, service_id: &str, meta: &Value) -> anyhow::Result<()> {
    let name = resolve_container_name(service_id);
    let host_port = port_field(meta, "host_port");
    let guest_port = port_field(meta, "guest_port");
    if host_port == 0 {
        anyhow::bail!("metadata.json records no host port for {service_id}");
    }
    PortAllocator::claim_existing(service_id, host_port)?;

    let mut cmd = podman_command().await;
    let output = cmd.args(["start", &name]).output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "podman start {name} failed: {}; run `russel update {service_id}` to deploy it again",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let addr = TapForwarder::host_port_addr(host_port);
    let outcome = wait_until_ready(
        || app_accepts(&addr),
        || inspect_state(&name),
        CONTAINER_READY_TIMEOUT,
    )
    .await;
    if outcome != ReadyOutcome::Ready {
        let tail = log_tail(&container_log_path(service_id), LOG_TAIL_LINES);
        anyhow::bail!(
            "{}",
            not_ready_error(
                &outcome,
                host_port,
                guest_port,
                CONTAINER_READY_TIMEOUT,
                &tail
            )
        );
    }

    if let Err(e) = crate::metadata::set_container_running(service_id, true) {
        tracing::warn!(service_id, error = %e, "could not record container_running=true");
    }
    let container_id = meta
        .get("container_id")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string();
    state.adopt_running_container(service_id, &container_id, host_port, guest_port, None);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wants_restart_needs_microvm_policy_and_no_operator_stop() {
        let base = json!({
            "runtime": "microvm",
            "desired_state": { "restart": "unless-stopped" }
        });
        assert!(wants_restart(&base));

        let mut stopped = base.clone();
        stopped[USER_STOPPED_KEY] = json!(true);
        assert!(!wants_restart(&stopped));

        let mut container = base.clone();
        container["runtime"] = json!("container");
        assert!(!wants_restart(&container), "Podman restarts containers");

        let no_policy = json!({ "runtime": "microvm", "desired_state": {} });
        assert!(!wants_restart(&no_policy));
    }

    #[test]
    fn boot_start_takes_any_runtime_unless_an_operator_stopped_it() {
        for runtime in ["container", "microvm"] {
            let meta = json!({ "runtime": runtime, "desired_state": {} });
            assert!(
                wants_start_at_boot(&meta),
                "{runtime} without a restart policy"
            );

            let mut stopped = meta.clone();
            stopped[USER_STOPPED_KEY] = json!(true);
            assert!(
                !wants_start_at_boot(&stopped),
                "{runtime} stopped by the operator"
            );
        }
        assert!(!wants_start_at_boot(&json!({})), "no recorded runtime");

        let legacy_stop = json!({ "runtime": "container", "container_running": false });
        assert!(
            !wants_start_at_boot(&legacy_stop),
            "container stopped before #450"
        );
    }

    #[test]
    fn port_field_reads_u16_ports_only() {
        let meta = json!({ "host_port": 3100, "guest_port": 70000 });
        assert_eq!(port_field(&meta, "host_port"), 3100);
        assert_eq!(port_field(&meta, "guest_port"), 0);
        assert_eq!(port_field(&meta, "missing"), 0);
    }
}
