use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use russel_core::api::{DeployEvent, DeployRequest, LogsResponse, StatusResponse, VmsResponse};
use tokio::process::Child;
use tokio_stream::StreamExt;

use crate::{
    deploy::DeployPipeline,
    microvm::MicrovmRunner,
    network::release_subnet,
    state::{AppState, LifecycleClaim},
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/vm/{service_id}/status", get(vm_status))
        .route("/vm/{service_id}/logs", get(vm_logs))
        .route("/status", get(status_all))
        .route("/logs", get(logs_all))
        .route("/vms", get(vms_list))
        .route("/vm/{service_id}/stop", post(vm_stop))
        .route("/vm/{service_id}", delete(vm_destroy))
        .with_state(state)
}

async fn deploy(
    State(state): State<AppState>,
    Json(request): Json<DeployRequest>,
) -> axum::response::Response {
    let service_id = request.vm_id.clone().unwrap_or_else(|| "api".to_string());

    tracing::info!(
        repo = %request.repo_url,
        service_id = %service_id,
        port = ?request.port.as_ref().map(|p| format!("{}:{}", p.host, p.guest)),
        "POST /deploy"
    );

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let deploy_tx = tx.clone();
    let monitor_state = state.clone();
    let sid = service_id.clone();
    let sid2 = service_id.clone();

    let deploy_handle = tokio::spawn(async move {
        let pipeline = DeployPipeline::new(state);
        let response = pipeline.deploy(request, deploy_tx.clone()).await;
        let status = response.status.clone();
        let elapsed_ms = response.elapsed_ms;
        tracing::info!(
            service_id = %sid,
            status = %status,
            elapsed_ms = elapsed_ms,
            "POST /deploy -> {}", status
        );
        let _ = deploy_tx
            .send(DeployEvent::Complete(Box::new(response)))
            .await;
    });

    tokio::spawn(async move {
        if let Err(e) = deploy_handle.await
            && e.is_panic()
        {
            let panic = e.into_panic();
            let detail = panic
                .downcast_ref::<&'static str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "deploy task panicked".to_string());
            tracing::error!(
                service_id = %sid2,
                panic = %detail,
                "deploy task panicked"
            );
            monitor_state.mark_failed(&sid2, detail);
            let _ = tx
                .send(DeployEvent::Error("deploy task failed".to_string()))
                .await;
        }
        // Cancelled join errors (runtime shutdown) are intentionally dropped
        // so the client falls back to the generic closed-connection message.
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|msg| {
        let json = serde_json::to_string(&msg).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize deploy event");
            std::io::Error::other(e)
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

async fn vm_status(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    tracing::debug!(service_id = %service_id, "GET /vm/{}/status", service_id);
    state.status(&service_id).map(Json).ok_or((
        StatusCode::NOT_FOUND,
        format!("service {} not found", service_id),
    ))
}

async fn vm_logs(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<LogsResponse>, (StatusCode, String)> {
    tracing::debug!(service_id = %service_id, "GET /vm/{}/logs", service_id);
    state.logs(&service_id).map(Json).ok_or((
        StatusCode::NOT_FOUND,
        format!("service {} not found", service_id),
    ))
}

// Flat endpoints kept for CLI compatibility
async fn status_all(
    State(state): State<AppState>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let ids = state.list_services();
    match ids.len() {
        0 => Err((StatusCode::NOT_FOUND, "no services".to_string())),
        1 => {
            let sid = &ids[0];
            state
                .status(sid)
                .map(Json)
                .ok_or((StatusCode::NOT_FOUND, format!("service {} not found", sid)))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "multiple services exist; specify a service_id".to_string(),
        )),
    }
}

async fn logs_all(
    State(state): State<AppState>,
) -> Result<Json<LogsResponse>, (StatusCode, String)> {
    let ids = state.list_services();
    match ids.len() {
        0 => Err((StatusCode::NOT_FOUND, "no services".to_string())),
        1 => {
            let sid = &ids[0];
            state
                .logs(sid)
                .map(Json)
                .ok_or((StatusCode::NOT_FOUND, format!("service {} not found", sid)))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "multiple services exist; specify a service_id".to_string(),
        )),
    }
}

async fn vms_list(State(state): State<AppState>) -> Json<VmsResponse> {
    // Start with in-memory inventory
    let mut vms = state.list_services();
    let mut seen: std::collections::HashSet<String> = vms.iter().cloned().collect();

    // Augment with VMs discovered on disk (to handle restarts/rebuilds).
    // Rehydrate the state so lifecycle endpoints can find them.
    for base in &["/var/lib/russel", "/var/lib/microvms"] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type()
                    && file_type.is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    // Exclude .bak backup directories
                    if name.ends_with(".bak") {
                        continue;
                    }
                    // De-duplicate across both disk roots and in-memory services
                    if seen.insert(name.to_string()) {
                        state.ensure_service(name);
                        vms.push(name.to_string());
                    }
                }
            }
        }
    }

    tracing::debug!(
        count = vms.len(),
        "GET /vms -> {} VMs (from disk+memory)",
        vms.len()
    );
    Json(VmsResponse { vms })
}

async fn vm_stop(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<String>, (StatusCode, String)> {
    tracing::info!(service_id = %service_id, "POST /vm/{}/stop", service_id);

    let (runner, vm_child, aux_processes) =
        claim_lifecycle_operation(&state, &service_id, "stopping")?;
    let result = runner.stop(&service_id).await;
    reap_children(vm_child, aux_processes).await;

    match result {
        Ok(_) => {
            tracing::info!(service_id = %service_id, "stopped microvm");
            state.set_status(&service_id, "stopped", "none");
            Ok(Json(format!("stopped {}", service_id)))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, error = %e, "failed to stop microvm");
            state.set_status(&service_id, "failed", "failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

async fn vm_destroy(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<String>, (StatusCode, String)> {
    tracing::info!(service_id = %service_id, "DELETE /vm/{}", service_id);

    let (runner, vm_child, aux_processes) =
        claim_lifecycle_operation(&state, &service_id, "destroying")?;
    let result = runner.destroy(&service_id).await;
    reap_children(vm_child, aux_processes).await;

    match result {
        Ok(_) => {
            release_subnet(&service_id);
            tracing::info!(service_id = %service_id, "destroyed microvm");
            state.remove_service(&service_id);
            Ok(Json(format!("destroyed {}", service_id)))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, error = %e, "failed to destroy microvm");
            state.set_status(&service_id, "failed", "failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

/// Claim a service for a lifecycle operation without killing its processes.
///
/// Cloud Hypervisor must receive the graceful shutdown request while its child
/// is still alive. The handles are returned so the caller can reap them after
/// `MicrovmRunner::stop`/`destroy` has completed. Maps `NotFound` -> 404 and
/// `Busy` -> 409.
fn claim_lifecycle_operation(
    state: &AppState,
    service_id: &str,
    target_status: &str,
) -> Result<(MicrovmRunner, Option<Child>, Vec<Child>), (StatusCode, String)> {
    let (vm_child, aux_processes) =
        match state.begin_lifecycle_operation(service_id, target_status, "pending") {
            LifecycleClaim::Claimed(vm, aux) => (vm, aux),
            LifecycleClaim::NotFound => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("service {} not found", service_id),
                ));
            }
            LifecycleClaim::Busy => {
                return Err((
                    StatusCode::CONFLICT,
                    format!("service {} is already in a lifecycle operation", service_id),
                ));
            }
        };

    Ok((MicrovmRunner::new(), vm_child, aux_processes))
}

async fn reap_children(vm_child: Option<Child>, aux_processes: Vec<Child>) {
    if let Some(child) = vm_child {
        reap_child(child).await;
    }
    for child in aux_processes {
        reap_child(child).await;
    }
}

async fn reap_child(mut child: Child) {
    let wait = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    if !matches!(wait, Ok(Ok(_))) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
