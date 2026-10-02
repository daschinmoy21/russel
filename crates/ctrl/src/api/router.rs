//! HTTP router and service lifecycle handlers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::{HeaderValue, StatusCode, Uri, header::CONTENT_TYPE},
    middleware,
    response::IntoResponse,
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
use tower_http::services::ServeDir;

use super::auth::{auth_middleware, deploy_semaphore, max_concurrent_deploys};
use super::secrets::{secrets_delete, secrets_list, secrets_set};
use crate::{
    container::{ContainerRunner, container_log_path},
    dashboard::DashboardDir,
    deploy::DeployPipeline,
    deployments,
    ingress::default_ingress,
    metadata::{load_metadata_from_disk, prior_runtime_from_disk, resolve_lifecycle_runtime},
    network::{PortAllocator, release_subnet},
    runtime::{self, RuntimeLifecycle},
    state::{AppState, LifecycleClaim},
};

/// Control-plane API only (CLI paths). Tests and `--no-dashboard` use this.
pub fn router(state: AppState) -> Router {
    router_inner(state, None)
}

/// API at `/` and `/api/*`, plus the static dashboard on GET `/`, `/deploy`, …
pub fn router_with_dashboard(state: AppState, dashboard_dir: impl Into<PathBuf>) -> Router {
    router_inner(state, Some(dashboard_dir.into()))
}

fn router_inner(state: AppState, dashboard_dir: Option<PathBuf>) -> Router {
    let routes = if let Some(dir) = dashboard_dir.as_ref() {
        // GET /deploy is the UI page; POST /deploy stays the API. Nested `/api`
        // is the dashboard's default base (Vite used to strip that prefix).
        api_routes(true)
            .nest_service("/_astro", ServeDir::new(dir.join("_astro")))
            .nest("/api", api_routes(false))
    } else {
        api_routes(false)
    };
    let routes = routes.layer(middleware::from_fn(auth_middleware));
    let routes = match dashboard_dir {
        Some(dir) => routes.layer(Extension(DashboardDir(dir))),
        None => routes,
    };
    routes.with_state(state)
}

fn api_routes(dashboard: bool) -> Router<AppState> {
    let r = Router::new()
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
        .route("/secrets/{name}", post(secrets_set).delete(secrets_delete));

    if dashboard {
        r.route("/", get(serve_dashboard))
            .route("/services", get(serve_dashboard))
            .route("/services/", get(serve_dashboard))
            .route("/service-detail", get(serve_dashboard))
            .route("/service-detail/", get(serve_dashboard))
            .route("/settings", get(serve_dashboard))
            .route("/settings/", get(serve_dashboard))
            .route("/favicon.svg", get(serve_dashboard))
            .route("/deploy/", get(serve_dashboard))
            .route("/deploy", get(serve_dashboard).post(deploy))
    } else {
        r.route("/deploy", post(deploy))
    }
}

async fn serve_dashboard(
    uri: Uri,
    Extension(dir): Extension<DashboardDir>,
) -> axum::response::Response {
    let Some(path) = crate::dashboard::dashboard_page_file(&dir.0, uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ctype = match path.extension().and_then(|e| e.to_str()) {
                Some("svg") => "image/svg+xml",
                Some("js") => "application/javascript",
                Some("css") => "text/css",
                _ => "text/html; charset=utf-8",
            };
            let mut response = axum::response::Response::new(bytes.into());
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static(ctype));
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
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
    let pipeline = DeployPipeline::new(state);
    let claimed_id = pipeline.claimed_service_id();

    let deploy_handle = tokio::spawn(async move {
        let _permit = permit;
        let _guard = deploy_guard;
        let response = pipeline.deploy(request, deploy_tx.clone()).await;
        let status = response.status.clone();
        let elapsed_ms = response.elapsed_ms;
        tracing::info!(
            service_id = %response.service_id,
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
                service_id = %service_id,
                panic = %detail,
                "{task_label} deploy task panicked"
            );
            // Only a claimed service is marked failed: the id comes from the
            // Russelfile, and a panic before `mark_building` touched nothing.
            if let Some(id) = claimed_id.get() {
                monitor_state.mark_failed(id, detail);
            }
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
    // The service id is the Russelfile `service.name`; the pipeline reads it
    // after fetching the source. `vm_id`, when sent, is only a check (#446).
    let requested_id = request.vm_id.clone().unwrap_or_default();
    tracing::info!(
        repo = %crate::git::redact_repo_url(&request.repo_url),
        requested_id = %requested_id,
        "POST /deploy"
    );

    match spawn_deploy_stream(state, request, requested_id, "deploy") {
        Ok(response) => response,
        Err((status, message)) => text_response(status, message),
    }
}

async fn vm_status(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    check_service_id(&service_id)?;
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
            route_host: None,
            restarts: None,
            requested: None,
            effective: None,
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
    check_service_id(&service_id)?;
    tracing::debug!(service_id = %service_id, "GET /vm/{}/logs", service_id);
    let mut resp = state.logs(&service_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("service {} not found", service_id),
    ))?;

    // Unreadable metadata is 500 on stop/destroy. Logs still return the snapshot.
    let runtime =
        match resolve_lifecycle_runtime(state.runtime_for_service(&service_id), &service_id) {
            Ok(rt) => Some(rt),
            Err(e) => {
                tracing::warn!(
                    service_id = %service_id,
                    error = %e,
                    "metadata unreadable; returning in-memory logs without podman tail"
                );
                None
            }
        };
    if runtime == Some(RuntimeKind::Container) {
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

    let bases = [crate::paths::data_root(), crate::paths::microvms_root()];
    for base in &bases {
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
            // `destroy` with `keep = true` leaves only `volumes/`. That is
            // data, not a service. Listing it re-created a stopped entry that
            // a second destroy would then treat as a microVM.
            if crate::container::dir_is_kept_volumes_only(&entry.path()) {
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
            // Unreadable metadata is already logged; do not treat it as missing.
            let runtime = cached
                .and_then(|s| s.runtime)
                .or_else(|| prior_runtime_from_disk(id).ok().flatten());
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

/// Map a Podman container name to a service id.
///
/// Accepts `russel-{id}` and the dual-live generation name
/// `russel-{id}_g` + exactly 8 lowercase hex digits (`new_generation_id`).
/// Shorter or non-hex suffixes are part of the id. Names without the
/// `russel-` prefix are ignored.
fn service_id_from_container_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("russel-")?;
    if rest.is_empty() {
        return None;
    }
    let Some((base, hex)) = rest.rsplit_once("_g") else {
        return Some(rest);
    };
    if !base.is_empty()
        && hex.len() == 8
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        Some(base)
    } else {
        Some(rest)
    }
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
        // `russel-{id}_g{8 hex}` is a dual-live generation container, not a
        // second service. Other suffixes stay intact.
        let Some(service_id) = service_id_from_container_name(name) else {
            continue;
        };
        if is_reserved_service_dir(service_id) {
            continue;
        }

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
    check_service_id(&service_id)?;
    tracing::info!(service_id = %service_id, "POST /vm/{}/stop", service_id);

    let handle = claim_lifecycle_operation(&state, &service_id, ServiceStatus::Stopping)?;
    let result = handle.lifecycle.stop(&service_id).await;
    let runtime = handle.runtime;

    match result {
        Ok(_) => {
            // Reap only after successful stop — handles stayed in state during the op.
            handle.reap(&state, &service_id).await;
            // restart = "unless-stopped" must leave an operator stop alone,
            // including across a ctrl restart.
            if runtime == RuntimeKind::Microvm {
                crate::restart::note_user_stop(&service_id);
            }
            // Deregister from ingress so the proxy stops routing to this backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during stop");
            }
            tracing::info!(service_id = %service_id, %runtime, "stopped service");
            state.set_status(&service_id, ServiceStatus::Stopped, VmState::None);
            Ok(Json(format!("stopped {runtime} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, %runtime, error = %e, "failed to stop service");
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
    /// Build the source's current HEAD instead of the recorded commit.
    #[serde(default)]
    refresh: bool,
}

/// `GET /vm/{service_id}/deployments` — deployment history journal (newest first).
async fn vm_deployments(
    Path(service_id): Path<String>,
) -> Result<Json<DeploymentsResponse>, (StatusCode, String)> {
    check_service_id(&service_id)?;
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
    check_service_id(&service_id)?;

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

    let request = target
        .recorded_source()
        .redeploy_request(&service_id)
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "previous generation not retained; redeploy required — full retain-N=2 cutover TBD"
                    .to_string(),
            )
        })?;

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
    check_service_id(&service_id)?;

    let body = body.map(|j| j.0).unwrap_or_default();
    let path = crate::metadata::metadata_path(&service_id);

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

    // Precedence: request body > desired_state > legacy top-level.
    let mut source = deployments::DesiredStateSnapshot::from_metadata_with_legacy(&meta)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Update rebuilds the recorded commit (#448). A new source or config
    // path, or --refresh, builds whatever that source points at now.
    if body.refresh || body.repo_url.is_some() || body.config_path.is_some() {
        source.rev = None;
    }
    source.repo_url = body.repo_url.or(source.repo_url);
    source.config_path = body.config_path.or(source.config_path);
    let request = source.redeploy_request(&service_id).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "no repo_url in request or metadata; pass {\"repo_url\":\"...\"} or redeploy once first"
                .to_string(),
        )
    })?;

    tracing::info!(service_id = %service_id, "POST /vm/{}/update", service_id);

    spawn_deploy_stream(state, request, service_id, "update")
}

#[derive(Debug, Default, serde::Deserialize)]
struct DestroyQuery {
    #[serde(default)]
    keep_volumes: Option<bool>,
}

async fn vm_destroy(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
    Query(query): Query<DestroyQuery>,
) -> Result<Json<String>, (StatusCode, String)> {
    check_service_id(&service_id)?;
    let policy = russel_core::VolumeDestroyPolicy::from_keep_override(query.keep_volumes);
    tracing::info!(service_id = %service_id, "DELETE /vm/{}", service_id);

    // Resolve runtime and reject unsupported query params BEFORE claiming
    // Destroying. Returning Err after claim leaves status stuck.
    // Unreadable metadata is 500 — assuming microVM would destroy a container.
    require_lifecycle_runtime(&state, &service_id)?;
    // Service-dir cleanup is owned by destroy_with_policy on both runtimes
    // (FollowFile / KeepAll / DeleteAll). The agent RPC path has no policy
    // slot yet, so an explicit keep_volumes query against agent mode is
    // rejected instead of silently following the file.
    if crate::agent_client::agent_mode_enabled() && query.keep_volumes.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "keep_volumes is not supported in agent mode (unset RUSSEL_AGENT_URL or omit the query)"
                .into(),
        ));
    }

    let handle = claim_lifecycle_operation(&state, &service_id, ServiceStatus::Destroying)?;
    let runtime = handle.runtime;
    let result = match runtime {
        _ if crate::agent_client::agent_mode_enabled() => {
            handle.lifecycle.destroy(&service_id).await
        }
        RuntimeKind::Container => {
            crate::container::destroy_with_policy_for(&service_id, policy).await
        }
        RuntimeKind::Microvm => {
            crate::microvm::shared_runner()
                .destroy_with_policy(&service_id, policy)
                .await
        }
    };

    // Control-plane inventory ownership: always release port/subnet after a
    // destroy attempt (idempotent), including partial-failure / TAP teardown
    // error paths. Runner also releases for in-process callers; the API path
    // covers agent mode and ensures inventory is never permanently held when
    // destroy returns an error. Failed status is preserved below for retry.
    PortAllocator::release_service(&service_id);
    if runtime == RuntimeKind::Microvm {
        release_subnet(&service_id);
    }

    match result {
        Ok(_) => {
            // Reap child handles after successful destroy.
            handle.reap(&state, &service_id).await;
            // Container service-dir cleanup is owned by
            // destroy_with_policy (FollowFile / KeepAll / DeleteAll).
            // No extra remove_dir_all here: it would delete kept volumes.
            // Deregister from ingress so the proxy stops routing to this (now destroyed) backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during destroy");
            }
            // Kept volumes are not Nix paths; every generation root goes.
            if let Err(e) = crate::gcroots::remove(&service_id) {
                tracing::warn!(service_id = %service_id, error = %e, "failed to remove gcroots during destroy");
            }
            tracing::info!(service_id = %service_id, %runtime, "destroyed service");
            state.remove_service(&service_id);
            Ok(Json(format!("destroyed {runtime} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, %runtime, error = %e, "failed to destroy service");
            // Inventory already released above. Partial destroy may have
            // already killed processes — reap remaining handles; do not
            // restore deployed. Leave failed so the operator can retry
            // residual runtime/TAP cleanup.
            handle.reap(&state, &service_id).await;
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

impl LifecycleClaimHandle {
    /// Wait for (or kill) the microVM's process handles once the operation
    /// ended. Containers have none: Podman owns their processes.
    async fn reap(&self, state: &AppState, service_id: &str) {
        if self.runtime == RuntimeKind::Microvm
            && let Some((vm, aux)) = state.take_processes_for_reap(service_id)
        {
            reap_children(vm, aux).await;
        }
    }
}

fn check_service_id(service_id: &str) -> Result<(), (StatusCode, String)> {
    russel_core::ids::validate_service_id(service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

fn require_lifecycle_runtime(
    state: &AppState,
    service_id: &str,
) -> Result<RuntimeKind, (StatusCode, String)> {
    resolve_lifecycle_runtime(state.runtime_for_service(service_id), service_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read metadata for {service_id}: {e}"),
        )
    })
}

fn claim_lifecycle_operation(
    state: &AppState,
    service_id: &str,
    target_status: ServiceStatus,
) -> Result<LifecycleClaimHandle, (StatusCode, String)> {
    let runtime = require_lifecycle_runtime(state, service_id)?;

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

    Ok(LifecycleClaimHandle {
        runtime,
        lifecycle: runtime::lifecycle_for(runtime),
        claim_generation,
        expected_status: target_status,
        prior_status,
        prior_vm_state,
    })
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

#[cfg(test)]
mod tests {
    use super::service_id_from_container_name;

    #[test]
    fn service_id_from_container_name_strips_generation_suffix() {
        assert_eq!(service_id_from_container_name("russel-api"), Some("api"));
        assert_eq!(
            service_id_from_container_name("russel-api_gdeadbeef"),
            Some("api")
        );
        // 7 hex digits is not a generation id — do not strip.
        assert_eq!(
            service_id_from_container_name("russel-api_gdeadbee"),
            Some("api_gdeadbee")
        );
        assert_eq!(
            service_id_from_container_name("russel-foo_g12345678"),
            Some("foo")
        );
        assert_eq!(service_id_from_container_name("api"), None);
        assert_eq!(service_id_from_container_name("russel-"), None);
    }
}
