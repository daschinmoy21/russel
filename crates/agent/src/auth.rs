//! Agent node-token auth (PSK first; mTLS later per horizontal plan).
//!
//! Mirrors control-plane Bearer rules: min 32 printable ASCII chars when set.
//! Env: `RUSSEL_AGENT_TOKEN` (preferred) or fallback `RUSSEL_API_TOKEN` so a
//! single-node split can share one secret during Phase 1.

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Minimum accepted length for agent / API tokens after trim.
pub const MIN_TOKEN_LEN: usize = 32;

/// Env name for the agent node token.
pub const AGENT_TOKEN_ENV: &str = "RUSSEL_AGENT_TOKEN";

/// Fallback shared with the control plane.
pub const API_TOKEN_ENV: &str = "RUSSEL_API_TOKEN";

/// Normalize: unset/blank → None.
pub fn normalize_token(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn token_is_http_header_safe(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
}

/// Reject tokens that are too short or cannot be sent as Bearer.
pub fn check_token_min_length(token: &str) -> Result<(), String> {
    if !token_is_http_header_safe(token) {
        return Err(
            "agent token must be printable ASCII only so it can be sent in an \
             HTTP Authorization header. Generate with: openssl rand -hex 32"
                .to_string(),
        );
    }
    if token.len() < MIN_TOKEN_LEN {
        Err(format!(
            "agent token must be at least {MIN_TOKEN_LEN} characters after trim (got {}). \
             Generate with: openssl rand -hex 32",
            token.len()
        ))
    } else {
        Ok(())
    }
}

/// Resolve configured token: `RUSSEL_AGENT_TOKEN` then `RUSSEL_API_TOKEN`.
pub fn configured_agent_token() -> Option<String> {
    resolve_token(
        std::env::var(AGENT_TOKEN_ENV).ok().as_deref(),
        std::env::var(API_TOKEN_ENV).ok().as_deref(),
    )
}

/// Resolve token from explicit values (tests / injectable startup).
pub fn resolve_token(agent: Option<&str>, api_fallback: Option<&str>) -> Option<String> {
    normalize_token(agent).or_else(|| normalize_token(api_fallback))
}

/// Constant-time-ish equality for Bearer secrets (length leak is acceptable).
fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Extract Bearer token from Authorization header.
pub fn bearer_from_request(req: &Request) -> Option<&str> {
    let val = req.headers().get(axum::http::header::AUTHORIZATION)?;
    let s = val.to_str().ok()?;
    let rest = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() { None } else { Some(t) }
}

/// Axum middleware state: expected token when auth is enabled.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// When `Some`, require matching Bearer; when `None`, allow all (dev only).
    pub expected: Option<String>,
}

/// Auth middleware: when `expected` is set, require matching Bearer token.
pub async fn require_bearer(req: Request, next: Next, expected: Option<String>) -> Response {
    if let Some(ref want) = expected {
        match bearer_from_request(&req) {
            Some(got) if tokens_equal(got, want) => next.run(req).await,
            Some(_) => (StatusCode::UNAUTHORIZED, "invalid agent token").into_response(),
            None => (
                StatusCode::UNAUTHORIZED,
                "missing Authorization: Bearer <token>",
            )
                .into_response(),
        }
    } else {
        next.run(req).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn short_token_rejected() {
        assert!(check_token_min_length("short").is_err());
    }

    #[test]
    fn long_token_ok() {
        let t = "a".repeat(MIN_TOKEN_LEN);
        assert!(check_token_min_length(&t).is_ok());
    }

    #[test]
    fn resolve_prefers_agent_token() {
        assert_eq!(
            resolve_token(
                Some("agent-token-value-32chars-min!!"),
                Some("api-fallback")
            ),
            Some("agent-token-value-32chars-min!!".into())
        );
    }

    #[test]
    fn resolve_falls_back_to_api() {
        let api = "a".repeat(MIN_TOKEN_LEN);
        assert_eq!(resolve_token(None, Some(&api)), Some(api));
    }

    #[test]
    fn tokens_equal_matches() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "ab"));
    }
}
