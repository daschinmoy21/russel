//! In-memory control-plane service state, lifecycle claims, and deploy tracking.

mod adopt;
mod app;
mod helpers;
mod lifecycle;

// Stable crate::state::* surface (pre-split public API).
pub use app::{AppState, DeployGuard, LifecycleClaim};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
