use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
};
use russel_core::api::{
    DeployEvent, DeployRequest, DeploymentsResponse, LogsResponse, RollbackRequest, ServiceSummary,
    StatusResponse, VmsResponse,
};
use russel_core::config::RuntimeKind;
use tokio::process::Child;
use tokio_stream::StreamExt;

use crate::{
    container::{ContainerRunner, container_log_path},
    deploy::DeployPipeline,
    deployments,
    ingress::default_ingress,
    metadata::{
        is_reserved_service_dir, load_metadata_from_disk, prior_runtime_from_disk,
        resolve_lifecycle_runtime,
    },
    microvm::MicrovmRunner,
    network::{PortAllocator, release_subnet},
    runtime::{self, RuntimeLifecycle},
    state::{AppState, LifecycleClaim},
};

/// Minimum accepted length for `RUSSEL_API_TOKEN` after trim (when set).
///
/// Floor is 32 **ASCII** characters so weak tokens like `"a"` are rejected.
/// Prefer `openssl rand -hex 32` (64 hex chars / 256 bits) for production.
///
/// Length is measured in bytes/`str::len`, which matches character count only
/// because non-ASCII tokens are rejected (see [`check_api_token_min_length`]).
pub const MIN_API_TOKEN_LEN: usize = 32;

/// Pure token normalize: unset/blank/whitespace → None.
///
/// Does **not** enforce min length / charset — call [`check_api_token_min_length`]
/// at startup when a token is present so short or non-header-safe secrets fail closed.
pub fn normalize_api_token(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether `token` can appear in an HTTP `Authorization` header value.
///
/// Matches what the CLI needs: `HeaderValue` accepts visible ASCII (0x20..=0x7E)
/// and HTAB. Multibyte Unicode and control bytes are rejected so ctrl never
/// starts with a token clients cannot send.
fn token_is_http_header_safe(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
}

/// Reject tokens that are too short or cannot be sent as a Bearer header value.
///
/// Call this at control-plane startup whenever `normalize_api_token` returns
/// `Some`. Middleware still uses the env token as-is; startup is the gate.
///
/// Checks (in order):
/// 1. HTTP header-safe charset (ASCII visible / HTAB) — same constraint as the CLI
/// 2. Length ≥ [`MIN_API_TOKEN_LEN`] (byte length; equivalent to char count after 1)
pub fn check_api_token_min_length(token: &str) -> Result<(), String> {
    if !token_is_http_header_safe(token) {
        return Err(
            "RUSSEL_API_TOKEN must be printable ASCII only so it can be sent in an \
             HTTP Authorization header (the CLI rejects non-header-safe tokens). \
             Generate a strong token with: openssl rand -hex 32"
                .to_string(),
        );
    }
    if token.len() < MIN_API_TOKEN_LEN {
        Err(format!(
            "RUSSEL_API_TOKEN must be at least {MIN_API_TOKEN_LEN} characters after trim \
             (got {}). Generate a strong token with: openssl rand -hex 32",
            token.len()
        ))
    } else {
        Ok(())
    }
}

/// Truthy parse for `RUSSEL_REQUIRE_AUTH`: `1`, `true`, or `yes` (case-insensitive).
///
/// When enabled, the control plane refuses to start without a valid token even
/// on loopback — use for production packaging that would otherwise default to
/// loopback bind.
pub fn require_auth_from_env(raw: Option<&str>) -> bool {
    raw.map(|s| {
        let s = s.trim();
        s.eq_ignore_ascii_case("1")
            || s.eq_ignore_ascii_case("true")
            || s.eq_ignore_ascii_case("yes")
    })
    .unwrap_or(false)
}

/// Non-empty RUSSEL_API_TOKEN after trim; None if unset/blank.
///
/// Length is not checked here — `main` calls [`check_api_token_min_length`]
/// before serving so short tokens never enable a weak auth mode.
pub fn configured_api_token() -> Option<String> {
    normalize_api_token(std::env::var("RUSSEL_API_TOKEN").ok().as_deref())
}

/// Parse max concurrent deploys (default 4, clamp 1..=64).
pub fn parse_max_concurrent_deploys(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse().ok())
        .map(|n: usize| n.clamp(1, 64))
        .unwrap_or(4)
}

/// Max concurrent deploy tasks, from RUSSEL_MAX_CONCURRENT_DEPLOYS (default 4).
fn max_concurrent_deploys() -> usize {
    static MAX: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        parse_max_concurrent_deploys(
            std::env::var("RUSSEL_MAX_CONCURRENT_DEPLOYS")
                .ok()
                .as_deref(),
        )
    });
    *MAX
}

/// Global semaphore bounding in-flight deploy/update tasks.
pub(crate) fn deploy_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::LazyLock<tokio::sync::Semaphore> =
        std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(max_concurrent_deploys()));
    &SEM
}

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

/// Constant-time token comparison to avoid timing side-channels.
///
/// Always walks `max(a.len(), b.len())` bytes so the result does not leak the
/// input lengths. A length mismatch is folded into the accumulator as a
/// non-zero delta rather than returned early.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    // Length mismatch must always contribute a nonzero delta. Narrowing
    // `(a.len() ^ b.len()) as u8` drops high bits (e.g. len 1 vs 257 → 0).
    let mut diff: u8 = u8::from(a.len() != b.len());
    for i in 0..max_len {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Bearer auth middleware: if RUSSEL_API_TOKEN is set (non-empty, trimmed),
/// require it on every request.
///
/// Env: `RUSSEL_API_TOKEN` (min length enforced at process start), optional
/// `RUSSEL_REQUIRE_AUTH=1|true|yes` to fail closed without a token on loopback.
async fn auth_middleware(
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = configured_api_token() else {
        return Ok(next.run(request).await);
    };

    let header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided = header.strip_prefix("Bearer ").unwrap_or("");

    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

/// Build an `application/x-ndjson` streaming response from a deploy-event
/// receiver. The receiver is drained by the HTTP client; if serialization
/// fails mid-stream the connection closes cleanly.
fn ndjson_response(rx: tokio::sync::mpsc::Receiver<DeployEvent>) -> axum::response::Response {
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|msg| {
        let json = serde_json::to_string(&msg).map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("{}\n", json)))
    });
    axum::response::Response::builder()
        .header("Content-Type", "application/x-ndjson")
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to build NDJSON stream response");
            axum::response::Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::from(e.to_string()))
                .unwrap_or_else(|_| {
                    axum::response::Response::new(axum::body::Body::from("internal server error"))
                })
        })
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
    let permit = match deploy_semaphore().try_acquire() {
        Ok(p) => p,
        Err(_) => {
            let max = max_concurrent_deploys();
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("too many concurrent deploys (max {max}); retry later"),
            ));
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let deploy_tx = tx.clone();
    let monitor_state = state.clone();
    let deploy_guard = state.begin_deploy();
    let sid = service_id.clone();
    let sid2 = service_id.clone();

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
                .map(|s| s.to_string())
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
    let service_id = request.vm_id.clone().unwrap_or_else(|| "api".to_string());

    tracing::info!(
        repo = %request.repo_url,
        service_id = %service_id,
        port = ?request.port.as_ref().map(|p| format!("{}:{}", p.host, p.guest)),
        "POST /deploy"
    );

    match spawn_deploy_stream(state, request, service_id, "deploy") {
        Ok(response) => response,
        Err((status, message)) => axum::response::Response::builder()
            .status(status)
            .header("Content-Type", "text/plain")
            .body(axum::body::Body::from(message))
            .unwrap_or_else(|_| {
                axum::response::Response::new(axum::body::Body::from("internal server error"))
            }),
    }
}

async fn vm_status(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
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
    if log_path.exists() {
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
        let mut entries = match tokio::fs::read_dir(base).await {
            Ok(e) => e,
            Err(_) => continue,
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

    let (runtime, handle) = claim_lifecycle_operation(&state, &service_id, "stopping")?;
    let result = handle.lifecycle.stop(&service_id).await;
    if handle.runtime == RuntimeKind::Microvm {
        reap_children(handle.vm_child, handle.aux_processes).await;
    }

    let label = runtime_label(runtime);

    match result {
        Ok(_) => {
            // Deregister from ingress so the proxy stops routing to this backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during stop");
            }
            tracing::info!(service_id = %service_id, runtime = %label, "stopped service");
            state.set_status(&service_id, "stopped", "none");
            Ok(Json(format!("stopped {label} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, runtime = %label, error = %e, "failed to stop service");
            state.set_status(&service_id, "failed", "failed");
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

    // ── Legacy top-level metadata fields (fallback) ────────────────────────
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

    // ── desired_state block (SHARED CONTRACT with deploy-desired-state agent)
    //
    // All fields are optional; when absent we fall back to the legacy
    // top-level metadata. The writer persists the user's original (pre-secret
    // resolution) env plus the podman_args / runtime / port that the deploy
    // ran with, so a later update can reproduce the request verbatim.
    let desired = meta.get("desired_state").and_then(|v| v.as_object());
    let ds_repo_url = desired
        .and_then(|d| d.get("repo_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ds_config_path = desired
        .and_then(|d| d.get("config_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ds_runtime: Option<RuntimeKind> = desired
        .and_then(|d| d.get("runtime"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let ds_env: HashMap<String, String> = desired
        .and_then(|d| d.get("env"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let ds_podman_args: Vec<String> = desired
        .and_then(|d| d.get("podman_args"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let ds_port = desired.and_then(|d| d.get("port").and_then(|p| p.as_object()));
    let ds_host_port: Option<u16> = ds_port
        .and_then(|p| p.get("host"))
        .and_then(|v| v.as_u64())
        .map(u16::try_from)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid desired_state.port.host: {e}"),
            )
        })?;
    let ds_guest_port: Option<u16> = ds_port
        .and_then(|p| p.get("guest"))
        .and_then(|v| v.as_u64())
        .map(u16::try_from)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid desired_state.port.guest: {e}"),
            )
        })?;

    // Precedence: request body > desired_state > legacy top-level.
    let repo_url = body
        .repo_url
        .or(ds_repo_url)
        .or(top_repo_url)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "no repo_url in request or metadata; pass {\"repo_url\":\"...\"} or redeploy once first".into(),
            )
        })?;
    let config_path = body
        .config_path
        .or(ds_config_path)
        .or(top_config_path)
        .unwrap_or_else(|| "Russelfile.toml".into());

    let host_port = ds_host_port.or(top_host_port);
    let guest_port = ds_guest_port.or(top_guest_port).unwrap_or(3000);
    let runtime = ds_runtime.or(top_runtime);

    let request = DeployRequest {
        repo_url,
        config_path,
        vm_id: Some(service_id.clone()),
        port: host_port.map(|host| russel_core::api::PortMapping {
            host,
            guest: guest_port,
        }),
        runtime,
        env: ds_env,
        podman_args: ds_podman_args,
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

    let (runtime, handle) = claim_lifecycle_operation(&state, &service_id, "destroying")?;
    let result = handle.lifecycle.destroy(&service_id).await;
    if handle.runtime == RuntimeKind::Microvm {
        reap_children(handle.vm_child, handle.aux_processes).await;
    }
    // container destroy still does base-dir cleanup on success as today
    if handle.runtime == RuntimeKind::Container && result.is_ok() {
        PortAllocator::release(&service_id);
        let base = crate::container::default_base_dir(&service_id);
        if base.exists() {
            let _ = tokio::fs::remove_dir_all(&base).await;
        }
    }

    let label = runtime_label(runtime);

    match result {
        Ok(_) => {
            // Deregister from ingress so the proxy stops routing to this (now destroyed) backend.
            let ingress = default_ingress();
            if let Err(e) = ingress.deregister(&service_id).await {
                tracing::warn!(service_id = %service_id, error = %e, "failed to deregister from ingress during destroy");
            }
            if runtime == RuntimeKind::Microvm {
                release_subnet(&service_id);
            }
            tracing::info!(service_id = %service_id, runtime = %label, "destroyed service");
            state.remove_service(&service_id);
            Ok(Json(format!("destroyed {label} {service_id}")))
        }
        Err(e) => {
            tracing::error!(service_id = %service_id, runtime = %label, error = %e, "failed to destroy service");
            state.set_status(&service_id, "failed", "failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("error: {}", e)))
        }
    }
}

/// Handle returned by `claim_lifecycle_operation` — holds a swappable
/// lifecycle provider plus any microVM child processes to reap after
/// stop/destroy.
struct LifecycleClaimHandle {
    runtime: RuntimeKind,
    lifecycle: Arc<dyn RuntimeLifecycle>,
    /// Only microvm carries claimed children to reap after stop/destroy
    vm_child: Option<Child>,
    aux_processes: Vec<Child>,
}

fn runtime_label(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Microvm => "microvm",
        RuntimeKind::Container => "container",
    }
}

fn claim_lifecycle_operation(
    state: &AppState,
    service_id: &str,
    target_status: &str,
) -> Result<(RuntimeKind, LifecycleClaimHandle), (StatusCode, String)> {
    let runtime = resolve_lifecycle_runtime(state.runtime_for_service(service_id), service_id);

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

    let handle = LifecycleClaimHandle {
        runtime,
        lifecycle: runtime::lifecycle_for(runtime),
        vm_child,
        aux_processes,
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

// ── Secrets API ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct SecretSetBody {
    value: String,
}

#[derive(Debug, serde::Serialize)]
struct SecretsListResponse {
    secrets: Vec<String>,
}

async fn secrets_list() -> Result<Json<SecretsListResponse>, (StatusCode, String)> {
    crate::secrets::list_secrets()
        .map(|secrets| Json(SecretsListResponse { secrets }))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn secrets_set(
    Path(name): Path<String>,
    Json(body): Json<SecretSetBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let result = crate::secrets::set_secret(&name, &body.value)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()));
    if result.is_ok() {
        // Audit log — name only, never the secret value.
        tracing::info!(secret = %name, "secret set via API");
    }
    result
}

async fn secrets_delete(Path(name): Path<String>) -> Result<StatusCode, (StatusCode, String)> {
    match crate::secrets::delete_secret(&name) {
        Ok(true) => {
            // Audit log — name only, never the secret value.
            tracing::info!(secret = %name, "secret deleted via API");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, format!("secret {name:?} not found"))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── auth / concurrency helpers ─────────────────────────────────────

    #[test]
    fn token_normalize_empty() {
        assert_eq!(normalize_api_token(None), None);
        assert_eq!(normalize_api_token(Some("")), None);
        assert_eq!(normalize_api_token(Some("   ")), None);
    }

    #[test]
    fn token_normalize_valid() {
        // Normalize still returns short non-empty strings; min length is a
        // separate startup check (see check_api_token_min_length).
        assert_eq!(
            normalize_api_token(Some("secret")),
            Some("secret".to_string())
        );
        assert_eq!(
            normalize_api_token(Some("  secret  ")),
            Some("secret".to_string())
        );
        let long = "a".repeat(MIN_API_TOKEN_LEN);
        assert_eq!(
            normalize_api_token(Some(&format!("  {long}  "))),
            Some(long)
        );
    }

    #[test]
    fn token_min_length_rejects_short() {
        assert!(check_api_token_min_length("a").is_err());
        assert!(check_api_token_min_length("short-token").is_err());
        assert!(check_api_token_min_length(&"x".repeat(MIN_API_TOKEN_LEN - 1)).is_err());
        let err = check_api_token_min_length("a").unwrap_err();
        assert!(err.contains("openssl rand -hex 32"), "err={err}");
        assert!(err.contains(&MIN_API_TOKEN_LEN.to_string()), "err={err}");
    }

    #[test]
    fn token_min_length_accepts_floor_and_longer() {
        assert!(check_api_token_min_length(&"a".repeat(MIN_API_TOKEN_LEN)).is_ok());
        assert!(check_api_token_min_length(&"b".repeat(64)).is_ok()); // openssl rand -hex 32
    }

    #[test]
    fn token_rejects_non_ascii_even_when_utf8_byte_len_meets_floor() {
        // Each 'é' is 2 UTF-8 bytes; 16 of them → 32 bytes, which used to pass
        // a pure `str::len` floor while the CLI cannot put it in Authorization.
        let unicode = "é".repeat(16);
        assert!(unicode.len() >= MIN_API_TOKEN_LEN);
        assert!(unicode.chars().count() < MIN_API_TOKEN_LEN);
        let err = check_api_token_min_length(&unicode).unwrap_err();
        assert!(
            err.contains("printable ASCII") || err.contains("Authorization"),
            "err={err}"
        );

        // Multibyte emoji: few chars, many bytes.
        let emoji = "🔐".repeat(8);
        assert!(emoji.len() >= MIN_API_TOKEN_LEN);
        assert!(check_api_token_min_length(&emoji).is_err());
    }

    #[test]
    fn token_rejects_ascii_control_bytes() {
        let mut s = "a".repeat(MIN_API_TOKEN_LEN);
        s.replace_range(0..1, "\n");
        assert!(check_api_token_min_length(&s).is_err());
    }

    #[test]
    fn require_auth_from_env_truthy() {
        assert!(!require_auth_from_env(None));
        assert!(!require_auth_from_env(Some("")));
        assert!(!require_auth_from_env(Some("0")));
        assert!(!require_auth_from_env(Some("false")));
        assert!(!require_auth_from_env(Some("no")));
        assert!(require_auth_from_env(Some("1")));
        assert!(require_auth_from_env(Some("true")));
        assert!(require_auth_from_env(Some("YES")));
        assert!(require_auth_from_env(Some(" True ")));
    }

    #[test]
    fn parse_max_concurrent_deploys_clamps() {
        assert_eq!(parse_max_concurrent_deploys(None), 4);
        assert_eq!(parse_max_concurrent_deploys(Some("")), 4);
        assert_eq!(parse_max_concurrent_deploys(Some("8")), 8);
        assert_eq!(parse_max_concurrent_deploys(Some("0")), 1);
        assert_eq!(parse_max_concurrent_deploys(Some("999")), 64);
        assert_eq!(parse_max_concurrent_deploys(Some("nope")), 4);
    }

    // ── existing tests ─────────────────────────────────────────────────

    #[test]
    fn resolve_lifecycle_runtime_uses_state_over_disk_default() {
        assert_eq!(
            resolve_lifecycle_runtime(Some(RuntimeKind::Container), "missing"),
            RuntimeKind::Container
        );
        assert_eq!(
            resolve_lifecycle_runtime(None, "missing"),
            RuntimeKind::Microvm
        );
    }

    #[test]
    fn runtime_label_matches_kind() {
        assert_eq!(runtime_label(RuntimeKind::Microvm), "microvm");
        assert_eq!(runtime_label(RuntimeKind::Container), "container");
    }

    // ── constant_time_eq ───────────────────────────────────────────────

    #[test]
    fn constant_time_eq_identical() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_different_same_length() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"\x00\x01", b"\x00\x00"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        // Same prefix, different lengths — MUST return false.
        assert!(!constant_time_eq(b"hello", b"hello!"));
        // Completely different lengths
        assert!(!constant_time_eq(b"a", b""));
        assert!(!constant_time_eq(b"", b"a"));
        // Long vs short with shared prefix
        assert!(!constant_time_eq(b"abcdefghij", b"abcde"));
    }

    #[test]
    fn constant_time_eq_zeroed_suffix_matches() {
        // A shorter slice that is a prefix of the longer one, where the
        // longer slice has zero-padding after the shared prefix — NOT equal
        // because the length mismatch is folded into the diff.
        assert!(!constant_time_eq(b"abc", b"abc\0\0"));
        // Len XOR truncated to u8 would be 0 for 1 vs 257; still must reject.
        let short = [0u8; 1];
        let long = [0u8; 257];
        assert!(!constant_time_eq(&short, &long));
    }
}
