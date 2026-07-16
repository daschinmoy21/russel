// ponytail: scaffold modules have unused items; allowed deliberately for the MVP.
// Remove these allows once HealthChecker, DatabaseProvisioner, and TraefikClient
// are integrated into the deploy pipeline.
#![allow(dead_code, clippy::type_complexity, clippy::too_many_arguments)]

mod api;
mod build;
mod database;
mod deploy;
mod git;
mod health;
mod microvm;
mod network;
mod state;
mod traefik;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "russel-ctrl requires Linux — it depends on cloud-hypervisor, iptables, socat, and TAP networking"
);

use anyhow::Result;
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::state::AppState;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_timer(tracing_subscriber::fmt::time::uptime())
                .with_target(false),
        )
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,russel_ctrl=debug")),
        )
        .init();

    // Remove only Russel-owned stale TAP interfaces from previous sessions.
    // Never flush host-global iptables chains (Docker/VPN/admin rules).
    cleanup_stale_resources().await;

    // Keep a clone so we can detach workload children after Axum drops the
    // router state. Child handles are spawned with kill_on_drop(true); without
    // an explicit detach, dropping AppState would SIGKILL every VM on exit.
    let state = AppState::default();
    let app: Router = api::router(state.clone());
    let bind_addr = std::env::var("RUSSEL_CTRL_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into());
    let listener = TcpListener::bind(&bind_addr).await?;

    info!(
        "russel control plane listening on {}",
        listener.local_addr()?
    );
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    // Wait for in-flight deploy tasks to complete before detaching.
    // Deploy tasks may still be creating VMs; if we detach now, they'd
    // register handles after detach and get SIGKILLed on drop.
    state.wait_for_deploys().await;

    // Workload teardown is an explicit admin action (`russel destroy` /
    // DELETE /vm/{id}). Controller restart must not take the fleet down.
    let detached = state.detach_all_processes();
    info!(
        detached,
        "control plane stopped; left workloads running \
         (use `russel vms` / `russel destroy <id>` to manage them)"
    );

    serve_result?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let sigterm = signal(SignalKind::terminate());

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down control plane...");
            }
            _ = async {
                if let Ok(mut sigterm) = sigterm {
                    sigterm.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::info!("received SIGTERM, shutting down control plane...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received SIGINT, shutting down control plane...");
    }
}

/// Clean up stale Russel-owned resources from previous controller sessions.
///
/// Intentionally does **not** flush host-global iptables chains (`OUTPUT`,
/// `POSTROUTING`, `FORWARD`). Those belong to Docker, VPN, and the host admin.
/// When Russel needs firewall rules it must install dedicated `RUSSEL_*` chains
/// and only remove those.
async fn cleanup_stale_resources() {
    use tokio::process::Command;

    // Remove stale TAP interfaces owned by Russel (`rsl-<8 hex chars>`).
    // See `network::subnet_for` for the naming scheme.
    match Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let Some(raw) = line.split_whitespace().nth(1) else {
                    continue;
                };
                // `ip -o link show` names look like `rsl-a1b2c3d4@NONE:` — strip
                // trailing colon and optional `@peer` suffix.
                let base = raw
                    .trim_end_matches(':')
                    .split('@')
                    .next()
                    .unwrap_or(raw);
                if !is_russel_tap(base) {
                    continue;
                }
                match Command::new("ip")
                    .args(["link", "delete", base])
                    .output()
                    .await
                {
                    Ok(del) if del.status.success() => {
                        tracing::info!(tap = base, "deleted stale Russel TAP interface");
                    }
                    Ok(del) => {
                        tracing::warn!(
                            tap = base,
                            stderr = %String::from_utf8_lossy(&del.stderr).trim(),
                            "failed to delete stale TAP interface"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(tap = base, error = %e, "failed to delete stale TAP interface");
                    }
                }
            }
        }
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "ip link show failed with status {}",
                out.status,
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list network interfaces during cleanup");
        }
    }

    tracing::info!("stale resource cleanup finished (Russel TAPs only; host iptables untouched)");
}

/// True for current Russel TAP names: `rsl-` + exactly 8 lowercase hex digits.
fn is_russel_tap(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("rsl-") else {
        return false;
    };
    suffix.len() == 8
        && suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::is_russel_tap;

    #[test]
    fn russel_tap_names_match_hash_scheme() {
        assert!(is_russel_tap("rsl-a1b2c3d4"));
        assert!(is_russel_tap("rsl-00000000"));
        assert!(!is_russel_tap("rsl-short"));
        assert!(!is_russel_tap("rsl-a1b2c3d4e")); // too long
        assert!(!is_russel_tap("vm-api"));
        assert!(!is_russel_tap("docker0"));
        assert!(!is_russel_tap("rsl-A1B2C3D4")); // uppercase not lowercase hex
        assert!(!is_russel_tap("rsl-gggggggg")); // not hex
    }
}
