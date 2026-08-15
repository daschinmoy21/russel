//! API token auth helpers, middleware, and deploy concurrency limits.

use axum::{
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};

/// Minimum accepted length for `RUSSEL_API_TOKEN` after trim (when set).
///
/// Floor is 32 **ASCII** characters so weak tokens like `"a"` are rejected.
/// Prefer `openssl rand -hex 32` (64 hex chars / 256 bits) for production.
///
/// Length is measured in bytes/`str::len`, which matches character count only
/// because non-ASCII tokens are rejected (see [`check_api_token_min_length`]).
pub const MIN_API_TOKEN_LEN: usize = russel_core::tokens::MIN_TOKEN_LEN;

/// Pure token normalize: unset/blank/whitespace → None.
///
/// Does **not** enforce min length / charset — call [`check_api_token_min_length`]
/// at startup when a token is present so short or non-header-safe secrets fail closed.
pub fn normalize_api_token(raw: Option<&str>) -> Option<String> {
    russel_core::tokens::normalize_token(raw)
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
    russel_core::tokens::check_token_min_length(token)
}

/// Truthy parse for `RUSSEL_REQUIRE_AUTH`: `1`, `true`, `yes`, or `on`
/// (case-insensitive).
///
/// When enabled, the control plane refuses to start without a valid token even
/// on loopback — use for production packaging that would otherwise default to
/// loopback bind.
pub fn require_auth_from_env(raw: Option<&str>) -> bool {
    russel_core::env_util::env_bool(raw).unwrap_or(false)
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
pub(super) fn max_concurrent_deploys() -> usize {
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

/// Bearer auth middleware: if RUSSEL_API_TOKEN is set (non-empty, trimmed),
/// require it on every request.
///
/// Env: `RUSSEL_API_TOKEN` (min length enforced at process start), optional
/// `RUSSEL_REQUIRE_AUTH=1|true|yes` to fail closed without a token on loopback.
pub(super) async fn auth_middleware(
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

    let provided = russel_core::tokens::bearer_token_from_header(header).unwrap_or("");

    if !russel_core::tokens::constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}
