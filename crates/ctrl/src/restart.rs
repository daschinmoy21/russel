//! `restart = "unless-stopped"` for microVMs (#467).
//!
//! Containers get this from Podman's restart policy. A microVM is relaunched
//! by ctrl from its recorded generation (same build and config, nothing is
//! rebuilt) when its VM exits without a stop or destroy, and at ctrl start
//! when reconcile finds it not running. Crash loops back off (1s, 2s, 4s, …
//! 30s) and read `failed` in status meanwhile. An operator `stop` is recorded
//! in metadata so neither path brings the service back.

use std::time::Duration;

use serde_json::Value;

use crate::deploy::{FailedLaunch, RecordedMicrovm};
use crate::network::{MicrovmNet, MicrovmNetMode, lookup_subnet};
use crate::state::AppState;

/// Metadata flag set by an operator stop; the next deploy's metadata drops it.
const USER_STOPPED_KEY: &str = "user_stopped";

fn read_metadata(service_id: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(crate::metadata::metadata_path(service_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// A microVM whose recorded deploy asks for `restart = "unless-stopped"` and
/// that no one stopped on purpose.
fn wants_restart(meta: &Value) -> bool {
    meta.get("runtime").and_then(Value::as_str) == Some("microvm")
        && meta
            .pointer("/desired_state/restart")
            .and_then(Value::as_str)
            == Some("unless-stopped")
        && meta.get(USER_STOPPED_KEY).and_then(Value::as_bool) != Some(true)
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

/// After startup reconcile: relaunch microVMs with the policy that it found
/// not running (host reboot, VM died while ctrl was down).
pub fn relaunch_at_startup(state: &AppState) {
    for service_id in state.list_services() {
        if service_wants_restart(&service_id) {
            schedule(state, &service_id);
        }
    }
}

fn schedule(state: &AppState, service_id: &str) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Some((generation, delay)) = state.note_crash_for_restart(service_id) else {
        return;
    };
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
            "restart = unless-stopped: relaunching microVM after backoff"
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
                tracing::info!(service_id, "microVM relaunched");
                return;
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(service_id, error = %error, "microVM relaunch failed");
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
    let runner = crate::microvm::shared_runner();
    let meta = read_metadata(service_id)
        .ok_or_else(|| anyhow::anyhow!("no readable metadata.json for {service_id}"))?;
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
}
