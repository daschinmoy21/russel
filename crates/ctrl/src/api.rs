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
    DeployEvent, DeployRequest, LogsResponse, ServiceSummary, StatusResponse, VmsResponse,
};
use russel_core::config::RuntimeKind;
use tokio::process::Child;
use tokio_stream::StreamExt;

use crate::{
    container::{ContainerRunner, container_log_path},
    deploy::DeployPipeline,
    ingress::default_ingress,
    metadata::{load_metadata_from_disk, prior_runtime_from_disk, resolve_lifecycle_runtime},
    microvm::MicrovmRunner,
    network::{PortAllocator, release_subnet},
    state::{AppState, LifecycleClaim},
};

/// Pure token normalize: unset/blank/whitespace → None.
pub fn normalize_api_token(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Non-empty RUSSEL_API_TOKEN after trim; None if unset/blank.
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
fn deploy_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::LazyLock<tokio::sync::Semaphore> =
        std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(max_concurrent_deploys()));
    &SEM
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/deploy", post(deploy))
        .route("/vm/{service_id}/status", get(vm_status))
        .route("/vm/{service_id}/logs", get(vm_logs))
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
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Bearer auth middleware: if RUSSEL_API_TOKEN is set (non-empty, trimmed),
/// require it on every request.
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

    // Bound concurrent deploys: if all slots are busy, return 503 immediately.
    let permit = match deploy_semaphore().try_acquire() {
        Ok(p) => p,
        Err(_) => {
            let max = max_concurrent_deploys();
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("Content-Type", "text/plain")
                .body(axum::body::Body::from(format!(
                    "too many concurrent deploys (max {max}); retry later"
                )))
                .unwrap_or_else(|e| {
                    axum::response::Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(axum::body::Body::from(e.to_string()))
                        .unwrap_or_else(|_| {
                            axum::response::Response::new(axum::body::Body::from(
                                "internal server error",
                            ))
                        })
                });
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let deploy_tx = tx.clone();
    let monitor_state = state.clone();
    let sid = service_id.clone();
    let sid2 = service_id.clone();

    // Track in-flight deploy so shutdown can wait before detaching processes
    let deploy_guard = state.begin_deploy();

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
    let mut vms = state.list_services();
    let mut seen: std::collections::HashSet<String> = vms.iter().cloned().collect();

    for base in &["/var/lib/russel", "/var/lib/microvms"] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type()
                    && file_type.is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    if name.ends_with(".bak") {
                        continue;
                    }
                    if seen.insert(name.to_string()) {
                        state.ensure_service(name);
                        vms.push(name.to_string());
                    }
                }
            }
        }
    }

    discover_podman_containers(&state, &mut vms, &mut seen).await;

    vms.sort();

    let services: Vec<ServiceSummary> = vms
        .iter()
        .map(|id| {
            let status = state
                .status(id)
                .map(|s| s.status)
                .unwrap_or_else(|| "stopped".to_string());
            let runtime = state
                .status(id)
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
    tracing::info!(service_id = %service_id, "POST /vm/{}/stop", service_id);

    let (runtime, claim) = claim_lifecycle_operation(&state, &service_id, "stopping")?;
    let result = match claim {
        LifecycleClaimKind::Microvm(claim) => {
            let result = claim.runner.stop(&service_id).await;
            reap_children(claim.vm_child, claim.aux_processes).await;
            result
        }
        LifecycleClaimKind::Container { runner } => runner.stop(&service_id).await,
    };

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

async fn vm_update(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
    body: Option<Json<UpdateBody>>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    MicrovmRunner::validate_service_id(&service_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let body = body.map(|j| j.0).unwrap_or_default();
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let meta: serde_json::Value = if std::path::Path::new(&path).exists() {
        let content = std::fs::read_to_string(&path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read metadata: {e}"),
            )
        })?;
        serde_json::from_str(&content).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("parse metadata: {e}"),
            )
        })?
    } else {
        serde_json::json!({})
    };

    let repo_url = body
        .repo_url
        .or_else(|| {
            meta.get("repo_url")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "no repo_url in request or metadata; pass {\"repo_url\":\"...\"} or redeploy once first".into(),
            )
        })?;
    let config_path = body
        .config_path
        .or_else(|| {
            meta.get("config_path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "Russelfile.toml".into());

    let host_port = meta
        .get("host_port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16);
    let guest_port = meta
        .get("guest_port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(3000);
    let runtime = meta
        .get("runtime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());

    let request = DeployRequest {
        repo_url,
        config_path,
        vm_id: Some(service_id.clone()),
        port: host_port.map(|host| russel_core::api::PortMapping {
            host,
            guest: guest_port,
        }),
        runtime,
        env: Default::default(),
        podman_args: vec![],
    };

    tracing::info!(service_id = %service_id, "POST /vm/{}/update", service_id);

    // Bound concurrent deploys (same semaphore as POST /deploy).
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

    // Same NDJSON stream as POST /deploy so CLI reuses event parsing.
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
        let _ = deploy_tx
            .send(DeployEvent::Complete(Box::new(response)))
            .await;
        tracing::info!(service_id = %sid, "update deploy finished");
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
                .unwrap_or_else(|| "update deploy task panicked".to_string());
            tracing::error!(
                service_id = %sid2,
                panic = %detail,
                "update deploy task panicked"
            );
            monitor_state.mark_failed(&sid2, detail);
            let _ = tx
                .send(DeployEvent::Error("update deploy task failed".to_string()))
                .await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|msg| {
        let json = serde_json::to_string(&msg).map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("{}\n", json)))
    });

    axum::response::Response::builder()
        .header("Content-Type", "application/x-ndjson")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("build update stream: {e}"),
            )
        })
}

async fn vm_destroy(
    State(state): State<AppState>,
    Path(service_id): Path<String>,
) -> Result<Json<String>, (StatusCode, String)> {
    tracing::info!(service_id = %service_id, "DELETE /vm/{}", service_id);

    let (runtime, claim) = claim_lifecycle_operation(&state, &service_id, "destroying")?;
    let result = match claim {
        LifecycleClaimKind::Microvm(claim) => {
            let result = claim.runner.destroy(&service_id).await;
            reap_children(claim.vm_child, claim.aux_processes).await;
            result
        }
        LifecycleClaimKind::Container { runner } => {
            let result = runner.destroy(&service_id).await;
            if result.is_ok() {
                PortAllocator::release(&service_id);
                let base = crate::container::default_base_dir(&service_id);
                if base.exists() {
                    let _ = tokio::fs::remove_dir_all(&base).await;
                }
            }
            result
        }
    };

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

struct MicrovmLifecycleClaim {
    runner: MicrovmRunner,
    vm_child: Option<Child>,
    aux_processes: Vec<Child>,
}

enum LifecycleClaimKind {
    // Box large variant payload (clippy large_enum_variant).
    Microvm(Box<MicrovmLifecycleClaim>),
    Container { runner: ContainerRunner },
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
) -> Result<(RuntimeKind, LifecycleClaimKind), (StatusCode, String)> {
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

    let claim = match runtime {
        RuntimeKind::Microvm => LifecycleClaimKind::Microvm(Box::new(MicrovmLifecycleClaim {
            runner: MicrovmRunner::new(),
            vm_child,
            aux_processes,
        })),
        RuntimeKind::Container => LifecycleClaimKind::Container {
            runner: ContainerRunner::new(),
        },
    };

    Ok((runtime, claim))
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
    crate::secrets::set_secret(&name, &body.value)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

async fn secrets_delete(Path(name): Path<String>) -> Result<StatusCode, (StatusCode, String)> {
    match crate::secrets::delete_secret(&name) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
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
        assert_eq!(
            normalize_api_token(Some("secret")),
            Some("secret".to_string())
        );
        assert_eq!(
            normalize_api_token(Some("  secret  ")),
            Some("secret".to_string())
        );
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
}
