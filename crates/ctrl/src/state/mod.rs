//! In-memory control-plane service state, lifecycle claims, and deploy tracking.

mod adopt;
mod app;
mod helpers;
mod lifecycle;

// Stable crate::state::* surface (pre-split public API).
#[allow(unused_imports)]
pub use app::{AppState, DeployGuard, LifecycleClaim};

// Internal types used across the ctrl crate.
#[allow(unused_imports)]
pub(crate) use app::{ServiceState, StateInner};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
