//! `russel-agent` — worker process for horizontal scaling Phase 1 (#213).
//!
//! Serves local capacity heartbeats and reserves `/agent/v1/*` lifecycle routes
//! (501 until #214 wires ctrl → agent RPC). Default single-node installs keep
//! using monolithic `russel-ctrl`; this binary is opt-in.

#[cfg(not(target_os = "linux"))]
compile_error!("russel-agent requires Linux (same host constraints as russel-ctrl)");

mod auth;
mod capacity;
mod lifecycle;
mod node_id;
mod routes;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::middleware;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::auth::{
    AGENT_TOKEN_ENV, check_token_min_length, configured_agent_token, require_bearer,
};
use crate::capacity::data_root_from_env;
use crate::node_id::resolve_node_id;
use crate::routes::{AgentState, agent_router};

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
                .unwrap_or_else(|_| EnvFilter::new("info,russel_agent=debug")),
        )
        .init();

    let node_id = resolve_node_id();
    let data_root = data_root_from_env();
    let bind_addr = std::env::var("RUSSEL_AGENT_ADDR").unwrap_or_else(|_| "127.0.0.1:7946".into());

    // Auth: require token on non-loopback; optional on loopback with warning.
    let token = match configured_agent_token() {
        Some(t) => {
            if let Err(msg) = check_token_min_length(&t) {
                anyhow::bail!("{msg}");
            }
            Some(t)
        }
        None => None,
    };

    let listener = TcpListener::bind(&bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let is_loopback = match local_addr.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => ip.is_loopback(),
    };

    if token.is_none() {
        if is_loopback {
            tracing::warn!(
                "dev mode: no {AGENT_TOKEN_ENV} / RUSSEL_API_TOKEN — any local user can \
                 call the agent API; set a token of at least {} chars for production",
                auth::MIN_TOKEN_LEN
            );
        } else {
            anyhow::bail!(
                "{AGENT_TOKEN_ENV} (or RUSSEL_API_TOKEN) must be set when binding to \
                 non-loopback address '{bind_addr}' (bound {local_addr}); use at least \
                 {} characters (e.g. openssl rand -hex 32)",
                auth::MIN_TOKEN_LEN
            );
        }
    } else {
        info!("agent token configured — requiring Bearer auth on all routes");
    }

    let state = Arc::new(AgentState::new(node_id.clone(), data_root.clone()));
    let expected_token = token.clone();
    let app: Router = agent_router(state).layer(middleware::from_fn(move |req, next| {
        let expected = expected_token.clone();
        async move { require_bearer(req, next, expected).await }
    }));

    info!(
        %node_id,
        %local_addr,
        data_root = %data_root.display(),
        "russel-agent listening (heartbeat + stop/destroy/status live; deploy RPC TBD)"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            tracing::warn!("failed to install Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl+C — shutting down agent"),
        _ = terminate => info!("received SIGTERM — shutting down agent"),
    }
}
