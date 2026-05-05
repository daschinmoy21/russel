use axum::{Json, Router, extract::State, routing::get, routing::post, routing::delete, extract::Path};
use russel_core::api::{DeployRequest, DeployResponse, LogsResponse, StatusResponse, VmsResponse};

use crate::{deploy::DeployPipeline, microvm::MicrovmRunner, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/status", get(status))
        .route("/logs", get(logs))
        .route("/vms", get(vms_list))
        .route("/vm/:id/stop", post(vm_stop))
        .route("/vm/:id", delete(vm_destroy))
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

async fn vms_list() -> Json<VmsResponse> {
    let runner = MicrovmRunner;
    let vms = runner.list().await.unwrap_or_default();
    Json(VmsResponse { vms })
}

async fn vm_stop(Path(id): Path<String>) -> Json<String> {
    let runner = MicrovmRunner;
    match runner.stop(&id).await {
        Ok(_) => Json(format!("stopped {}", id)),
        Err(e) => Json(format!("error: {}", e)),
    }
}

async fn vm_destroy(Path(id): Path<String>) -> Json<String> {
    let runner = MicrovmRunner;
    match runner.destroy(&id).await {
        Ok(_) => Json(format!("destroyed {}", id)),
        Err(e) => Json(format!("error: {}", e)),
    }
}
