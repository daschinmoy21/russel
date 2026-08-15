//! Agent HTTP routes (`/agent/v1/*`).
//!
//! Phase 1 (#213): heartbeat + health are live. Phase 2 (#214): stop / destroy /
//! status are real — they reuse `russel-ctrl`'s in-process runners after
//! resolving the runtime kind from on-disk metadata. Deploy boot and logs are
//! still 501 (documented follow-ups).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use russel_core::api::{
    AgentErrorResponse, AgentHeartbeat, AgentLifecycleResponse, AgentNodeStatus,
    AgentNotImplemented, AgentStatusResponse, NodeCapacity,
};

use crate::capacity::{collect_capacity, parse_node_labels};
use crate::lifecycle;

/// Shared agent process state.
#[derive(Clone)]
pub struct AgentState {
    pub node_id: String,
    pub data_root: PathBuf,
    /// Skip `podman info` in unit tests. Production always probes — no prod
    /// code path sets this (test-only flag, not yet wired to an env/config).
    pub probe_podman: bool,
    pub labels: HashMap<String, String>,
    pub agent_version: String,
    /// When true, report NotReady (drain / maintenance). Reserved for the
    /// drain-mode design — no production code path sets it yet.
    pub not_ready: bool,
}

impl AgentState {
    pub fn new(node_id: String, data_root: PathBuf) -> Self {
        let labels = parse_node_labels(std::env::var("RUSSEL_NODE_LABELS").ok().as_deref());
        Self {
            node_id,
            data_root,
            probe_podman: true,
            labels,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            not_ready: false,
        }
    }
}

/// Build the `/agent/v1` router (no auth layer — applied by caller).
pub fn agent_router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/agent/v1/health", get(health))
        .route("/agent/v1/heartbeat", get(heartbeat))
        // Runtime lifecycle (#214): stop/destroy/status live on the agent;
        // deploy boot + logs remain 501 (documented follow-ups).
        .route("/agent/v1/deploy", post(deploy_not_implemented))
        .route("/agent/v1/stop/{service_id}", post(stop_handler))
        .route("/agent/v1/destroy/{service_id}", post(destroy_handler))
        .route("/agent/v1/status/{service_id}", get(status_handler))
        .route("/agent/v1/logs/{service_id}", get(logs_not_implemented))
        .with_state(state)
}

async fn health(State(state): State<Arc<AgentState>>) -> impl IntoResponse {
    let status = if state.not_ready { "not_ready" } else { "ok" };
    Json(serde_json::json!({
        "status": status,
        "node_id": state.node_id,
        "version": state.agent_version,
    }))
}

async fn heartbeat(State(state): State<Arc<AgentState>>) -> impl IntoResponse {
    let capacity = collect_capacity(&state.data_root, state.probe_podman).await;
    let body = build_heartbeat(&state, capacity, now_rfc3339());
    Json(body)
}

/// Pure helper for tests: assemble heartbeat from fixed capacity + timestamp.
pub fn build_heartbeat(
    state: &AgentState,
    capacity: NodeCapacity,
    timestamp: String,
) -> AgentHeartbeat {
    AgentHeartbeat {
        node_id: state.node_id.clone(),
        timestamp,
        status: if state.not_ready {
            AgentNodeStatus::NotReady
        } else {
            AgentNodeStatus::Ready
        },
        capacity,
        labels: state.labels.clone(),
        agent_version: Some(state.agent_version.clone()),
    }
}

fn now_rfc3339() -> String {
    // Avoid chrono dependency: format UNIX seconds as approximate UTC.
    // Good enough for ops visibility; Phase 3 can switch to a time crate.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Manual RFC3339 without chrono: use a minimal formatter via humantime-free path.
    // We only need a stable sortable timestamp for heartbeat freshness.
    format_unix_secs_rfc3339(secs)
}

/// Format UNIX seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn format_unix_secs_rfc3339(secs: u64) -> String {
    // Civil date from days since epoch (Howard Hinnant algorithm, public domain).
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;

    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Days since Unix epoch → (year, month, day) UTC.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

async fn deploy_not_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(AgentNotImplemented {
            error: "agent deploy boot is not implemented yet — control plane still boots \
                     workloads in-process; stop/destroy/status are live (#214)"
                .into(),
            phase: "horiz/p1-02 (#214)".into(),
        }),
    )
}

async fn logs_not_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(AgentNotImplemented {
            error: "agent logs are not implemented yet (#214 follow-up)".into(),
            phase: "horiz/p1-02 (#214)".into(),
        }),
    )
}

/// Map a lifecycle error to a structured `{"error": …}` response.
fn lifecycle_error_response(
    err: lifecycle::LifecycleError,
) -> (StatusCode, Json<AgentErrorResponse>) {
    let status = err.status;
    (status, Json(AgentErrorResponse { error: err.message }))
}

async fn stop_handler(
    State(state): State<Arc<AgentState>>,
    Path(service_id): Path<String>,
) -> Result<Json<AgentLifecycleResponse>, (StatusCode, Json<AgentErrorResponse>)> {
    lifecycle::stop(&state.data_root, &service_id)
        .await
        .map(Json)
        .map_err(lifecycle_error_response)
}

async fn destroy_handler(
    State(state): State<Arc<AgentState>>,
    Path(service_id): Path<String>,
) -> Result<Json<AgentLifecycleResponse>, (StatusCode, Json<AgentErrorResponse>)> {
    lifecycle::destroy(&state.data_root, &service_id)
        .await
        .map(Json)
        .map_err(lifecycle_error_response)
}

async fn status_handler(
    State(state): State<Arc<AgentState>>,
    Path(service_id): Path<String>,
) -> Result<Json<AgentStatusResponse>, (StatusCode, Json<AgentErrorResponse>)> {
    lifecycle::status(&state.data_root, &service_id)
        .await
        .map(Json)
        .map_err(lifecycle_error_response)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::auth::{check_token_min_length, require_bearer};
    use axum::body::Body;
    use axum::http::{Request, header};
    use axum::middleware;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> Arc<AgentState> {
        let dir = std::env::temp_dir().join(format!(
            "russel-agent-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AgentState {
            node_id: "test-node".into(),
            data_root: dir,
            probe_podman: false,
            labels: HashMap::from([("env".into(), "test".into())]),
            agent_version: "0.1.0-test".into(),
            not_ready: false,
        })
    }

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_ok() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["status"], "ok");
        assert_eq!(v["node_id"], "test-node");
    }

    #[tokio::test]
    async fn heartbeat_returns_capacity_shape() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/heartbeat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["node_id"], "test-node");
        assert_eq!(v["status"], "ready");
        assert!(v["capacity"]["cpus_total"].as_u64().unwrap() >= 1);
        assert_eq!(v["labels"]["env"], "test");
        assert_eq!(v["agent_version"], "0.1.0-test");
        // Timestamp looks like RFC3339 UTC.
        let ts = v["timestamp"].as_str().unwrap();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[tokio::test]
    async fn deploy_returns_501() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/v1/deploy")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        let v = body_json(res).await;
        assert!(v["phase"].as_str().unwrap().contains("214"));
    }

    #[tokio::test]
    async fn stop_unknown_service_returns_404() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/v1/stop/svc-unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let v = body_json(res).await;
        assert!(v["error"].as_str().unwrap().contains("svc-unknown"));
    }

    #[tokio::test]
    async fn stop_invalid_service_id_returns_400() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/v1/stop/..%2Fetc%2Fpasswd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn destroy_unknown_service_returns_404() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/v1/destroy/svc-unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_unknown_service_returns_404() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/status/svc-unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_reports_stopped_for_service_without_live_process() {
        // Metadata exists in the temp data root, but no podman container /
        // live pid backs it → the agent reports "stopped" with a 200.
        let state = test_state();
        std::fs::create_dir_all(state.data_root.join("svc-c")).unwrap();
        std::fs::write(
            state.data_root.join("svc-c/metadata.json"),
            r#"{"service_id":"svc-c","runtime":"container","host_port":8080,"guest_port":3000}"#,
        )
        .unwrap();
        let app = agent_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/status/svc-c")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["status"], "stopped");
        assert_eq!(v["runtime"], "container");
        assert_eq!(v["host_port"], 8080);
    }

    #[tokio::test]
    async fn logs_returns_501() {
        let app = agent_router(test_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/logs/svc-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn auth_middleware_rejects_missing_token() {
        let token = "a".repeat(32);
        check_token_min_length(&token).unwrap();
        let expected = Some(token.clone());
        let app = agent_router(test_state()).layer(middleware::from_fn(move |req, next| {
            let expected = expected.clone();
            async move { require_bearer(req, next, expected).await }
        }));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_accepts_valid_bearer() {
        let token = "b".repeat(32);
        let expected = Some(token.clone());
        let app = agent_router(test_state()).layer(middleware::from_fn(move |req, next| {
            let expected = expected.clone();
            async move { require_bearer(req, next, expected).await }
        }));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/agent/v1/heartbeat")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn format_unix_epoch() {
        // 0 → 1970-01-01T00:00:00Z
        assert_eq!(format_unix_secs_rfc3339(0), "1970-01-01T00:00:00Z");
        // 1_700_000_000 → 2023-11-14T22:13:20Z
        assert_eq!(
            format_unix_secs_rfc3339(1_700_000_000),
            "2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn build_heartbeat_respects_not_ready() {
        let mut st = AgentState::new("n".into(), PathBuf::from("/tmp"));
        st.not_ready = true;
        st.probe_podman = false;
        let hb = build_heartbeat(
            &st,
            NodeCapacity {
                cpus_total: 2,
                ..Default::default()
            },
            "t".into(),
        );
        assert_eq!(hb.status, AgentNodeStatus::NotReady);
        assert_eq!(hb.node_id, "n");
    }
}
