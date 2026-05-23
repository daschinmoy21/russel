use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use russel_core::api::{DeployRequest, DeployEvent, LogsResponse, StatusResponse, VmsResponse};
use tokio_stream::StreamExt;

use crate::{deploy::DeployPipeline, microvm::MicrovmRunner, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/status", get(status))
        .route("/logs", get(logs))
        .route("/vms", get(vms_list))
        .route("/vm/{id}/stop", post(vm_stop))
        .route("/vm/{id}", delete(vm_destroy))
        .with_state(state)
}

async fn deploy(
    State(state): State<AppState>,
    Json(request): Json<DeployRequest>,
) -> axum::response::Response {
    tracing::info!(
        repo = %request.repo_url,
        vm_id = ?request.vm_id,
        port = ?request.port.as_ref().map(|p| format!("{}:{}", p.host, p.guest)),
        "POST /deploy"
    );
    
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    tokio::spawn(async move {
        let pipeline = DeployPipeline::new(state);
        let response = pipeline.deploy(request, tx.clone()).await;
        let status = response.status.clone();
        let elapsed_ms = response.elapsed_ms;
        tracing::info!(
            status = %status,
            elapsed_ms = elapsed_ms,
            "POST /deploy -> {}", status
        );
        let _ = tx.send(DeployEvent::Complete(response)).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|msg| {
            let json = serde_json::to_string(&msg).unwrap();
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(format!("{}\n", json)))
        });

    axum::response::Response::builder()
        .header("Content-Type", "application/x-ndjson")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let s = state.status();
    tracing::debug!(service_id = %s.service_id, status = %s.status, "GET /status");
    Json(s)
}

async fn logs(State(state): State<AppState>) -> Json<LogsResponse> {
    tracing::debug!("GET /logs");
    Json(state.logs())
}

async fn vms_list() -> Json<VmsResponse> {
    tracing::debug!("GET /vms");
    let runner = MicrovmRunner::new();
    let vms = runner.list().await.unwrap_or_default();
    tracing::debug!(count = vms.len(), "GET /vms -> {} VMs", vms.len());
    Json(VmsResponse { vms })
}

async fn vm_stop(Path(id): Path<String>) -> Json<String> {
    tracing::info!(vm_id = %id, "POST /vm/{}/stop", id);
    let runner = MicrovmRunner::new();
    match runner.stop(&id).await {
        Ok(_) => {
            tracing::info!(vm_id = %id, "stopped microvm");
            Json(format!("stopped {}", id))
        }
        Err(e) => {
            tracing::error!(vm_id = %id, error = %e, "failed to stop microvm");
            Json(format!("error: {}", e))
        }
    }
}

async fn vm_destroy(Path(id): Path<String>) -> Json<String> {
    tracing::info!(vm_id = %id, "DELETE /vm/{}", id);
    let runner = MicrovmRunner::new();
    match runner.destroy(&id).await {
        Ok(_) => {
            tracing::info!(vm_id = %id, "destroyed microvm");
            Json(format!("destroyed {}", id))
        }
        Err(e) => {
            tracing::error!(vm_id = %id, error = %e, "failed to destroy microvm");
            Json(format!("error: {}", e))
        }
    }
}
