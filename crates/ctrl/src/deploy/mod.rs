//! Deploy pipeline: cold deploy, dual-live generations, and rollback helpers.

mod config;
mod env;
mod pipeline;
mod rollback;
mod runtime;

// Re-export the pre-split public surface so callers keep `crate::deploy::…`.
pub use crate::metadata::prior_runtime_from_disk;
pub use env::{build_container_env, shell_quote, validate_bin_name};
pub use pipeline::DeployPipeline;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
