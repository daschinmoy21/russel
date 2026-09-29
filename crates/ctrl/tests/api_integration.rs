//! Control-plane HTTP API integration tests.
//!
//! These exercise the real Axum router + `AppState` over in-process requests
//! (`tower::ServiceExt::oneshot`). They do **not** boot Cloud Hypervisor or
//! Podman; full deploy e2e remains a manual / optional smoke path.
//!
//! Covers (parent #309):
//! - auth middleware (token required / wrong / correct)
//! - inventory: `/vms`, `/status`, `/logs`
//! - per-service status / stop / destroy not-found + reserved ids
//! - multi-service aggregate status/logs rejection
//! - secrets CRUD against an isolated secrets dir
//! - deploy request validation and NDJSON stream open

#![allow(clippy::unwrap_used, clippy::expect_used)]
// EnvGuard holds a std Mutex across `.await` so process-global RUSSEL_* env
// mutations stay serialized under parallel test threads. Prefer this over races.
#![allow(clippy::await_holding_lock)]

use std::sync::{Mutex, MutexGuard, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use russel_core::api::{ServiceStatus, VmState};
use russel_ctrl::api::router;
use russel_ctrl::state::AppState;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// Missing on purpose: `podman` calls fail as on a host without Podman.
const NO_PODMAN: &str = "/nonexistent/russel-test-podman";

/// Serializes every test in this binary and hands each one empty host roots.
///
/// The data and microVM roots are pinned once per process to a temp dir
/// (never `/var/lib/russel`), and Podman to a missing binary so `/vms` does
/// not list the host's real containers. Because the lock serializes tests,
/// clearing the roots here gives each test a clean slate.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    russel_core::paths::pin_temp_roots();
    russel_ctrl::container::pin_podman_program(NO_PODMAN);
    for root in [
        russel_ctrl::paths::data_root(),
        russel_ctrl::paths::microvms_root(),
    ] {
        clear_dir(&root);
    }
    guard
}

fn clear_dir(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).expect("read test root").flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path).expect("clear test root");
        } else {
            std::fs::remove_file(&path).expect("clear test root");
        }
    }
}

/// Holds the env lock and restores `RUSSEL_API_TOKEN` (+ optional secrets dir)
/// when dropped — safe across `.await` points in a single task.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    prev_token: Option<std::ffi::OsString>,
    prev_secrets: Option<std::ffi::OsString>,
    touched_secrets: bool,
}

impl EnvGuard {
    /// Clear API token (open auth) for the duration of the guard.
    fn open_auth() -> Self {
        let lock = env_lock();
        let prev_token = std::env::var_os("RUSSEL_API_TOKEN");
        // SAFETY: exclusive env_lock held for the guard lifetime.
        unsafe {
            std::env::remove_var("RUSSEL_API_TOKEN");
        }
        Self {
            _lock: lock,
            prev_token,
            prev_secrets: None,
            touched_secrets: false,
        }
    }

    /// Set a required API token for the duration of the guard.
    fn with_token(token: &str) -> Self {
        let lock = env_lock();
        let prev_token = std::env::var_os("RUSSEL_API_TOKEN");
        unsafe {
            std::env::set_var("RUSSEL_API_TOKEN", token);
        }
        Self {
            _lock: lock,
            prev_token,
            prev_secrets: None,
            touched_secrets: false,
        }
    }

    /// Open auth + isolated secrets directory.
    fn open_auth_with_secrets(dir: &std::path::Path) -> Self {
        let lock = env_lock();
        let prev_token = std::env::var_os("RUSSEL_API_TOKEN");
        let prev_secrets = std::env::var_os("RUSSEL_SECRETS_DIR");
        unsafe {
            std::env::remove_var("RUSSEL_API_TOKEN");
            std::env::set_var("RUSSEL_SECRETS_DIR", dir);
        }
        Self {
            _lock: lock,
            prev_token,
            prev_secrets,
            touched_secrets: true,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: still holds env_lock until `_lock` drops after this body.
        unsafe {
            match self.prev_token.take() {
                Some(v) => std::env::set_var("RUSSEL_API_TOKEN", v),
                None => std::env::remove_var("RUSSEL_API_TOKEN"),
            }
            if self.touched_secrets {
                match self.prev_secrets.take() {
                    Some(v) => std::env::set_var("RUSSEL_SECRETS_DIR", v),
                    None => std::env::remove_var("RUSSEL_SECRETS_DIR"),
                }
            }
        }
    }
}

fn app(state: AppState) -> axum::Router {
    router(state)
}

async fn body_bytes(res: axum::response::Response) -> Vec<u8> {
    res.into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = body_bytes(res).await;
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "expected JSON body, got {:?}: {e}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

async fn body_text(res: axum::response::Response) -> String {
    String::from_utf8_lossy(&body_bytes(res).await).into_owned()
}

async fn get(router: axum::Router, uri: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

async fn get_auth(router: axum::Router, uri: &str, token: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

async fn post_json(router: axum::Router, uri: &str, body: &Value) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

async fn post_json_auth(
    router: axum::Router,
    uri: &str,
    body: &Value,
    token: &str,
) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

async fn delete_req(router: axum::Router, uri: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot")
}

/// Seed a service that appears in inventory without a real runtime process.
fn seed_service(state: &AppState, id: &str) {
    state.mark_building(id).expect("mark_building");
    state.set_status(id, ServiceStatus::Deployed, VmState::Running);
}

const STRONG_TOKEN: &str = "integration-test-token-32chars!!"; // 32 chars

#[tokio::test]
async fn vms_list_empty_when_no_services() {
    let _env = EnvGuard::open_auth();
    let res = get(app(AppState::default()), "/vms").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["vms"], json!([]));
    if let Some(services) = body.get("services") {
        assert!(services.as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn state_writes_stay_in_temp_data_root() {
    let _env = EnvGuard::open_auth();
    let root = russel_ctrl::paths::data_root();
    assert!(
        root.starts_with(std::env::temp_dir()),
        "test data root must be a temp dir, got {}",
        root.display()
    );
    assert!(russel_ctrl::paths::microvms_root().starts_with(std::env::temp_dir()));

    let state = AppState::default();
    seed_service(&state, "catalog-probe");
    state.write_catalog().expect("write catalog");
    let catalog = std::fs::read_to_string(root.join("ctrl-catalog.json")).expect("catalog");
    assert!(catalog.contains("catalog-probe"), "catalog={catalog}");
}

#[tokio::test]
async fn vms_list_includes_microvm_marker_dirs_from_test_root() {
    let _env = EnvGuard::open_auth();
    std::fs::create_dir_all(russel_ctrl::paths::microvm_dir("marker-vm")).unwrap();

    let res = get(app(AppState::default()), "/vms").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["vms"], json!(["marker-vm"]));
}

#[tokio::test]
async fn vms_list_includes_seeded_service() {
    let _env = EnvGuard::open_auth();
    let state = AppState::default();
    seed_service(&state, "demo-svc");

    let res = get(app(state), "/vms").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let vms = body["vms"].as_array().expect("vms array");
    assert!(
        vms.iter().any(|v| v.as_str() == Some("demo-svc")),
        "expected demo-svc in {vms:?}"
    );
}

#[tokio::test]
async fn vms_list_skips_dir_with_only_kept_volumes() {
    let _env = EnvGuard::open_auth();
    // What `destroy` leaves behind for a `keep = true` volume.
    let kept = russel_ctrl::paths::service_dir("kept-svc").join("volumes/data");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join("db.sqlite"), b"data").unwrap();

    let state = AppState::default();
    let res = get(app(state.clone()), "/vms").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let vms = body["vms"].as_array().expect("vms array");
    assert!(
        !vms.iter().any(|v| v.as_str() == Some("kept-svc")),
        "kept volumes listed as a service: {vms:?}"
    );
    assert!(state.status("kept-svc").is_none());

    // No state entry, so a follow-up destroy is a 404 and the data stays.
    let res = app(state)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/vm/kept-svc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(kept.join("db.sqlite").is_file());
}

#[tokio::test]
async fn status_all_not_found_when_empty() {
    let _env = EnvGuard::open_auth();
    let res = get(app(AppState::default()), "/status").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let text = body_text(res).await;
    assert!(text.contains("no services"), "body={text}");
}

#[tokio::test]
async fn status_all_ok_for_single_service() {
    let _env = EnvGuard::open_auth();
    let state = AppState::default();
    seed_service(&state, "solo");

    let res = get(app(state), "/status").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["service_id"], "solo");
    assert_eq!(body["status"], "deployed");
}

#[tokio::test]
async fn status_all_rejects_multiple_services() {
    let _env = EnvGuard::open_auth();
    let state = AppState::default();
    seed_service(&state, "a");
    seed_service(&state, "b");

    let res = get(app(state), "/status").await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = body_text(res).await;
    assert!(
        text.contains("multiple services") && text.contains("service_id"),
        "body={text}"
    );
}

#[tokio::test]
async fn logs_all_rejects_multiple_services() {
    let _env = EnvGuard::open_auth();
    let state = AppState::default();
    seed_service(&state, "a");
    seed_service(&state, "b");

    let res = get(app(state), "/logs").await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = body_text(res).await;
    assert!(text.contains("multiple services"), "body={text}");
}

#[tokio::test]
async fn vm_status_not_found() {
    let _env = EnvGuard::open_auth();
    let res = get(app(AppState::default()), "/vm/missing-svc/status").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn vm_status_returns_seeded_service() {
    let _env = EnvGuard::open_auth();
    let state = AppState::default();
    seed_service(&state, "web");

    let res = get(app(state), "/vm/web/status").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["service_id"], "web");
    assert_eq!(body["status"], "deployed");
    assert_eq!(body["vm_state"], "running");
}

#[tokio::test]
async fn reserved_service_id_rejected_on_status() {
    let _env = EnvGuard::open_auth();
    let res = get(app(AppState::default()), "/vm/secrets/status").await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = body_text(res).await;
    assert!(
        text.contains("reserved") || text.contains("service_id"),
        "body={text}"
    );
}

#[tokio::test]
async fn reserved_service_id_rejected_on_stop() {
    let _env = EnvGuard::open_auth();
    // Empty JSON body — stop handler does not require a body, but POST with
    // no content-type is fine for axum when no Json extractor is used.
    let res = app(AppState::default())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vm/traefik/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = body_text(res).await;
    assert!(text.contains("reserved"), "body={text}");
}

#[tokio::test]
async fn stop_unknown_service_is_not_found() {
    let _env = EnvGuard::open_auth();
    let res = app(AppState::default())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vm/ghost/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn destroy_unknown_service_is_not_found() {
    let _env = EnvGuard::open_auth();
    let res = delete_req(app(AppState::default()), "/vm/ghost").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_required_when_token_configured() {
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let res = get(app(AppState::default()), "/vms").await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let res = get_auth(
        app(AppState::default()),
        "/vms",
        "wrong-token-wrong-token-wrong!!",
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_accepts_correct_bearer() {
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let state = AppState::default();
    seed_service(&state, "authed");

    let res = get_auth(app(state), "/vms", STRONG_TOKEN).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let vms = body["vms"].as_array().unwrap();
    assert!(vms.iter().any(|v| v.as_str() == Some("authed")));
}

#[tokio::test]
async fn auth_accepts_case_insensitive_bearer_scheme() {
    // RFC 7235: the auth scheme is case-insensitive. Lowercase "bearer " must
    // be accepted just like "Bearer ".
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let state = AppState::default();
    seed_service(&state, "authed");

    let res = app(state)
        .oneshot(
            Request::builder()
                .uri("/vms")
                .header(header::AUTHORIZATION, format!("bearer {STRONG_TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let vms = body["vms"].as_array().unwrap();
    assert!(vms.iter().any(|v| v.as_str() == Some("authed")));
}

#[tokio::test]
async fn secrets_list_set_delete_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let _env = EnvGuard::open_auth_with_secrets(tmp.path());
    let state = AppState::default();

    let res = get(app(state.clone()), "/secrets").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["secrets"], json!([]));

    let res = post_json(
        app(state.clone()),
        "/secrets/db_password",
        &json!({ "value": "s3cret" }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = get(app(state.clone()), "/secrets").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    let names = body["secrets"].as_array().unwrap();
    assert!(
        names.iter().any(|n| n.as_str() == Some("db_password")),
        "names={names:?}"
    );

    let res = delete_req(app(state.clone()), "/secrets/db_password").await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = delete_req(app(state), "/secrets/db_password").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn secrets_reject_invalid_name() {
    let tmp = TempDir::new().unwrap();
    let _env = EnvGuard::open_auth_with_secrets(tmp.path());

    // Leading '-' is URI-safe but rejected by validate_secret_name.
    let res = post_json(
        app(AppState::default()),
        "/secrets/-bad-name",
        &json!({ "value": "x" }),
    )
    .await;

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let text = body_text(res).await;
    assert!(
        text.contains("secret name") || text.contains("must not start"),
        "body={text}"
    );
}

#[tokio::test]
async fn deploy_rejects_malformed_json() {
    let _env = EnvGuard::open_auth();
    let res = app(AppState::default())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/deploy")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not-json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        res.status() == StatusCode::BAD_REQUEST
            || res.status() == StatusCode::UNPROCESSABLE_ENTITY
            || res.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "status={}",
        res.status()
    );
}

#[tokio::test]
async fn deploy_requires_auth_when_token_set() {
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let res = post_json(
        app(AppState::default()),
        "/deploy",
        &json!({
            "repo_url": "https://example.com/app.git",
            "config_path": "Russelfile.toml",
            "vm_id": "nope"
        }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn deploy_with_auth_accepts_request_and_streams_ndjson() {
    // Does not require a successful deploy — only that the control plane
    // accepts the request and opens an NDJSON stream. Use a loopback git URL
    // so SSRF validation fails immediately (no DNS / outbound network).
    // Pipeline still emits Progress + Complete NDJSON events.
    let _env = EnvGuard::with_token(STRONG_TOKEN);
    let res = post_json_auth(
        app(AppState::default()),
        "/deploy",
        &json!({
            "repo_url": "https://127.0.0.1/does-not-exist.git",
            "config_path": "Russelfile.toml",
            "vm_id": "integ-deploy"
        }),
        STRONG_TOKEN,
    )
    .await;

    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("ndjson") || ct.contains("json"),
        "content-type={ct}"
    );

    let text = tokio::time::timeout(std::time::Duration::from_secs(5), body_text(res))
        .await
        .expect("deploy stream body should finish quickly without network I/O");
    assert!(
        !text.trim().is_empty(),
        "expected NDJSON events, got empty body"
    );
    let mut saw_json = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if serde_json::from_str::<Value>(line).is_ok() {
            saw_json = true;
            break;
        }
    }
    assert!(
        saw_json,
        "expected at least one JSON event line in:\n{text}"
    );
}
