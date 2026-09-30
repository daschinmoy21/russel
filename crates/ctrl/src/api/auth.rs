//! API token auth helpers, middleware, and deploy concurrency limits.

use axum::{
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// `RUSSEL_API_TOKEN` rules, shared with the CLI and agent: unset/blank means
/// none, and a set token must be at least [`MIN_API_TOKEN_LEN`] (32)
/// printable-ASCII chars. `main` runs the check at startup whenever a token
/// is set, so a short or non-header-safe secret fails closed; the middleware
/// uses the env token as-is. Prefer `openssl rand -hex 32` for production.
pub use russel_core::tokens::{
    MIN_TOKEN_LEN as MIN_API_TOKEN_LEN, check_token_min_length as check_api_token_min_length,
    normalize_token as normalize_api_token,
};

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
///
/// Rejections carry `WWW-Authenticate: Bearer realm="russel-ctrl"` so local
/// probes (`install.sh host/connect/status`) can tell this control plane
/// apart from an unrelated listener that also returns HTTP 401.
pub(super) async fn auth_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    // Dashboard HTML/JS is public; the client sends Bearer on `/api/*`.
    // Only skip when a dashboard dist is actually mounted (Extension present).
    if crate::dashboard::is_public_dashboard_request(request.method(), request.uri().path())
        && request
            .extensions()
            .get::<crate::dashboard::DashboardDir>()
            .is_some()
    {
        return next.run(request).await;
    }

    let Some(expected) = configured_api_token() else {
        return next.run(request).await;
    };

    let header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided = russel_core::tokens::bearer_token_from_header(header).unwrap_or("");

    if !russel_core::tokens::constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return unauthorized_response();
    }

    next.run(request).await
}

/// Identifiable 401 for Bearer rejections (see [`auth_middleware`]).
pub(super) fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            "Bearer realm=\"russel-ctrl\"",
        )],
        "unauthorized\n",
    )
        .into_response()
}
