//! HTTP router and service lifecycle handlers.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header::CONTENT_TYPE},
    middleware,
    routing::{delete, get, post},
};
use russel_core::api::{
    DeployEvent, DeployRequest, DeploymentsResponse, LogsResponse, RollbackRequest, ServiceStatus,
    ServiceSummary, StatusResponse, VmState, VmsResponse,
};
use russel_core::config::RuntimeKind;
use russel_core::reserved::is_reserved_service_dir;
use tokio::process::Child;
use tokio_stream::StreamExt;

use super::auth::{auth_middleware, deploy_semaphore, max_concurrent_deploys};
use super::secrets::{secrets_delete, secrets_list, secrets_set};
use crate::{
    container::{ContainerRunner, container_log_path},
    deploy::DeployPipeline,
    deployments,
    ingress::default_ingress,
    metadata::{load_metadata_from_disk, prior_runtime_from_disk, resolve_lifecycle_runtime},
    microvm::MicrovmRunner,
    network::{PortAllocator, release_subnet},
    runtime::{self, RuntimeLifecycle},
    state::{AppState, LifecycleClaim},
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/vm/{service_id}/status", get(vm_status))
        .route("/vm/{service_id}/logs", get(vm_logs))
        .route("/vm/{service_id}/deployments", get(vm_deployments))
        .route("/vm/{service_id}/rollback", post(vm_rollback))
        .route("/status", get(status_all))
        .route("/logs", get(logs_all))
        .route("/vms", get(vms_list))
        .route("/vm/{service_id}/stop", post(vm_stop))
        .route("/vm/{service_id}/update", post(vm_update))
        .route("/vm/{service_id}", delete(vm_destroy))
        .route("/secrets", get(secrets_list))
        .route("/secrets/{name}", post(secrets_set).delete(secrets_delete))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state)
}

/// Build an `application/x-ndjson` streaming response from a deploy-event
/// receiver. The receiver is drained by the HTTP client; if serialization
/// fails mid-stream the connection closes cleanly.
fn ndjson_response(rx: tokio::sync::mpsc::Receiver<DeployEvent>) -> axum::response::Response {
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|msg| {
        let json = serde_json::to_string(&msg).map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("{}\n", json)))
    });
    let mut response = axum::response::Response::new(axum::body::Body::from_stream(stream));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

fn text_response(
    status: StatusCode,
    body: impl Into<axum::body::Body>,
) -> axum::response::Response {
    let mut response = axum::response::Response::new(body.into());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

/// Spawn an NDJSON-streamed deploy/update task, enforcing the deploy semaphore
/// and in-flight deploy guard (so graceful shutdown can wait). Returns the
/// streaming response on success, or a structured 503 when all deploy slots are
/// busy. Shared by `deploy` and `vm_update` (issue #54-lite).
fn spawn_deploy_stream(
    state: AppState,
    request: DeployRequest,
    service_id: String,
    task_label: &'static str,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let permit = deploy_semaphore().try_acquire().map_err(|_| {
        let max = max_concurrent_deploys();
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("too many concurrent deploys (max {max}); retry later"),
        )
    })?;

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let deploy_tx = tx.clone();
    let monitor_state = state.clone();
    let deploy_guard = state.begin_deploy();
    let sid = service_id;
    let sid2 = sid.clone();

    let deploy_handle = tokio::spawn(async move {
        let _permit = permit;
        let _guard = deploy_guard;
        let pipeline = DeployPipeline::new(state);
        let response = pipeline.deploy(request, deploy_tx.clone()).await;
        let status = response.status.clone();
        let elapsed_ms = response.elapsed_ms;
        tracing::info!(
            service_id = %sid,
            status = %status,
            elapsed_ms = elapsed_ms,
            "{task_label} deploy finished"
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
                .map(ToString::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| format!("{task_label} deploy task panicked"));
            tracing::error!(
                service_id = %sid2,
                panic = %detail,
                "{task_label} deploy task panicked"
            );
            monitor_state.mark_failed(&sid2, detail);
            let _ = tx
                .send(DeployEvent::Error(format!(
                    "{task_label} deploy task failed"
                )))
                .await;
        }
    });

    Ok(ndjson_response(rx))
}

async fn deploy(
    State(state): State<AppState>,
    Json(request): Json<DeployRequest>,
) -> axum::response::Response {
    if let Some(err) = request.port.as_ref().and_then(|p| p.validate().err()) {
        return text_response(StatusCode::BAD_REQUEST, err);
    }

    // #300: never default to shared "api" — concurrent deploys would collide.
    let service_id = match request
        .vm_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "vm_id is required (e.g. \"my-service\"); shared default \"api\" was removed",
            );
        }
    };

    tracing::info!(
        repo = %request.repo_url,
        service_id = %service_id,
        port = ?request.port.as_ref().map(|p| format!("{}:{}", p.host, p.guest)),
        "POST /deploy"
    );

    match spawn_deploy_stream(state, request, service_id, "deploy") {
        Ok(response) => response,
        Err((status, message)) => text_response(status, message),
    }
}

async fn vm_status(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::debug!(service_id = %service_id, "GET /vm/{}/status", service_id);

    // Agent mode (#214): the worker's view of local process state is
    // authoritative. Default (RUSSEL_AGENT_URL unset) stays in-process.
    if let Some(agent) = crate::agent_client::AgentClient::from_env() {
        let agent_status = agent
            .status(&service_id)
            .await
            .map_err(|e| (e.status, e.message))?;
        return Ok(Json(StatusResponse {
            service_id,
            status: if agent_status.status == ServiceStatus::Running.as_str() {
                ServiceStatus::Deployed.as_str().to_string()
            } else {
                agent_status.status
            },
            vm_state: agent_status.vm_state,
            uptime_seconds: agent_status.uptime_seconds,
            runtime: agent_status.runtime,
            host_port: agent_status.host_port,
            guest_port: agent_status.guest_port,
        }));
    }

    state.status(&service_id).map(Json).ok_or((
        StatusCode::NOT_FOUND,
        format!("service {} not found", service_id),
    ))
}

async fn vm_logs(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<LogsResponse>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::debug!(service_id = %service_id, "GET /vm/{}/logs", service_id);
    let mut resp = state.logs(&service_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("service {} not found", service_id),
    ))?;

    let runtime = resolve_lifecycle_runtime(state.runtime_for_service(&service_id), &service_id);
    if runtime == RuntimeKind::Container {
        append_podman_logs(&service_id, &mut resp.output).await;
    }

    Ok(Json(resp))
}

async fn append_podman_logs(service_id: &str, output: &mut String) {
    let name = ContainerRunner::container_name(service_id);
    let log_path = container_log_path(service_id);
    // #298: only skip podman logs when the on-disk file has usable content.
    // Empty/stale zero-byte files used to short-circuit and hide container output.
    if log_path.exists()
        && let Ok(meta) = tokio::fs::metadata(&log_path).await
        && meta.len() > 0
    {
        return;
    }

    let result = crate::container::podman_command()
        .await
        .args(["logs", "--tail", "200", &name])
        .output()
        .await;

    if let Ok(out) = result
        && out.status.success()
    {
        let logs = String::from_utf8_lossy(&out.stdout);
        if !logs.trim().is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("--- podman logs ---\n");
            output.push_str(&logs);
        }
    }
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
            vm_logs(State(state), Path(sid.clone())).await
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "multiple services exist; specify a service_id".to_string(),
        )),
    }
}

async fn vms_list(State(state): State<AppState>) -> Json<VmsResponse> {
    // Drop reserved system dirs that may already be in memory from older builds.
    let mut vms: Vec<String> = state
        .list_services()
        .into_iter()
        .filter(|id| !is_reserved_service_dir(id))
        .collect();
    let mut seen: std::collections::HashSet<String> = vms.iter().cloned().collect();

    for base in &["/var/lib/russel", "/var/lib/microvms"] {
        let Ok(mut entries) = tokio::fs::read_dir(base).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if is_reserved_service_dir(&name) {
                continue;
            }
            if seen.insert(name.clone()) {
                state.ensure_service(&name);
                vms.push(name);
            }
        }
    }

    discover_podman_containers(&state, &mut vms, &mut seen).await;

    vms.sort();

    let services: Vec<ServiceSummary> = vms
        .iter()
        .map(|id| {
            let cached = state.status(id);
            let status = cached
                .as_ref()
                .map(|s| s.status.clone())
                .unwrap_or_else(|| "stopped".to_string());
            let runtime = cached
                .and_then(|s| s.runtime)
                .or_else(|| prior_runtime_from_disk(id));
            ServiceSummary {
                service_id: id.clone(),
                runtime,
                status,
            }
        })
        .collect();

    tracing::debug!(
        count = vms.len(),
        "GET /vms -> {} services (from disk+memory+podman)",
        vms.len()
    );
    Json(VmsResponse { vms, services })
}

async fn discover_podman_containers(
    state: &AppState,
    vms: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    // Format: "name\tstate" so we can claim ports only for running containers.
    let output = crate::container::podman_command()
        .await
        .args([
            "ps",
            "-a",
            "--filter",
            "label=russel.runtime=container",
            "--format",
            "{{.Names}}\t{{.State}}",
        ])
        .output()
        .await;

    let Ok(out) = output else {
        return;
    };
    if !out.status.success() {
        return;
    }

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, container_state) = match line.split_once('\t') {
            Some((n, s)) => (n.trim(), s.trim().to_ascii_lowercase()),
            None => (line, String::new()),
        };
        let Some(service_id) = name.strip_prefix("russel-") else {
            continue;
        };

        // Always re-init known services so ports can be reclaimed after ctrl restart.
        state.ensure_service(service_id);
        if let Some(meta) = load_metadata_from_disk(service_id)
            && let Some(host_port) = meta.host_port
        {
            // Only bind allocator to ports of *running* containers.
            if container_state == "running"
                && let Err(e) = PortAllocator::claim_existing(service_id, host_port)
            {
                tracing::warn!(
                    service_id = %service_id,
                    host_port,
                    error = %e,
                    "failed to claim existing container port"
                );
            }
        }

        // De-dupe inventory listing only.
        if seen.insert(service_id.to_string()) {
            vms.push(service_id.to_string());
        }
    }
}

async fn vm_stop(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<String>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::info!(service_id = %service_id, "POST /vm/{}/stop", service_id);

    let (runtime, handle) =
        claim_lifecycle_operation(&state, &service_id, ServiceStatus::Stopping)?;
    let result = handle.lifecycle.stop(&service_id).await;

    let label = runtime_label(runtime);

    match result {
        Ok(_) => {
            // Reap only after successful stop — handles stayed in state during the op.
            if handle.runtime == RuntimeKind::Microvm
                && let Some((vm, aux)) = state.take_processes_for_reap(&service_id)
            {
                reap_children(vm, aux).await;
            }
            // Deregister from ingress so the proxy stops routing to this backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during stop");
            }
            tracing::info!(service_id = %service_id, runtime = %label, "stopped service");
            state.set_status(&service_id, ServiceStatus::Stopped, VmState::None);
            Ok(Json(format!("stopped {label} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, runtime = %label, error = %e, "failed to stop service");
            // Keep process ownership; restore prior status and re-supervise
            // only if this claim still owns the lifecycle generation.
            state.abort_lifecycle_operation(
                &service_id,
                handle.claim_generation,
                handle.expected_status,
                handle.prior_status,
                handle.prior_vm_state,
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

/// Desired-state update: re-read recorded Russelfile source and redeploy.
///
/// Optional body may override `repo_url` / `config_path`. When omitted, both
/// are taken from on-disk metadata written by the last successful deploy.
#[derive(Debug, Default, serde::Deserialize)]
struct UpdateBody {
    #[serde(default)]
    repo_url: Option<String>,
    #[serde(default)]
    config_path: Option<String>,
}

/// `GET /vm/{service_id}/deployments` — deployment history journal (newest first).
async fn vm_deployments(
    Path(service_id): Path<String>,
) -> Result<Json<DeploymentsResponse>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::info!(service_id = %service_id, "GET /vm/{}/deployments", service_id);
    deployments::list(&service_id)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `POST /vm/{service_id}/rollback` — explicit operator rollback to a prior version.
///
/// MVP strategy: redeploy from the journal entry's recorded `desired_state`
/// (repo_url / config_path / runtime / env / port / podman_args). Instant
/// dual-live retain-N=2 cutover is a follow-up.
async fn vm_rollback(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
    body: Option<Json<RollbackRequest>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let body = body.map(|j| j.0).unwrap_or_default();
    tracing::info!(
        service_id = %service_id,
        version = ?body.version,
        "POST /vm/{}/rollback",
        service_id
    );

    let target = deployments::select_rollback_target(&service_id, body.version).map_err(|e| {
        let status = match &e {
            deployments::RollbackSelectError::NotFound { .. } => StatusCode::NOT_FOUND,
            deployments::RollbackSelectError::Conflict { .. } => StatusCode::CONFLICT,
            deployments::RollbackSelectError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, e.to_string())
    })?;

    // Build DeployRequest from journal desired_state / top-level source fields.
    let ds = target.desired_state.as_ref();
    let repo_url = ds
        .and_then(|d| d.repo_url.clone())
        .or_else(|| target.repo_url.clone())
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "previous generation not retained; redeploy required — full retain-N=2 cutover TBD"
                    .to_string(),
            )
        })?;
    let config_path = ds
        .and_then(|d| d.config_path.clone())
        .or_else(|| target.config_path.clone())
        .unwrap_or_else(|| "Russelfile.toml".into());
    let runtime = ds.and_then(|d| d.runtime).or(target.runtime);
    let host_port = ds.and_then(|d| d.host_port).or(target.host_port);
    let guest_port = ds
        .and_then(|d| d.guest_port)
        .or(target.guest_port)
        .unwrap_or(3000);
    let env = ds.map(|d| d.env.clone()).unwrap_or_default();
    let podman_args = ds.map(|d| d.podman_args.clone()).unwrap_or_default();

    let request = DeployRequest {
        repo_url,
        config_path,
        vm_id: Some(service_id.clone()),
        port: host_port.map(|host| russel_core::api::PortMapping {
            host,
            guest: guest_port,
        }),
        runtime,
        env,
        podman_args,
    };

    // On the next successful deploy append, demote current active → rolled_back.
    // Marker is only consumed after success so a failed rollback redeploy leaves
    // history pointing at the still-live generation. Fail closed: if we can't
    // persist the marker, refuse to start the deploy stream.
    if let Err(e) = deployments::note_pending_rollback(&service_id, target.version) {
        tracing::error!(
            service_id = %service_id,
            error = %e,
            "failed to write rollback.pending marker"
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to record rollback pending marker: {e}"),
        ));
    }

    tracing::info!(
        service_id = %service_id,
        target_version = target.version,
        "rolling back via redeploy-from-history"
    );

    spawn_deploy_stream(state, request, service_id, "rollback")
}

async fn vm_update(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
    body: Option<Json<UpdateBody>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let body = body.map(|j| j.0).unwrap_or_default();
    let path = format!("/var/lib/russel/{service_id}/metadata.json");

    // Async metadata read (F-25-api): tokio::fs on the async hot path, no std::fs blocking.
    let meta: serde_json::Value = match tokio::fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str(&content).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("parse metadata: {e}"),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read metadata: {e}"),
            ));
        }
    };

    let top_repo_url = meta
        .get("repo_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let top_config_path = meta
        .get("config_path")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let top_host_port: Option<u16> = meta
        .get("host_port")
        .and_then(|v| v.as_u64())
        .map(u16::try_from)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid host_port in metadata: {e}"),
            )
        })?;
    let top_guest_port: Option<u16> = meta
        .get("guest_port")
        .and_then(|v| v.as_u64())
        .map(u16::try_from)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid guest_port in metadata: {e}"),
            )
        })?;
    let top_runtime: Option<RuntimeKind> = meta
        .get("runtime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());

    //
    // Typed via DesiredStateSnapshot (serde(default)); when absent/malformed we
    // fall back to the legacy top-level metadata. The writer persists the user's
    // original (pre-secret resolution) env plus podman_args / runtime / port that
    // the deploy ran with, so a later update can reproduce the request verbatim.
    let ds = deployments::DesiredStateSnapshot::from_metadata_desired_state(&meta);

    // Precedence: request body > desired_state > legacy top-level.
    let repo_url = body
        .repo_url
        .or(ds.repo_url.clone())
        .or(top_repo_url)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "no repo_url in request or metadata; pass {\"repo_url\":\"...\"} or redeploy once first".into(),
            )
        })?;
    let config_path = body
        .config_path
        .or(ds.config_path.clone())
        .or(top_config_path)
        .unwrap_or_else(|| "Russelfile.toml".into());

    let host_port = ds.host_port.or(top_host_port);
    let guest_port = ds.guest_port.or(top_guest_port).unwrap_or(3000);
    let runtime = ds.runtime.or(top_runtime);

    let request = DeployRequest {
        repo_url,
        config_path,
        vm_id: Some(service_id.clone()),
        port: host_port.map(|host| russel_core::api::PortMapping {
            host,
            guest: guest_port,
        }),
        runtime,
        env: ds.env,
        podman_args: ds.podman_args,
    };

    tracing::info!(service_id = %service_id, "POST /vm/{}/update", service_id);

    spawn_deploy_stream(state, request, service_id, "update")
}

async fn vm_destroy(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<String>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    tracing::info!(service_id = %service_id, "DELETE /vm/{}", service_id);

    let (runtime, handle) =
        claim_lifecycle_operation(&state, &service_id, ServiceStatus::Destroying)?;
    let result = handle.lifecycle.destroy(&service_id).await;

    let label = runtime_label(runtime);

    // Control-plane inventory ownership: always release port/subnet after a
    // destroy attempt (idempotent), including partial-failure / TAP teardown
    // error paths. Runner also releases for in-process callers; the API path
    // covers agent mode and ensures inventory is never permanently held when
    // destroy returns an error. Failed status is preserved below for retry.
    PortAllocator::release(&service_id);
    if runtime == RuntimeKind::Microvm {
        release_subnet(&service_id);
    }

    match result {
        Ok(_) => {
            // Reap child handles after successful destroy.
            if handle.runtime == RuntimeKind::Microvm
                && let Some((vm, aux)) = state.take_processes_for_reap(&service_id)
            {
                reap_children(vm, aux).await;
            }
            if handle.runtime == RuntimeKind::Container {
                let base = crate::container::default_base_dir(&service_id);
                if base.exists() {
                    let _ = tokio::fs::remove_dir_all(&base).await;
                }
            }
            // Deregister from ingress so the proxy stops routing to this (now destroyed) backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during destroy");
            }
            tracing::info!(service_id = %service_id, runtime = %label, "destroyed service");
            state.remove_service(&service_id);
            Ok(Json(format!("destroyed {label} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, runtime = %label, error = %e, "failed to destroy service");
            // Inventory already released above. Partial destroy may have
            // already killed processes — reap remaining handles; do not
            // restore deployed. Leave failed so the operator can retry
            // residual runtime/TAP cleanup.
            if handle.runtime == RuntimeKind::Microvm
                && let Some((vm, aux)) = state.take_processes_for_reap(&service_id)
            {
                reap_children(vm, aux).await;
            }
            state.set_status(&service_id, ServiceStatus::Failed, VmState::Failed);
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

/// Handle returned by `claim_lifecycle_operation` — holds a swappable
/// lifecycle provider plus prior status for failure recovery.
struct LifecycleClaimHandle {
    runtime: RuntimeKind,
    lifecycle: Arc<dyn RuntimeLifecycle>,
    /// Process generation at claim time; abort must match to restore.
    claim_generation: u64,
    /// In-progress status this claim set (`stopping` / `destroying`).
    expected_status: ServiceStatus,
    prior_status: ServiceStatus,
    prior_vm_state: VmState,
}

pub(crate) fn runtime_label(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Microvm => "microvm",
        RuntimeKind::Container => "container",
    }
}

fn claim_lifecycle_operation(
    state: &AppState,
    service_id: &str,
    target_status: ServiceStatus,
) -> Result<(RuntimeKind, LifecycleClaimHandle), (StatusCode, String)> {
    let runtime = resolve_lifecycle_runtime(state.runtime_for_service(service_id), service_id);

    let (prior_status, prior_vm_state, claim_generation) =
        match state.begin_lifecycle_operation(service_id, target_status, VmState::Pending) {
            LifecycleClaim::Claimed {
                prior_status,
                prior_vm_state,
                claim_generation,
            } => (prior_status, prior_vm_state, claim_generation),
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

    let handle = LifecycleClaimHandle {
        runtime,
        lifecycle: runtime::lifecycle_for(runtime),
        claim_generation,
        expected_status: target_status,
        prior_status,
        prior_vm_state,
    };

    Ok((runtime, handle))
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
