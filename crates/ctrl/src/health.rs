//! Periodic health monitoring and optional auto-restart of deployed services.

use std::collections::HashMap;
use std::time::Duration;

use russel_core::api::DeployRequest;

use crate::api::deploy_semaphore;
use crate::deploy::DeployPipeline;
use crate::metadata::load_metadata_from_disk;
use crate::network::publish_bind_addr;
use crate::state::AppState;

/// Health checker — TCP reachability probe + background loop.
#[derive(Debug, Default)]
pub struct HealthChecker;

impl HealthChecker {
    /// Check if a service is reachable via TCP at the given socket address.
    /// Accepts a raw `host:port` string (e.g. `"10.0.5.2:3000"`) or a full
    /// HTTP URL — the scheme and path are stripped automatically.
    /// Returns true if the connection succeeds within a short timeout.
    pub async fn check(&self, addr: &str) -> bool {
        // Strip scheme and path so callers can pass http://... URLs directly
        let addr = addr
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(addr);
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::net::TcpStream::connect(addr).await.is_ok()
        })
        .await
        .unwrap_or(false)
    }
}

/// Whether auto-restart is enabled (`RUSSEL_HEALTH_RESTART=1`).
fn restart_enabled() -> bool {
    matches!(
        std::env::var("RUSSEL_HEALTH_RESTART")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn probe_interval() -> Duration {
    let secs = std::env::var("RUSSEL_HEALTH_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64)
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

/// Spawn the background health loop. Failures never take down the control plane.
pub fn spawn_health_loop(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut failures: HashMap<String, u32> = HashMap::new();
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
                let Some(status) = state.status(&id) else {
                    continue;
                };
                if status.status != "deployed" || status.vm_state != "running" {
                    failures.remove(&id);
                    continue;
                }
                let host_port = status
                    .host_port
                    .or_else(|| load_metadata_from_disk(&id).and_then(|m| m.host_port));
                let Some(port) = host_port else {
                    continue;
                };
                let bind = publish_bind_addr();
                // Mirror network::wait_for_host_port: probe 127.0.0.1 when the
                // publish bind is wildcard / loopback; otherwise probe the bind IP.
                // Bracket IPv6 literals so `TcpStream::connect` parses correctly.
                let addr = health_probe_addr(&bind, port);
                checks.spawn(async move {
                    let reachable = HealthChecker.check(&addr).await;
                    (id, addr, port, reachable)
                });
            }

            while let Some(result) = checks.join_next().await {
                let (id, addr, port, reachable) = match result {
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
                let count = failures.entry(id.clone()).or_insert(0);
                *count += 1;
                tracing::warn!(
                    service_id = %id,
                    port,
                    consecutive_failures = *count,
                    "health check failed"
                );
                if *count < 3 {
                    continue;
                }
                failures.remove(&id);
                state.mark_failed(
                    &id,
                    format!("health check failed for {addr} (3 consecutive probes)"),
                );
                if restart_enabled() {
                    let restart_state = state.clone();
                    let restart_id = id.clone();
                    let log_id = restart_id.clone();
                    tokio::spawn(async move {
                        let result = tokio::spawn(async move {
                            try_auto_restart(&restart_state, &restart_id).await;
                        })
                        .await;
                        if let Err(error) = result {
                            if error.is_panic() {
                                tracing::error!(
                                    service_id = %log_id,
                                    "health auto-restart panicked"
                                );
                            } else {
                                tracing::error!(
                                    service_id = %log_id,
                                    %error,
                                    "health auto-restart task was cancelled"
                                );
                            }
                        }
                    });
                }
            }
        }
    });
}

async fn try_auto_restart(state: &AppState, service_id: &str) {
    let Some(meta) = load_source_from_metadata(service_id) else {
        tracing::warn!(
            service_id,
            "health restart skipped — no repo_url in metadata (redeploy once to record source)"
        );
        return;
    };

    // F-07: acquire the deploy semaphore so a health-driven restart does not
    // overwhelm the control plane when many services fail at once.
    let _permit = match deploy_semaphore().try_acquire() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!(
                service_id,
                "health restart skipped — deploy semaphore exhausted"
            );
            return;
        }
    };

    let _guard = state.begin_deploy();

    tracing::info!(
        service_id,
        repo = %meta.repo_url,
        "health restart: redeploying from recorded source"
    );
    let pipeline = DeployPipeline::new(state.clone());
    let request = DeployRequest {
        repo_url: meta.repo_url,
        config_path: meta.config_path,
        vm_id: Some(service_id.to_string()),
        port: meta.host_port.map(|host| russel_core::api::PortMapping {
            host,
            guest: meta.guest_port.unwrap_or(3000),
        }),
        runtime: meta.runtime,
        env: meta.env,
        podman_args: meta.podman_args,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    // Drain events so the channel never fills.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = pipeline.deploy(request, tx).await;
}

struct SourceMeta {
    repo_url: String,
    config_path: String,
    host_port: Option<u16>,
    guest_port: Option<u16>,
    runtime: Option<russel_core::config::RuntimeKind>,
    env: HashMap<String, String>,
    podman_args: Vec<String>,
}

fn load_source_from_metadata(service_id: &str) -> Option<SourceMeta> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Legacy top-level fields (fallback).
    let top_repo_url = value
        .get("repo_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let top_config_path = value
        .get("config_path")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let top_host_port: Option<u16> = value
        .get("host_port")
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    let top_guest_port: Option<u16> = value
        .get("guest_port")
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    let top_runtime: Option<russel_core::config::RuntimeKind> = value
        .get("runtime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());

    // desired_state block (SHARED CONTRACT). All fields are optional;
    // when absent we fall back to legacy top-level fields.
    let desired = value.get("desired_state").and_then(|v| v.as_object());
    let ds_repo_url = desired
        .and_then(|d| d.get("repo_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ds_config_path = desired
        .and_then(|d| d.get("config_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ds_runtime = desired
        .and_then(|d| d.get("runtime"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let ds_env: HashMap<String, String> = desired
        .and_then(|d| d.get("env"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let ds_podman_args: Vec<String> = desired
        .and_then(|d| d.get("podman_args"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let ds_port = desired.and_then(|d| d.get("port").and_then(|p| p.as_object()));
    let ds_host_port: Option<u16> = ds_port
        .and_then(|p| p.get("host"))
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    let ds_guest_port: Option<u16> = ds_port
        .and_then(|p| p.get("guest"))
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok());

    // Precedence: desired_state > legacy top-level.
    let repo_url = ds_repo_url.or(top_repo_url)?;

    Some(SourceMeta {
        repo_url,
        config_path: ds_config_path
            .or(top_config_path)
            .unwrap_or_else(|| "Russelfile.toml".into()),
        host_port: ds_host_port.or(top_host_port),
        guest_port: ds_guest_port.or(top_guest_port),
        runtime: ds_runtime.or(top_runtime),
        env: ds_env,
        podman_args: ds_podman_args,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_rejects_closed_port() {
        let h = HealthChecker;
        // Port 1 is typically closed / privileged.
        assert!(!h.check("127.0.0.1:1").await);
    }

    #[test]
    fn load_source_missing_is_none() {
        assert!(load_source_from_metadata("definitely-missing-svc-xyz").is_none());
    }

    #[test]
    fn health_probe_addr_brackets_ipv6() {
        assert_eq!(health_probe_addr("2001:db8::1", 8080), "[2001:db8::1]:8080");
        assert_eq!(health_probe_addr("10.0.0.5", 3000), "10.0.0.5:3000");
        assert_eq!(health_probe_addr("::", 7878), "127.0.0.1:7878");
        assert_eq!(health_probe_addr("0.0.0.0", 7878), "127.0.0.1:7878");
    }
}
