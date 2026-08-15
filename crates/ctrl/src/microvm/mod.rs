//! Cloud Hypervisor microVM runtime: VmSpec, runner lifecycle, agent initramfs.

mod agent;
mod process;
mod runner;
mod spec;

// Stable crate::microvm::* surface (pre-split public API).
pub use process::BootOutput;
pub use runner::MicrovmRunner;
pub use spec::{FsMount, KernelInfo, VmSpec};

/// Shared runner singleton used by deploy and warm_pool.
pub(crate) use runner::shared_runner;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
