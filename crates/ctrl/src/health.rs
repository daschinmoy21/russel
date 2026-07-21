//! Periodic health monitoring and optional auto-restart of deployed services.

use std::collections::HashMap;
use std::time::Duration;

use russel_core::api::DeployRequest;

use crate::deploy::DeployPipeline;
use crate::metadata::load_metadata_from_disk;
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

/// Spawn the background health loop. Failures never take down the control plane.
pub fn spawn_health_loop(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let checker = HealthChecker;
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
                let addr = format!("127.0.0.1:{port}");
                if checker.check(&addr).await {
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
                    try_auto_restart(&state, &id).await;
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
        env: Default::default(),
        podman_args: vec![],
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
}

fn load_source_from_metadata(service_id: &str) -> Option<SourceMeta> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let repo_url = value.get("repo_url")?.as_str()?.to_string();
    let config_path = value
        .get("config_path")
        .and_then(|v| v.as_str())
        .unwrap_or("Russelfile.toml")
        .to_string();
    Some(SourceMeta {
        repo_url,
        config_path,
        host_port: value
            .get("host_port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16),
        guest_port: value
            .get("guest_port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16),
        runtime: value
            .get("runtime")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok()),
    })
}

#[cfg(test)]
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
}
