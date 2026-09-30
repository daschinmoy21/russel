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

/// Token rules shared with the control plane: unset/blank means none, and a
/// set token must be at least [`MIN_TOKEN_LEN`] printable-ASCII chars.
pub use russel_core::tokens::{MIN_TOKEN_LEN, check_token_min_length, normalize_token};

/// Env name for the agent node token.
pub const AGENT_TOKEN_ENV: &str = "RUSSEL_AGENT_TOKEN";

/// Fallback shared with the control plane.
pub const API_TOKEN_ENV: &str = "RUSSEL_API_TOKEN";

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

/// Extract Bearer token from Authorization header (case-insensitive scheme).
pub fn bearer_from_request(req: &Request) -> Option<&str> {
    let val = req.headers().get(axum::http::header::AUTHORIZATION)?;
    let s = val.to_str().ok()?;
    russel_core::tokens::bearer_token_from_header(s)
}

/// Auth middleware: when `expected` is set, require a matching Bearer
/// token; when `None`, allow all (dev only).
pub async fn require_bearer(req: Request, next: Next, expected: Option<String>) -> Response {
    if let Some(ref want) = expected {
        match bearer_from_request(&req) {
            Some(got) if russel_core::tokens::constant_time_eq(got.as_bytes(), want.as_bytes()) => {
                next.run(req).await
            }
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
