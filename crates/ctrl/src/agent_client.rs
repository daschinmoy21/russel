//! ctrl → agent HTTP RPC client (#214).
//!
//! When `RUSSEL_AGENT_URL` is set, lifecycle stop/destroy (and status) are
//! issued to the worker agent over HTTP instead of the in-process runners.
//! Default (env unset) keeps the monolithic in-process path unchanged — no
//! single-node regression.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use russel_core::api::{AgentErrorResponse, AgentLifecycleResponse, AgentStatusResponse};

use crate::runtime::RuntimeLifecycle;

/// Env var that enables the agent RPC path (base URL, e.g. `http://127.0.0.1:7946`).
pub const AGENT_URL_ENV: &str = "RUSSEL_AGENT_URL";
/// Agent bearer token env (preferred over the API fallback).
pub const AGENT_TOKEN_ENV: &str = "RUSSEL_AGENT_TOKEN";
/// Fallback bearer token env — shared with the agent server's own auth.
pub const API_TOKEN_ENV: &str = "RUSSEL_API_TOKEN";

/// Shared HTTP client (connection pooling; built once per process).
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Teardown can legitimately take a while (podman stop grace period,
        // CH API shutdown + process reaping). 120s covers slow hosts.
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to build agent RPC HTTP client; using default");
            reqwest::Client::new()
        })
});

/// Trim + filter blank env values (same normalization as the agent server).
fn normalize(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve the agent base URL from env (unset/blank → `None`).
pub fn agent_url_from_env() -> Option<String> {
    normalize(std::env::var(AGENT_URL_ENV).ok().as_deref())
}

/// Resolve the bearer token ctrl sends to the agent:
/// `RUSSEL_AGENT_TOKEN` first, then `RUSSEL_API_TOKEN` (same as the agent).
pub fn agent_token_from_env() -> Option<String> {
    token_from_envs(
        std::env::var(AGENT_TOKEN_ENV).ok().as_deref(),
        std::env::var(API_TOKEN_ENV).ok().as_deref(),
    )
}

/// Pure token resolution helper (unit-tested; no env access).
pub fn token_from_envs(agent: Option<&str>, api: Option<&str>) -> Option<String> {
    normalize(agent).or_else(|| normalize(api))
}

/// True when ctrl should route lifecycle operations through the worker agent.
pub fn agent_mode_enabled() -> bool {
    agent_url_from_env().is_some()
}

/// Structured RPC failure carrying the HTTP status for API mapping.
#[derive(Debug)]
pub struct AgentRpcError {
    pub status: StatusCode,
    pub message: String,
}

impl AgentRpcError {
    /// Build from a non-2xx response body. Agent errors are `{"error": "…"}`;
    /// fall back to the raw body text when it is not that shape.
    fn from_response(status: StatusCode, body: &str) -> Self {
        let message = serde_json::from_str::<AgentErrorResponse>(body)
            .map(|e| e.error)
            .unwrap_or_else(|_| {
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    status.to_string()
                } else {
                    trimmed.to_string()
                }
            });
        Self { status, message }
    }
}

/// Agent RPC client. `stop` / `destroy` implement [`RuntimeLifecycle`]; `status`
/// is an extra read method (not part of the teardown seam).
#[derive(Clone)]
pub struct AgentClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl AgentClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            http: HTTP.clone(),
        }
    }

    /// Build from env when `RUSSEL_AGENT_URL` is set, else `None`.
    pub fn from_env() -> Option<Self> {
        let base_url = agent_url_from_env()?;
        Some(Self::new(base_url, agent_token_from_env()))
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn post(
        &self,
        service_id: &str,
        operation: &str,
    ) -> Result<AgentLifecycleResponse, AgentRpcError> {
        let url = format!("{}/agent/v1/{}/{}", self.base_url, operation, service_id);
        let mut req = self.http.post(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| AgentRpcError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("agent RPC failed ({operation} {service_id}): {e}"),
        })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AgentRpcError::from_response(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| AgentRpcError {
            status,
            message: format!("invalid agent response for {operation} {service_id}: {e}"),
        })
    }

    /// `POST /agent/v1/stop/{service_id}`.
    pub async fn stop(&self, service_id: &str) -> Result<AgentLifecycleResponse, AgentRpcError> {
        self.post(service_id, "stop").await
    }

    /// `POST /agent/v1/destroy/{service_id}`.
    pub async fn destroy(&self, service_id: &str) -> Result<AgentLifecycleResponse, AgentRpcError> {
        self.post(service_id, "destroy").await
    }

    /// `GET /agent/v1/status/{service_id}`.
    pub async fn status(&self, service_id: &str) -> Result<AgentStatusResponse, AgentRpcError> {
        let url = format!("{}/agent/v1/status/{}", self.base_url, service_id);
        let mut req = self.http.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| AgentRpcError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("agent RPC failed (status {service_id}): {e}"),
        })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AgentRpcError::from_response(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| AgentRpcError {
            status,
            message: format!("invalid agent response for status {service_id}: {e}"),
        })
    }
}

#[async_trait]
impl RuntimeLifecycle for AgentClient {
    async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        self.stop(service_id)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("agent stop failed: {}", e.message))
    }

    async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        self.destroy(service_id)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("agent destroy failed: {}", e.message))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::Path,
        http::{HeaderMap, header},
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// In-process mock agent: records Authorization headers, serves canned
    /// lifecycle/status responses, and can answer 404 with an error body.
    async fn spawn_mock_agent(seen_auth: Arc<Mutex<Vec<String>>>, not_found: bool) -> String {
        let app = Router::new()
            .route(
                "/agent/v1/stop/{service_id}",
                post({
                    let seen_auth = seen_auth.clone();
                    move |Path(id): Path<String>, headers: HeaderMap| {
                        let seen_auth = seen_auth.clone();
                        async move {
                            record_auth(&seen_auth, &headers);
                            if not_found {
                                return (
                                    StatusCode::NOT_FOUND,
                                    Json(serde_json::json!({
                                        "error": format!("service {id} not found"),
                                    })),
                                );
                            }
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "service_id": id.clone(),
                                    "operation": "stop",
                                    "status": "stopped",
                                    "message": format!("stopped container {id}"),
                                    "runtime": "container",
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/agent/v1/destroy/{service_id}",
                post({
                    let seen_auth = seen_auth.clone();
                    move |Path(id): Path<String>, headers: HeaderMap| {
                        let seen_auth = seen_auth.clone();
                        async move {
                            record_auth(&seen_auth, &headers);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "service_id": id.clone(),
                                    "operation": "destroy",
                                    "status": "destroyed",
                                    "message": format!("destroyed container {id}"),
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/agent/v1/status/{service_id}",
                get({
                    let seen_auth = seen_auth.clone();
                    move |Path(id): Path<String>, headers: HeaderMap| {
                        let seen_auth = seen_auth.clone();
                        async move {
                            record_auth(&seen_auth, &headers);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "service_id": id.clone(),
                                    "status": "running",
                                    "vm_state": "running",
                                    "uptime_seconds": 12,
                                    "runtime": "microvm",
                                    "host_port": 8080,
                                    "guest_port": 3000,
                                })),
                            )
                        }
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn record_auth(seen: &Arc<Mutex<Vec<String>>>, headers: &HeaderMap) {
        let value = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string();
        seen.lock().unwrap().push(value);
    }

    #[tokio::test]
    async fn stop_sends_bearer_and_parses_response() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_agent(seen.clone(), false).await;
        let client = AgentClient::new(base, Some("secret-token-123".into()));

        let resp = client.stop("svc-a").await.unwrap();
        assert_eq!(resp.status, "stopped");
        assert_eq!(resp.service_id, "svc-a");

        let auth = seen.lock().unwrap().clone();
        assert_eq!(auth, vec!["Bearer secret-token-123".to_string()]);
    }

    #[tokio::test]
    async fn destroy_and_status_hit_correct_routes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_agent(seen.clone(), false).await;
        let client = AgentClient::new(base, None);

        let destroyed = client.destroy("svc-b").await.unwrap();
        assert_eq!(destroyed.operation, "destroy");
        assert_eq!(destroyed.status, "destroyed");

        let status = client.status("svc-c").await.unwrap();
        assert_eq!(status.status, "running");
        assert_eq!(status.host_port, Some(8080));

        let auths = seen.lock().unwrap().clone();
        // No token configured → requests go out without an Authorization header.
        assert!(auths.iter().all(|a| a == "<none>"));
        assert_eq!(auths.len(), 2);
    }

    #[tokio::test]
    async fn not_found_error_is_mapped_with_status() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_agent(seen.clone(), true).await;
        let client = AgentClient::new(base, None);

        let err = client.stop("missing").await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.message, "service missing not found");
    }

    #[tokio::test]
    async fn connect_failure_maps_to_bad_gateway() {
        let client = AgentClient::new("http://127.0.0.1:1".into(), None);
        let err = client.status("svc").await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn token_resolution_prefers_agent_token() {
        assert_eq!(
            token_from_envs(Some(" a-token "), Some("api-token")),
            Some("a-token".into())
        );
        assert_eq!(
            token_from_envs(Some("   "), Some("api-token")),
            Some("api-token".into())
        );
        assert_eq!(token_from_envs(None, None), None);
    }

    #[test]
    fn base_url_trimmed_of_trailing_slash() {
        let client = AgentClient::new("http://127.0.0.1:7946/".into(), None);
        assert_eq!(client.base_url(), "http://127.0.0.1:7946");
    }
}
