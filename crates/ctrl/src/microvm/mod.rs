//! Cloud Hypervisor microVM runtime: VmSpec, runner lifecycle, agent initramfs.

mod agent;
pub(crate) mod preflight;
mod process;
pub(crate) mod ready;
mod runner;
mod spec;

// Stable crate::microvm::* surface (pre-split public API).
pub use process::{
    BootOutput, cloud_hypervisor_cmdline_matches, cloud_hypervisor_cmdline_matches_under,
};
pub use runner::MicrovmRunner;
pub use spec::{FsCache, FsMount, KernelInfo, MIN_MEMORY_MB, VmSpec, effective_memory_mb};
pub(crate) use spec::{
    check_volume_guest_paths, ensure_private_dir, render_guest_mounts, service_fs_mounts,
    volume_fs_mounts,
};

/// Shared runner singleton used by deploy and warm_pool.
pub(crate) use runner::shared_runner;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
