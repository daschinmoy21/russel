//! Bringing services back under `service.restart` (#467, #450). The default
//! is `"unless-stopped"`; `"no"` leaves a service down.
//!
//! Containers get restart-on-exit from Podman's restart policy. A microVM is
//! relaunched by ctrl from its recorded generation (same build and config,
//! nothing is rebuilt) when its VM exits without a stop or destroy. Crash
//! loops back off (1s, 2s, 4s, … 30s) and read `failed` in status meanwhile.
//!
//! At ctrl start (after a host reboot, for example) every service that
//! reconcile found down and whose policy restarts it is started again,
//! unless an operator stopped it: a container with `podman start` of the
//! container it last ran, a microVM from its recorded generation. Neither
//! rebuilds or adds a deployment. An operator `stop` is recorded in metadata
//! so neither path brings the service back; the next deploy clears it.

use std::time::Duration;

use russel_core::volumes::RestartPolicy;
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

/// The recorded `service.restart`. Omitted, including on services deployed
/// before it had a default, means `unless-stopped` (#450).
fn policy(meta: &Value) -> RestartPolicy {
    RestartPolicy::of(
        meta.pointer("/desired_state/restart")
            .and_then(Value::as_str),
    )
}

/// A microVM whose recorded policy restarts it and that no one stopped on
/// purpose. Podman restarts containers itself.
fn wants_restart(meta: &Value) -> bool {
    meta.get("runtime").and_then(Value::as_str) == Some("microvm")
        && policy(meta) == RestartPolicy::UnlessStopped
        && !user_stopped(meta)
}

/// A service ctrl starts again at ctrl start (#450): one with a recorded
/// runtime, a policy that restarts it, and no operator stop.
fn wants_start_at_boot(meta: &Value) -> bool {
    matches!(
        meta.get("runtime").and_then(Value::as_str),
        Some("container" | "microvm")
    ) && policy(meta) == RestartPolicy::UnlessStopped
        && !user_stopped(meta)
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
        let Some(meta) = read_metadata(&service_id) else {
            continue;
        };
        let running = state
            .status(&service_id)
            .is_some_and(|s| s.status == "deployed");
        if running && is_container(&meta) {
            // Adopted as running: Podman still holds the policy it was
            // created with, which predates the default for older services.
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(async move {
                    sync_restart_policy(&service_id, &meta).await;
                });
            }
        } else if wants_start_at_boot(&meta) {
            schedule_after(state, &service_id, Some(Duration::ZERO));
        }
    }
}

/// What Podman calls the policy: `no` and an empty name both mean none.
fn podman_policy_name(name: &str) -> &str {
    if name.is_empty() {
        RestartPolicy::NO
    } else {
        name
    }
}

/// Give an existing container the recorded `service.restart` (#450).
/// Containers created before `unless-stopped` became the default have no
/// policy, so Podman would leave them down after a crash. Podman releases
/// without `podman update --restart` keep the old policy until the next
/// deploy, and the log says so.
async fn sync_restart_policy(service_id: &str, meta: &Value) {
    sync_restart_policy_with(service_id, meta, podman_command).await;
}

async fn sync_restart_policy_with<F, Fut>(service_id: &str, meta: &Value, mut command: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = tokio::process::Command>,
{
    // The metadata snapshot belongs to this exact container. Its name can
    // be reused by a deploy while this background migration awaits Podman.
    // Never fall back to that name, even when the old container is gone.
    let Some(container_id) = meta
        .get("container_id")
        .and_then(Value::as_str)
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        tracing::warn!(
            service_id,
            "no full container ID recorded; `russel update` applies the restart policy"
        );
        return;
    };
    let want = match policy(meta) {
        RestartPolicy::UnlessStopped => RestartPolicy::UNLESS_STOPPED,
        RestartPolicy::No => RestartPolicy::NO,
    };
    let mut inspect = command().await;
    let Ok(out) = inspect
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.RestartPolicy.Name}}",
            container_id,
        ])
        .output()
        .await
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let have = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if podman_policy_name(&have) == want {
        return;
    }
    let mut update = command().await;
    match update
        .args(["update", "--restart", want, container_id])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            tracing::info!(service_id, from = %have, to = want, "container restart policy updated");
        }
        Ok(out) => tracing::warn!(
            service_id,
            want,
            error = %String::from_utf8_lossy(&out.stderr).trim(),
            "cannot change the container restart policy; `russel update` applies it"
        ),
        Err(e) => tracing::warn!(service_id, error = %e, "podman update failed to run"),
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
    sync_restart_policy(service_id, meta).await;

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
    let container_id = meta
        .get("container_id")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string();
    if outcome != ReadyOutcome::Ready {
        let tail = log_tail(&container_log_path(service_id), LOG_TAIL_LINES);
        let error = not_ready_error(
            &outcome,
            host_port,
            guest_port,
            CONTAINER_READY_TIMEOUT,
            &tail,
        );
        // It crashed, and its restart policy has Podman bring it back: keep
        // watching, so it reads `deployed` once it stays up (#450).
        if matches!(outcome, ReadyOutcome::Died(_)) && policy(meta) == RestartPolicy::UnlessStopped
        {
            mark_running(service_id);
            state.adopt_recovering_container(
                service_id,
                &container_id,
                host_port,
                guest_port,
                &format!("START CRASHED, Podman is restarting it: {error}"),
            );
            return Ok(());
        }
        anyhow::bail!("{error}");
    }

    mark_running(service_id);
    state.adopt_running_container(service_id, &container_id, host_port, guest_port, None);
    Ok(())
}

fn mark_running(service_id: &str) {
    if let Err(e) = crate::metadata::set_container_running(service_id, true) {
        tracing::warn!(service_id, error = %e, "could not record container_running=true");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn microvm_crash_restart_follows_the_policy_and_operator_stop() {
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

        let omitted = json!({ "runtime": "microvm", "desired_state": {} });
        assert!(wants_restart(&omitted), "unless-stopped is the default");

        let opted_out = json!({ "runtime": "microvm", "desired_state": { "restart": "no" } });
        assert!(!wants_restart(&opted_out));
    }

    #[test]
    fn boot_start_follows_the_policy_and_operator_stop() {
        for runtime in ["container", "microvm"] {
            let meta = json!({ "runtime": runtime, "desired_state": {} });
            assert!(
                wants_start_at_boot(&meta),
                "{runtime} with restart omitted (the default)"
            );

            let mut stopped = meta.clone();
            stopped[USER_STOPPED_KEY] = json!(true);
            assert!(
                !wants_start_at_boot(&stopped),
                "{runtime} stopped by the operator"
            );
        }
        assert!(!wants_start_at_boot(&json!({})), "no recorded runtime");

        for runtime in ["container", "microvm"] {
            let opted_out = json!({ "runtime": runtime, "desired_state": { "restart": "no" } });
            assert!(
                !wants_start_at_boot(&opted_out),
                "{runtime} with restart = no"
            );
        }

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

    #[test]
    fn podman_reports_no_policy_as_no() {
        assert_eq!(podman_policy_name(""), "no");
        assert_eq!(podman_policy_name("no"), "no");
        assert_eq!(podman_policy_name("unless-stopped"), "unless-stopped");
    }

    struct PolicyFixture {
        dir: tempfile::TempDir,
        container_id: String,
    }

    impl PolicyFixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("podman");
            std::fs::write(
                &script,
                r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> commands
target=$4
case "$1" in
inspect)
    if [ "$target" = "$OLD_CONTAINER_ID" ]; then
        [ -e old-present ] || exit 1
        have=$(cat old-policy)
    elif [ "$target" = russel-policy-test ]; then
        if [ -e old-present ]; then have=$(cat old-policy); else have=$(cat new-policy); fi
    else
        exit 1
    fi
    if [ -e block-inspect ]; then
        touch entered
        while [ ! -e release ]; do sleep 0.01; done
    fi
    printf '%s\n' "$have"
    ;;
update)
    if [ "$target" = "$OLD_CONTAINER_ID" ]; then
        [ -e old-present ] || exit 1
        printf '%s' "$3" > old-policy
    elif [ "$target" = russel-policy-test ]; then
        if [ -e old-present ]; then printf '%s' "$3" > old-policy; else printf '%s' "$3" > new-policy; fi
    else
        exit 1
    fi
    ;;
*) exit 1 ;;
esac
"#,
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            for (file, value) in [
                ("old-present", ""),
                ("old-policy", "no"),
                ("new-policy", "no"),
            ] {
                std::fs::write(dir.path().join(file), value).unwrap();
            }
            Self {
                dir,
                container_id: "a".repeat(64),
            }
        }

        fn command(&self) -> std::future::Ready<tokio::process::Command> {
            let mut command = tokio::process::Command::new(self.dir.path().join("podman"));
            command
                .current_dir(self.dir.path())
                .env("OLD_CONTAINER_ID", &self.container_id);
            std::future::ready(command)
        }

        fn metadata(&self) -> Value {
            json!({ "runtime": "container", "container_id": self.container_id })
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(file)).unwrap()
        }
    }

    #[tokio::test]
    async fn policy_migration_updates_only_the_recorded_container() {
        let fixture = PolicyFixture::new();
        let mut meta = fixture.metadata();
        sync_restart_policy_with("policy-test", &meta, || fixture.command()).await;
        assert_eq!(fixture.read("old-policy"), "unless-stopped");
        assert_eq!(fixture.read("new-policy"), "no");
        let commands = fixture.read("commands");
        assert!(commands.contains(&format!(
            "update --restart unless-stopped {}",
            fixture.container_id
        )));
        assert!(!commands.contains("russel-policy-test"));

        meta["desired_state"] = json!({ "restart": "no" });
        sync_restart_policy_with("policy-test", &meta, || fixture.command()).await;
        assert_eq!(fixture.read("old-policy"), "no");
    }

    #[tokio::test]
    async fn policy_migration_cannot_update_a_replacement_during_inspect() {
        let fixture = PolicyFixture::new();
        std::fs::write(fixture.dir.path().join("block-inspect"), "").unwrap();
        let migration = async {
            sync_restart_policy_with("policy-test", &fixture.metadata(), || fixture.command())
                .await;
        };
        let replace = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !fixture.dir.path().join("entered").exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "inspection never started"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Cold deploy removes the old ID and reuses the service name.
            std::fs::remove_file(fixture.dir.path().join("old-present")).unwrap();
            std::fs::write(fixture.dir.path().join("release"), "").unwrap();
        };
        tokio::time::timeout(Duration::from_secs(6), async {
            tokio::join!(migration, replace);
        })
        .await
        .unwrap();
        assert_eq!(fixture.read("new-policy"), "no");
        assert!(fixture.read("commands").contains(&format!(
            "update --restart unless-stopped {}",
            fixture.container_id
        )));
    }

    #[tokio::test]
    async fn policy_migration_cannot_update_a_replacement_before_inspect() {
        let fixture = PolicyFixture::new();
        std::fs::remove_file(fixture.dir.path().join("old-present")).unwrap();
        sync_restart_policy_with("policy-test", &fixture.metadata(), || fixture.command()).await;
        assert_eq!(fixture.read("new-policy"), "no");
        assert!(!fixture.read("commands").contains("update --restart"));
    }

    #[tokio::test]
    async fn policy_migration_refuses_missing_or_reusable_container_ids() {
        for container_id in [
            Value::Null,
            json!("russel-policy-test"),
            json!("a".repeat(12)),
            json!("z".repeat(64)),
        ] {
            let meta = json!({ "container_id": container_id });
            sync_restart_policy_with(
                "policy-test",
                &meta,
                || -> std::future::Ready<tokio::process::Command> {
                    panic!("unsafe container identity must not reach Podman")
                },
            )
            .await;
        }
    }
}
