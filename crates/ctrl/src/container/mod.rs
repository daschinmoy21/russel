//! Rootless Podman container runtime for Russel services.

mod passthrough;
mod podman_user;
mod rootfs;
mod runner;

// Stable crate::container::* surface (pre-split public API).
#[allow(unused_imports)]
pub use passthrough::{validate_podman_args_for_runtime, validate_podman_passthrough_args};
/// Used across ctrl; remains `pub(crate)` (not a public library API).
pub(crate) use podman_user::podman_command;
#[allow(unused_imports)]
pub use podman_user::{PodmanUserSource, log_podman_identity, podman_user_source};
#[allow(unused_imports)]
pub use rootfs::{
    DebugToolsCache, PreparedRootfs, RootfsSpec, default_base_dir, prepare_rootfs,
    validate_entrypoint,
};
#[allow(unused_imports)]
pub use runner::{
    ContainerRunner, ContainerStartSpec, RunningContainer, build_run_args, container_log_path,
    parse_podman_rootless,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
