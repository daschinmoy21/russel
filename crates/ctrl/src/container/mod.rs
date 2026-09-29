//! Rootless Podman container runtime for Russel services.

mod passthrough;
mod podman_user;
mod ready;
mod rootfs;
mod runner;

// Stable crate::container::* surface (pre-split public API).
pub use passthrough::{validate_podman_args_for_runtime, validate_podman_passthrough_args};
/// Used across ctrl; remains `pub(crate)` (not a public library API).
pub(crate) use podman_user::podman_command;
pub use podman_user::{
    PodmanUserSource, log_podman_identity, pin_podman_program, podman_user_source,
};
pub use ready::{
    CONTAINER_READY_TIMEOUT, ContainerState, LOG_TAIL_LINES, ReadyOutcome, inspect_state, log_tail,
    not_ready_error, wait_until_ready,
};
pub use rootfs::{
    DebugToolsCache, PreparedRootfs, RootfsSpec, default_base_dir, prepare_rootfs,
    validate_entrypoint,
};
pub use runner::{
    ContainerRunner, ContainerStartSpec, RunningContainer, attach_managed_volumes, build_run_args,
    cleanup_service_dir, cleanup_service_dir_in, container_log_path, destroy_preserving_volumes,
    destroy_with_policy_for, detach_managed_volumes, dir_is_kept_volumes_only,
    is_trusted_container_name, parse_podman_rootless, restore_backed_up_service_dir,
};
pub(crate) use runner::{
    prepare_managed_volume_dirs, remove_service_payload_keep_volumes, remove_tree,
    volumes_recorded_for,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
