use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use russel_core::config::RuntimeKind;
use tower::ServiceExt;

use super::*;
use crate::metadata::resolve_lifecycle_runtime;
use crate::state::AppState;

#[test]
fn unauthorized_response_identifies_control_plane() {
    // Local probes (install.sh host/connect/status) treat HTTP 401 as a
    // Russel endpoint only when this marker is present, so an unrelated
    // listener on 127.0.0.1:7878 cannot pass compatibility.
    let res = super::auth::unauthorized_response();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = res
        .headers()
        .get(axum::http::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        header.contains("Bearer") && header.contains("russel"),
        "unexpected WWW-Authenticate: {header}"
    );
}

#[test]
fn require_auth_from_env_truthy() {
    assert!(!require_auth_from_env(None));
    assert!(!require_auth_from_env(Some("")));
    assert!(!require_auth_from_env(Some("0")));
    assert!(!require_auth_from_env(Some("false")));
    assert!(!require_auth_from_env(Some("no")));
    assert!(!require_auth_from_env(Some("off")));
    assert!(!require_auth_from_env(Some("disabled")));
    assert!(require_auth_from_env(Some("1")));
    assert!(require_auth_from_env(Some("true")));
    assert!(require_auth_from_env(Some("YES")));
    assert!(require_auth_from_env(Some(" True ")));
    assert!(require_auth_from_env(Some("on")));
    assert!(require_auth_from_env(Some("ON")));
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

#[test]
fn resolve_lifecycle_runtime_uses_state_over_disk_default() {
    assert_eq!(
        resolve_lifecycle_runtime(Some(RuntimeKind::Container), "missing").unwrap(),
        RuntimeKind::Container
    );
    assert_eq!(
        resolve_lifecycle_runtime(None, "missing").unwrap(),
        RuntimeKind::Microvm
    );
}

/// POST /deploy helper that does **not** mutate process environment.
///
/// When `RUSSEL_API_TOKEN` is set (common in CI shells), attach a matching
/// Bearer header so auth middleware passes. Clearing/restoring env across an
/// `await` races concurrent tests that also touch process-wide environment.
async fn post_deploy(body: &str) -> (StatusCode, String) {
    let app = router(AppState::default());
    let mut builder = Request::builder()
        .method("POST")
        .uri("/deploy")
        .header("content-type", "application/json");

    // Present the configured token when set — no env clear/restore.
    if let Some(token) = normalize_api_token(std::env::var("RUSSEL_API_TOKEN").ok().as_deref()) {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }

    let req = builder
        .body(Body::from(body.to_string()))
        .expect("build request");
    let res = app.oneshot(req).await.expect("oneshot");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (status, text)
}

/// #447: the Russelfile is the whole desired state. Former override fields
/// are rejected by the JSON extractor, before any deploy pipeline starts.
#[tokio::test]
async fn deploy_rejects_removed_override_fields() {
    for field in [
        r#""port":{"host":8080,"guest":3000}"#,
        r#""host":"api.example.com""#,
        r#""runtime":"container""#,
        r#""env":{"A":"1"}"#,
        r#""podman_args":["-v","/a:/b"]"#,
    ] {
        let (status, body) = post_deploy(&format!(
            r#"{{"repo_url":"https://example.com/org/app.git","config_path":"Russelfile.toml","vm_id":"app",{field}}}"#
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field}: body={body}"
        );
        assert!(body.contains("unknown field"), "{field}: body={body}");
    }
}

fn mini_dashboard() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<html>overview</html>").unwrap();
    std::fs::create_dir(dir.path().join("deploy")).unwrap();
    std::fs::write(
        dir.path().join("deploy/index.html"),
        "<html>deploy-page</html>",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("settings")).unwrap();
    std::fs::write(
        dir.path().join("settings/index.html"),
        "<html>settings</html>",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("_astro")).unwrap();
    std::fs::write(dir.path().join("_astro/app.js"), "console.log(1)").unwrap();
    std::fs::write(dir.path().join("favicon.svg"), "<svg></svg>").unwrap();
    dir
}

async fn get_path(app: axum::Router, uri: &str) -> (StatusCode, String, Option<String>) {
    let res = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let ctype = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned(), ctype)
}

#[tokio::test]
async fn dashboard_get_pages_are_public() {
    let dist = mini_dashboard();
    let app = router_with_dashboard(AppState::default(), dist.path());
    let (status, body, ctype) = get_path(app, "/").await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(body.contains("overview"), "body={body}");
    assert!(
        ctype.as_deref().is_some_and(|c| c.starts_with("text/html")),
        "ctype={ctype:?}"
    );

    let app = router_with_dashboard(AppState::default(), dist.path());
    let (status, body, _) = get_path(app, "/deploy").await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(body.contains("deploy-page"), "body={body}");

    let app = router_with_dashboard(AppState::default(), dist.path());
    let (status, body, ctype) = get_path(app, "/_astro/app.js").await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(body.contains("console.log"), "body={body}");
    assert!(
        ctype
            .as_deref()
            .is_some_and(|c| c.contains("javascript") || c.contains("text/plain")),
        "ctype={ctype:?}"
    );
}

#[tokio::test]
async fn dashboard_post_deploy_stays_the_api() {
    let dist = mini_dashboard();
    let app = router_with_dashboard(AppState::default(), dist.path());
    let mut builder = Request::builder()
        .method("POST")
        .uri("/deploy")
        .header("content-type", "application/json");
    if let Some(token) = normalize_api_token(std::env::var("RUSSEL_API_TOKEN").ok().as_deref()) {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let res = app
        .oneshot(
            builder
                .body(Body::from(
                    r#"{"repo_url":"https://example.com/org/app.git","config_path":"Russelfile.toml","runtime":"container"}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    // The API's JSON extractor answered, not the static dashboard.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert!(body.contains("unknown field"), "body={body}");
}

#[tokio::test]
async fn dashboard_nests_api_prefix() {
    let dist = mini_dashboard();
    let app = router_with_dashboard(AppState::default(), dist.path());
    let mut builder = Request::builder().uri("/api/vms");
    if let Some(token) = normalize_api_token(std::env::var("RUSSEL_API_TOKEN").ok().as_deref()) {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let res = app
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
}
