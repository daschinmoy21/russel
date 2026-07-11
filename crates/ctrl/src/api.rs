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
            let json = serde_json::to_string(&msg).map_err(|e| {
                tracing::error!(error = %e, "failed to serialize deploy event");
                std::io::Error::new(std::io::ErrorKind::Other, e)
            })?;
            Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("{}\n", json)))
        });

    axum::response::Response::builder()
        .header("Content-Type", "application/x-ndjson")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| {
            tracing::error!(error = %e, "failed to build NDJSON stream response");
            e
        })
        .unwrap_or_else(|e| {
            axum::response::Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from(e.to_string()))
                .expect("500 response")
        })
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

async fn vms_list() -> Result<Json<VmsResponse>, (axum::http::StatusCode, String)> {
    tracing::debug!("GET /vms");
    let runner = MicrovmRunner::new();
    let vms = runner.list().await.map_err(|e| {
        tracing::error!(error = %e, "failed to list VMs");
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("failed to list VMs: {}", e))
    })?;
    tracing::debug!(count = vms.len(), "GET /vms -> {} VMs", vms.len());
    Ok(Json(VmsResponse { vms }))
}

async fn vm_stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<String>, (axum::http::StatusCode, String)> {
    tracing::info!(vm_id = %id, "POST /vm/{}/stop", id);
    state.set_status_if_matches(&id, "stopping", "pending");
    let (vm_child, aux_processes) = state.take_processes_if_matches(&id);
    if let Some(mut child) = vm_child {
        let _ = child.kill().await;
    }
    for mut child in aux_processes {
        let _ = child.kill().await;
    }
    let runner = MicrovmRunner::new();
    match runner.stop(&id).await {
        Ok(_) => {
            tracing::info!(vm_id = %id, "stopped microvm");
            state.set_status_if_matches(&id, "stopped", "none");
            Ok(Json(format!("stopped {}", id)))
        }
        Err(e) => {
            tracing::error!(vm_id = %id, error = %e, "failed to stop microvm");
            state.set_status_if_matches(&id, "failed", "failed");
            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

async fn vm_destroy(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<String>, (axum::http::StatusCode, String)> {
    tracing::info!(vm_id = %id, "DELETE /vm/{}", id);
    state.set_status_if_matches(&id, "destroying", "pending");
    let (vm_child, aux_processes) = state.take_processes_if_matches(&id);
    if let Some(mut child) = vm_child {
        let _ = child.kill().await;
    }
    for mut child in aux_processes {
        let _ = child.kill().await;
    }
    let runner = MicrovmRunner::new();
    match runner.destroy(&id).await {
        Ok(_) => {
            tracing::info!(vm_id = %id, "destroyed microvm");
            state.set_status_if_matches(&id, "destroyed", "none");
            Ok(Json(format!("destroyed {}", id)))
        }
        Err(e) => {
            tracing::error!(vm_id = %id, error = %e, "failed to destroy microvm");
            state.set_status_if_matches(&id, "failed", "failed");
            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}
