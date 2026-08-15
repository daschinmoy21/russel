//! VmSpec and related Cloud Hypervisor spawn configuration types.

use std::path::PathBuf;

/// Filesystem mount for Cloud Hypervisor `--fs` arguments.
#[derive(Debug, Clone)]
pub struct FsMount {
    /// virtiofs tag (guest mount identifier, e.g. "nixstore", "russelcfg").
    pub tag: String,
    /// Host-side virtiofsd Unix socket path.
    pub socket: PathBuf,
    /// Host directory shared into the guest via virtiofs.
    pub shared_dir: PathBuf,
    /// Whether the mount is read-only (e.g. /nix/store).
    pub readonly: bool,
}

/// Centralized Cloud Hypervisor spawn configuration.
///
/// `--cpus boot=<cpus_boot>,max=<cpus_max>` sets a boot count lower than
/// the max so future `vm.resize` (CPU hotplug) can add vCPUs without a
/// restart.  Likewise `--memory size=<mb>M,shared=on,hotplug_size=<hotplug>M`
/// reserves headroom for memory hotplug.
///
/// **Hotplug is NOT implemented yet** — the args only make the guest
/// topology hotplug-capable for a future PR.
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    pub cpus_boot: u8,
    pub cpus_max: u8,
    pub memory_mb: u16,
    pub memory_hotplug_mb: u16,
    pub tap: String,
    pub mac: String,
    pub api_socket: PathBuf,
    pub fs: Vec<FsMount>,
    /// "null" for no console, "tty" for debug.
    pub console: String,
    /// If true, use `--restore source_url=…` instead of `--kernel`.
    pub restore_url: Option<String>,
}

/// Result of kernel resolution.
#[derive(Debug, Clone)]
pub struct KernelInfo {
    pub path: PathBuf,
    /// True when the kernel has virtio/fuse drivers built-in (=y).
    /// When true, the initramfs does NOT need kernel modules or insmod.
    pub drivers_builtin: bool,
}
