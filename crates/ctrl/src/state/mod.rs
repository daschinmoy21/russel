//! In-memory control-plane service state, lifecycle claims, and deploy tracking.

mod adopt;
mod app;
mod container_watch;
mod helpers;
mod lifecycle;
mod restart;

// Stable crate::state::* surface (pre-split public API).
pub use app::{AppState, DeployGuard, LifecycleClaim};
pub(crate) use helpers::{ContainerProbe, probe_container};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
