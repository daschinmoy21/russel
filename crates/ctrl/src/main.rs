// ponytail: scaffold modules have unused items; allowed deliberately for the MVP.
// Remove these allows once HealthChecker, DatabaseProvisioner, and TraefikClient
// are integrated into the deploy pipeline.
#![allow(dead_code, clippy::type_complexity, clippy::too_many_arguments)]

mod api;
mod build;
mod container;
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

use crate::network::release_subnet;
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

    // Flush stale iptables NAT rules and tap interfaces from previous sessions.
    cleanup_stale_resources().await;

    let state = AppState::default();
    let app: Router = api::router(state);
    let bind_addr = std::env::var("RUSSEL_CTRL_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into());
    let listener = TcpListener::bind(&bind_addr).await?;

    info!(
        "russel control plane listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutting down: cleaning up all running microVMs...");
    cleanup_all_vms().await;

    Ok(())
}

async fn cleanup_all_vms() {
    let runner = crate::microvm::MicrovmRunner::new();
    match runner.list().await {
        Ok(vms) => {
            let mut tasks = Vec::new();
            for vm_id in vms {
                let runner = runner.clone();
                tasks.push(tokio::spawn(async move {
                    info!(vm_id = %vm_id, "destroying microVM during shutdown");
                    match runner.destroy(&vm_id).await {
                        Ok(()) => {
                            release_subnet(&vm_id);
                        }
                        Err(e) => {
                            tracing::warn!(vm_id = %vm_id, error = %e, "failed to destroy microVM during shutdown");
                        }
                    }
                }));
            }
            for task in tasks {
                if let Err(e) = task.await {
                    tracing::warn!(error = %e, "shutdown cleanup task panicked");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list VMs during shutdown cleanup");
        }
    }
    cleanup_stale_resources().await;
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

/// Flush iptables NAT rules and tap interfaces left over from previous russel-ctrl runs.
async fn cleanup_stale_resources() {
    use tokio::process::Command;

    // 1. Flush iptables rules
    if let Err(e) = Command::new("iptables")
        .args(["-t", "nat", "-F", "OUTPUT"])
        .output()
        .await
    {
        tracing::warn!(error = %e, "failed to flush iptables NAT OUTPUT");
    }
    if let Err(e) = Command::new("iptables")
        .args(["-t", "nat", "-F", "POSTROUTING"])
        .output()
        .await
    {
        tracing::warn!(error = %e, "failed to flush iptables NAT POSTROUTING");
    }
    if let Err(e) = Command::new("iptables")
        .args(["-F", "FORWARD"])
        .output()
        .await
    {
        tracing::warn!(error = %e, "failed to flush iptables FORWARD");
    }
    if let Err(e) = Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.all.route_localnet=0"])
        .output()
        .await
    {
        tracing::warn!(error = %e, "failed to reset route_localnet sysctl");
    }

    // 2. Remove stale tap interfaces
    match Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains("vm-")
                    && let Some(name) = line.split_whitespace().nth(1)
                {
                    let name = name.trim_matches(':');
                    if let Err(e) = Command::new("ip")
                        .args(["link", "delete", name])
                        .output()
                        .await
                    {
                        tracing::warn!(tap = name, error = %e, "failed to delete stale tap interface");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list network interfaces during cleanup");
        }
    }

    tracing::info!("flushed stale iptables NAT/FORWARD rules and tap interfaces");
}
