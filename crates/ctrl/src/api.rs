use axum::{Json, Router, extract::State, routing::get, routing::post};
use russel_core::api::{DeployRequest, DeployResponse, LogsResponse, StatusResponse};

use crate::{deploy::DeployPipeline, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/status", get(status))
        .route("/logs", get(logs))
        .with_state(state)
}

async fn deploy(
    State(state): State<AppState>,
    Json(request): Json<DeployRequest>,
) -> Json<DeployResponse> {
    let pipeline = DeployPipeline::new(state);
    Json(pipeline.deploy(request).await)
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(state.status())
}

async fn logs(State(state): State<AppState>) -> Json<LogsResponse> {
    Json(state.logs())
}
