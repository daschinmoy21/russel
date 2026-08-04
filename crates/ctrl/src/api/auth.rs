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

/// Constant-time token comparison to avoid timing side-channels.
///
/// Always walks `max(a.len(), b.len())` bytes so the result does not leak the
/// input lengths. A length mismatch is folded into the accumulator as a
/// non-zero delta rather than returned early.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

    let provided = header.strip_prefix("Bearer ").unwrap_or("");

    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}
