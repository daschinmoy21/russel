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
pub const MIN_TOKEN_LEN: usize = russel_core::tokens::MIN_TOKEN_LEN;

/// Env name for the agent node token.
pub const AGENT_TOKEN_ENV: &str = "RUSSEL_AGENT_TOKEN";

/// Fallback shared with the control plane.
pub const API_TOKEN_ENV: &str = "RUSSEL_API_TOKEN";

/// Normalize: unset/blank → None.
pub fn normalize_token(raw: Option<&str>) -> Option<String> {
    russel_core::tokens::normalize_token(raw)
}

/// Reject tokens that are too short or cannot be sent as Bearer.
///
/// Shares one implementation with the control plane (`russel_core::tokens`).
pub fn check_token_min_length(token: &str) -> Result<(), String> {
    russel_core::tokens::check_token_min_length(token)
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

/// Constant-time token equality (shared `russel_core::tokens` implementation).
fn tokens_equal(a: &str, b: &str) -> bool {
    russel_core::tokens::constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Extract Bearer token from Authorization header (case-insensitive scheme).
pub fn bearer_from_request(req: &Request) -> Option<&str> {
    let val = req.headers().get(axum::http::header::AUTHORIZATION)?;
    let s = val.to_str().ok()?;
    russel_core::tokens::bearer_token_from_header(s)
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

    #[test]
    fn bearer_from_request_is_case_insensitive() {
        use axum::http::header;
        let make = |value: &str| {
            Request::builder()
                .header(header::AUTHORIZATION, value)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert_eq!(bearer_from_request(&make("Bearer tok")), Some("tok"));
        assert_eq!(bearer_from_request(&make("bearer tok")), Some("tok"));
        assert_eq!(bearer_from_request(&make("BEARER tok")), Some("tok"));
        assert_eq!(bearer_from_request(&make("bEaReR   tok  ")), Some("tok"));
        assert_eq!(bearer_from_request(&make("Basic tok")), None);
        assert_eq!(bearer_from_request(&make("Bearer ")), None);
    }
}
