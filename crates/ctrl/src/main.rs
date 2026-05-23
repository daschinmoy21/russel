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

use anyhow::Result;
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::state::AppState;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer()
            .with_timer(tracing_subscriber::fmt::time::uptime())
            .with_target(false))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,russel_ctrl=debug")))
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
    if let Ok(vms) = runner.list().await {
        let mut tasks = Vec::new();
        for vm_id in vms {
            let runner = runner.clone();
            tasks.push(tokio::spawn(async move {
                info!(vm_id = %vm_id, "destroying microVM during shutdown");
                let _ = runner.destroy(&vm_id).await;
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }
    cleanup_stale_resources().await;
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = signal(SignalKind::terminate())
            .expect("failed to install SIGTERM signal handler");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down control plane...");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down control plane...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C signal handler");
        tracing::info!("received SIGINT, shutting down control plane...");
    }
}

/// Flush iptables NAT rules and tap interfaces left over from previous russel-ctrl runs.
async fn cleanup_stale_resources() {
    use tokio::process::Command;

    // 1. Flush iptables rules
    let _ = Command::new("iptables").args(["-t", "nat", "-F", "OUTPUT"]).output().await;
    let _ = Command::new("iptables").args(["-t", "nat", "-F", "POSTROUTING"]).output().await;
    let _ = Command::new("iptables").args(["-F", "FORWARD"]).output().await;
    let _ = Command::new("sysctl").args(["-w", "net.ipv4.conf.all.route_localnet=0"]).output().await;

    // 2. Remove stale tap interfaces
    let output = Command::new("ip").args(["-o", "link", "show"]).output().await;
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if line.contains("vm-") {
                if let Some(name) = line.split_whitespace().nth(1) {
                    let name = name.trim_matches(':');
                    let _ = Command::new("ip").args(["link", "delete", name]).output().await;
                }
            }
        }
    }

    tracing::info!("flushed stale iptables NAT/FORWARD rules and tap interfaces");
}
