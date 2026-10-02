//! Periodic health monitoring and optional auto-restart of deployed services.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use russel_core::api::{DeployRequest, ServiceStatus, VmState};

use crate::api::deploy_semaphore;
use crate::deploy::DeployPipeline;
use crate::metadata::load_metadata_from_disk;
use crate::network::publish_bind_addr;
use crate::state::AppState;

/// Check whether the app at a socket address or HTTP URL accepts connections.
/// A bare connect is not enough: a port forwarder accepts for a dead app (#462).
pub async fn check(addr: &str) -> bool {
    let addr = addr
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(addr);
    crate::network::app_accepts(addr).await
}

/// Whether auto-restart is enabled (`RUSSEL_HEALTH_RESTART=1`).
fn restart_enabled() -> bool {
    russel_core::env_util::env_bool(std::env::var("RUSSEL_HEALTH_RESTART").ok().as_deref())
        .unwrap_or(false)
}

fn probe_interval() -> Duration {
    let secs = std::env::var("RUSSEL_HEALTH_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
        .max(5);
    Duration::from_secs(secs)
}

/// Build a TCP connect target for the health probe.
///
/// Wildcards map to loopback; IPv6 literals are bracketed so
/// `TcpStream::connect` can parse them.
fn health_probe_addr(bind: &str, port: u16) -> String {
    let connect_host = if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1"
    } else {
        bind
    };
    if let Ok(ip) = connect_host.parse::<std::net::IpAddr>() {
        return std::net::SocketAddr::new(ip, port).to_string();
    }
    format!("{connect_host}:{port}")
}

/// Resolve a TCP probe target for a running service.
///
/// Prefer the published host port (on the publish bind). When that is absent,
/// fall back to the microVM guest address (`vm_ip:guest_port`) so ingress-only
/// workloads without a durable host_port in status are still covered.
fn resolve_probe_target(
    host_port: Option<u16>,
    guest_port: Option<u16>,
    vm_ip: Option<&str>,
    bind: &str,
) -> Option<String> {
    if let Some(port) = host_port.filter(|p| *p > 0) {
        return Some(health_probe_addr(bind, port));
    }
    let guest = guest_port.filter(|p| *p > 0)?;
    let ip = vm_ip.filter(|s| !s.is_empty())?;
    Some(health_probe_addr(ip, guest))
}

/// Pure decision for how `try_auto_restart` should treat a deploy response.
#[derive(Debug, PartialEq, Eq)]
enum RestartApply {
    /// Deploy status `deployed` — restart succeeded.
    Succeeded,
    /// Status `rolled_back` — prior generation is live; do not `mark_failed`.
    FailedKeepPrior,
    /// Hard failure (`failed`, `error`, empty, unknown) — call `mark_failed`.
    FailedMark,
}

fn apply_restart_response(status: &str) -> RestartApply {
    match status {
        "deployed" => RestartApply::Succeeded,
        "rolled_back" => RestartApply::FailedKeepPrior,
        _ => RestartApply::FailedMark,
    }
}

/// Count one more failed probe of `id` at `generation` and return the
/// consecutive count. A probe of a different generation starts over: failures
/// of a replaced deployment never count toward its successor (#556).
fn record_failure(failures: &mut HashMap<String, (u64, u32)>, id: &str, generation: u64) -> u32 {
    let entry = failures.entry(id.to_string()).or_insert((generation, 0));
    if entry.0 != generation {
        *entry = (generation, 0);
    }
    entry.1 += 1;
    entry.1
}

/// Spawn the background health loop. Failures never take down the control plane.
pub fn spawn_health_loop(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut failures: HashMap<String, (u64, u32)> = HashMap::new();
        // Track services we already warned about (no probe target) to avoid spam;
        // re-warn after the set is cleared when the service leaves deployed/running.
        let mut no_probe_warned: HashSet<String> = HashSet::new();
        let mut interval = tokio::time::interval(probe_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!(
            interval_secs = probe_interval().as_secs(),
            restart = restart_enabled(),
            "health loop started"
        );
        loop {
            interval.tick().await;
            let ids = state.list_services();
            let mut checks = tokio::task::JoinSet::new();
            for id in ids {
                // Capture the generation before reading ports so a result can
                // only apply to the deployment that was sampled (#556).
                let Some(generation) = state.deployed_generation(&id) else {
                    failures.remove(&id);
                    no_probe_warned.remove(&id);
                    continue;
                };
                let Some(status) = state.status(&id) else {
                    continue;
                };
                if status.status != ServiceStatus::Deployed.as_str()
                    || status.vm_state != VmState::Running.as_str()
                    || state.deployed_generation(&id) != Some(generation)
                {
                    failures.remove(&id);
                    no_probe_warned.remove(&id);
                    continue;
                }
                let disk = load_metadata_from_disk(&id);
                let host_port = status
                    .host_port
                    .or_else(|| disk.as_ref().and_then(|m| m.host_port));
                let guest_port = status
                    .guest_port
                    .or_else(|| disk.as_ref().and_then(|m| m.guest_port));
                let vm_ip = disk.as_ref().and_then(|m| m.vm_ip.as_deref());
                let bind = publish_bind_addr();
                let Some(addr) = resolve_probe_target(host_port, guest_port, vm_ip, &bind) else {
                    if no_probe_warned.insert(id.clone()) {
                        tracing::warn!(
                            service_id = %id,
                            "health: skipped (no probe target — no host_port and no vm_ip:guest_port)"
                        );
                    }
                    continue;
                };
                no_probe_warned.remove(&id);
                // Mirror network::wait_for_host_port: probe 127.0.0.1 when the
                // publish bind is wildcard / loopback; otherwise probe the bind IP.
                // Bracket IPv6 literals so `TcpStream::connect` parses correctly.
                checks.spawn(async move {
                    let reachable = check(&addr).await;
                    (id, addr, generation, reachable)
                });
            }

            while let Some(result) = checks.join_next().await {
                let (id, addr, generation, reachable) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::error!(%error, "health probe task failed");
                        continue;
                    }
                };
                if reachable {
                    failures.remove(&id);
                    continue;
                }
                let count = record_failure(&mut failures, &id, generation);
                tracing::warn!(
                    service_id = %id,
                    %addr,
                    consecutive_failures = count,
                    "health check failed"
                );
                if count < 3 {
                    continue;
                }
                failures.remove(&id);
                // Apply only to the generation that was probed. A deploy,
                // stop or destroy that ran during the probe changed it, and
                // its claim on the service must stay intact.
                if !state.mark_failed_if_generation(
                    &id,
                    generation,
                    format!("health check failed for {addr} (3 consecutive probes)"),
                ) {
                    tracing::info!(
                        service_id = %id,
                        "health: dropped stale probe result (service changed during probe)"
                    );
                    continue;
                }
                if restart_enabled() {
                    let restart_state = state.clone();
                    let restart_id = id.clone();
                    // Single spawn: retry when the deploy semaphore is briefly full
                    // so a concurrent deploy wave does not permanently drop the intent.
                    tokio::spawn(async move {
                        for attempt in 0u32..6 {
                            match try_auto_restart(&restart_state, &restart_id, generation).await {
                                RestartOutcome::SkippedSemaphore if attempt + 1 < 6 => {
                                    tokio::time::sleep(Duration::from_secs(10)).await;
                                }
                                _ => break,
                            }
                        }
                    });
                }
            }
        }
    });
}

/// Outcome of a health-driven redeploy attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartOutcome {
    /// Redeploy finished with status `deployed`.
    Succeeded,
    /// Redeploy ran but did not land in `deployed`.
    Failed,
    /// No recorded source metadata to redeploy from.
    SkippedNoSource,
    /// Deploy concurrency limit hit; caller may retry later.
    SkippedSemaphore,
    /// The service is no longer in the `failed` state this restart was queued
    /// for (stopped, destroyed or redeployed meanwhile), or the restart was
    /// rejected before it claimed the service.
    SkippedStale,
}

async fn try_auto_restart(
    state: &AppState,
    service_id: &str,
    failed_generation: u64,
) -> RestartOutcome {
    // The restart was queued after a failed probe and may retry for a minute.
    // Only act while the service is still failed, so a stop, destroy or a
    // newer deploy in between is never overridden. This check is only an
    // early exit: the pipeline claims the service with
    // `mark_building_if_failed`, which makes the same check atomically.
    if state.status(service_id).map(|s| s.status)
        != Some(ServiceStatus::Failed.as_str().to_string())
    {
        tracing::info!(
            service_id,
            "health restart skipped: service is no longer failed"
        );
        return RestartOutcome::SkippedStale;
    }
    let Some(request) = load_restart_request(service_id).await else {
        tracing::warn!(
            service_id,
            "health restart skipped — no repo_url in metadata (redeploy once to record source)"
        );
        return RestartOutcome::SkippedNoSource;
    };

    // F-07: acquire the deploy semaphore so a health-driven restart does not
    // overwhelm the control plane when many services fail at once.
    let Ok(_permit) = deploy_semaphore().try_acquire() else {
        tracing::warn!(
            service_id,
            "health restart skipped — deploy semaphore exhausted"
        );
        return RestartOutcome::SkippedSemaphore;
    };

    let _guard = state.begin_deploy();

    tracing::info!(
        service_id,
        repo = %crate::git::redact_repo_url(&request.repo_url),
        "health restart: redeploying from recorded source"
    );
    let pipeline = DeployPipeline::new(state.clone()).for_health_restart(failed_generation);
    let claimed = pipeline.claimed_service_id();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    // Drain events so the channel never fills.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let response = pipeline.deploy(request, tx).await;
    if claimed.get().is_none() {
        // Rejected before the claim: the service changed since the failed
        // probe, or the source could not be loaded. Nothing ran, so leave the
        // service as it is now.
        tracing::info!(
            service_id,
            message = %response.message,
            "health restart did not run"
        );
        return RestartOutcome::SkippedStale;
    }

    match apply_restart_response(&response.status) {
        RestartApply::Succeeded => {
            tracing::info!(
                service_id,
                status = %response.status,
                message = %response.message,
                "health restart succeeded"
            );
            RestartOutcome::Succeeded
        }
        RestartApply::FailedKeepPrior => {
            // Prior generation is live again — do not mark_failed.
            tracing::error!(
                service_id,
                status = %response.status,
                message = %response.message,
                "health restart failed"
            );
            RestartOutcome::Failed
        }
        RestartApply::FailedMark => {
            tracing::error!(
                service_id,
                status = %response.status,
                message = %response.message,
                "health restart failed"
            );
            // The pipeline marked the service failed while it held the claim.
            // Marking it again here could override a stop that came after.
            RestartOutcome::Failed
        }
    }
}

/// Redeploy request from the last deploy's recorded source, or `None` when
/// metadata is missing, unparseable, has an invalid recorded port, or has no
/// `repo_url`.
async fn load_restart_request(service_id: &str) -> Option<DeployRequest> {
    let path = crate::metadata::metadata_path(service_id);
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    match crate::deployments::DesiredStateSnapshot::from_metadata_with_legacy(&value) {
        Ok(source) => source.redeploy_request(service_id),
        Err(e) => {
            tracing::warn!(service_id, error = %e, "health restart: unusable recorded source");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_rejects_closed_port() {
        // Port 1 is typically closed / privileged.
        assert!(!check("127.0.0.1:1").await);
    }

    #[tokio::test]
    async fn load_restart_request_missing_is_none() {
        assert!(
            load_restart_request("definitely-missing-svc-xyz")
                .await
                .is_none()
        );
    }

    #[test]
    fn failure_counts_start_over_for_a_new_generation() {
        let mut failures = HashMap::new();
        assert_eq!(record_failure(&mut failures, "api", 7), 1);
        assert_eq!(record_failure(&mut failures, "api", 7), 2);
        // Redeployed between probes: the old generation's failures are gone.
        assert_eq!(record_failure(&mut failures, "api", 8), 1);
        assert_eq!(record_failure(&mut failures, "api", 8), 2);
        assert_eq!(record_failure(&mut failures, "other", 8), 1);
        assert_eq!(record_failure(&mut failures, "api", 8), 3);
    }

    #[test]
    fn health_probe_addr_brackets_ipv6() {
        assert_eq!(health_probe_addr("2001:db8::1", 8080), "[2001:db8::1]:8080");
        assert_eq!(health_probe_addr("10.0.0.5", 3000), "10.0.0.5:3000");
        assert_eq!(health_probe_addr("::", 7878), "127.0.0.1:7878");
        assert_eq!(health_probe_addr("0.0.0.0", 7878), "127.0.0.1:7878");
    }

    #[test]
    fn resolve_probe_target_prefers_host_port() {
        let addr = resolve_probe_target(Some(8080), Some(3000), Some("10.0.5.2"), "0.0.0.0");
        assert_eq!(addr.as_deref(), Some("127.0.0.1:8080"));
    }

    #[test]
    fn resolve_probe_target_falls_back_to_guest() {
        let addr = resolve_probe_target(None, Some(3000), Some("10.0.5.2"), "0.0.0.0");
        assert_eq!(addr.as_deref(), Some("10.0.5.2:3000"));
    }

    #[test]
    fn resolve_probe_target_skips_zero_and_missing() {
        assert!(resolve_probe_target(Some(0), Some(0), Some("10.0.5.2"), "0.0.0.0").is_none());
        assert!(resolve_probe_target(None, Some(3000), None, "0.0.0.0").is_none());
        assert!(resolve_probe_target(None, None, Some("10.0.5.2"), "0.0.0.0").is_none());
        assert!(resolve_probe_target(None, Some(3000), Some(""), "0.0.0.0").is_none());
    }

    #[test]
    fn apply_restart_response_succeeded() {
        assert_eq!(apply_restart_response("deployed"), RestartApply::Succeeded);
    }

    #[test]
    fn apply_restart_response_rolled_back_keeps_prior() {
        assert_eq!(
            apply_restart_response("rolled_back"),
            RestartApply::FailedKeepPrior
        );
    }

    #[test]
    fn apply_restart_response_failed_mark_for_hard_failures() {
        for status in [
            "failed",
            "error",
            "",
            "building",
            "pending",
            "unknown",
            "DEPLOYED", // case-sensitive: only exact "deployed" succeeds
            "rolled_back ",
            " rolled_back",
        ] {
            assert_eq!(
                apply_restart_response(status),
                RestartApply::FailedMark,
                "status={status:?} should mark failed"
            );
        }
    }

    static HEALTH_RESTART_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_health_restart_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = HEALTH_RESTART_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("RUSSEL_HEALTH_RESTART").ok();
        // SAFETY: exclusive lock held for the duration of the mutation + assertion.
        unsafe {
            match value {
                Some(v) => std::env::set_var("RUSSEL_HEALTH_RESTART", v),
                None => std::env::remove_var("RUSSEL_HEALTH_RESTART"),
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match previous {
                Some(v) => std::env::set_var("RUSSEL_HEALTH_RESTART", v),
                None => std::env::remove_var("RUSSEL_HEALTH_RESTART"),
            }
        }
        match result {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn restart_enabled_parses_truthy_and_falsy() {
        with_health_restart_env(None, || {
            assert!(!restart_enabled());
        });
        for truthy in ["1", "true", "yes", "on", " On "] {
            with_health_restart_env(Some(truthy), || {
                assert!(restart_enabled(), "expected truthy for {truthy:?}");
            });
        }
        for falsy in ["0", "false", "no", "off", "disabled", "maybe"] {
            with_health_restart_env(Some(falsy), || {
                assert!(!restart_enabled(), "expected falsy for {falsy:?}");
            });
        }
    }
}
