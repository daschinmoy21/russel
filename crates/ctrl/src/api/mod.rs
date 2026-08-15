//! Control-plane HTTP API: auth, router, service lifecycle handlers, secrets.

mod auth;
mod router;
mod secrets;

// Stable crate::api::* surface (pre-split public API).
/// Used by health probes; remains `pub(crate)` (not a public library API).
pub(crate) use auth::deploy_semaphore;
pub use auth::{
    MIN_API_TOKEN_LEN, check_api_token_min_length, configured_api_token, normalize_api_token,
    parse_max_concurrent_deploys, require_auth_from_env,
};
pub use router::router;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
