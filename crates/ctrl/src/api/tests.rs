use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use russel_core::config::RuntimeKind;
use tower::ServiceExt;

use super::auth::constant_time_eq;
use super::router::runtime_label;
use super::*;
use crate::metadata::resolve_lifecycle_runtime;
use crate::state::AppState;

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

// ── POST /deploy vm_id contract (#300) ─────────────────────────────

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

#[tokio::test]
async fn deploy_missing_vm_id_returns_400() {
    let (status, body) = post_deploy(
        r#"{"repo_url":"https://example.com/org/app.git","config_path":"Russelfile.toml"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert!(
        body.contains("vm_id is required"),
        "expected vm_id contract message, got: {body}"
    );
    // Must not start an NDJSON deploy stream.
    assert!(
        !body.contains("application/x-ndjson") && !body.starts_with('{'),
        "must not invoke deploy pipeline; body={body}"
    );
}

#[tokio::test]
async fn deploy_whitespace_only_vm_id_returns_400() {
    let (status, body) = post_deploy(
        r#"{"repo_url":"https://example.com/org/app.git","config_path":"Russelfile.toml","vm_id":"   "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert!(
        body.contains("vm_id is required"),
        "expected vm_id contract message, got: {body}"
    );
}

#[tokio::test]
async fn deploy_null_vm_id_returns_400() {
    let (status, body) = post_deploy(
        r#"{"repo_url":"https://example.com/org/app.git","config_path":"Russelfile.toml","vm_id":null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={body}");
    assert!(body.contains("vm_id is required"), "body={body}");
}
