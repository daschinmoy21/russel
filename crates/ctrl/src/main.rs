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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let state = AppState::default();
    let app: Router = api::router(state);
    let bind_addr = std::env::var("RUSSEL_CTRL_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into());
    let listener = TcpListener::bind(&bind_addr).await?;

    info!(
        "russel control plane listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;

    Ok(())
}
