//! Cloud Hypervisor microVM runtime: VmSpec, runner lifecycle, agent initramfs.

mod agent;
mod process;
mod runner;
mod spec;

// Stable crate::microvm::* surface (pre-split public API).
#[allow(unused_imports)]
pub use process::BootOutput;
#[allow(unused_imports)]
pub use runner::MicrovmRunner;
#[allow(unused_imports)]
pub use spec::{FsMount, KernelInfo, VmSpec};

/// Shared runner singleton used by deploy and warm_pool.
pub(crate) use runner::shared_runner;
/// virtiofsd sandbox mode from env (used by runner spawn path).
#[allow(unused_imports)]
pub(crate) use runner::virtiofsd_sandbox;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
