//! VmSpec and related Cloud Hypervisor spawn configuration types.

use std::path::{Path, PathBuf};

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
    /// virtiofsd `--cache` policy.
    pub cache: FsCache,
}

/// virtiofsd `--cache` policy for one share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsCache {
    /// Read-only shares: cache everything.
    Always,
    /// Guest page cache with close-to-open consistency. Writable volumes need
    /// it: SQLite in WAL mode and LMDB use shared writable mmap, which `never`
    /// (FUSE direct I/O) refuses with EIO.
    Auto,
    /// No guest cache, so the host sees scratch writes (`.agent_ready`) at once.
    Never,
}

impl FsCache {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
            Self::Never => "never",
        }
    }
}

/// virtiofs shares for a service VM: host store (RO), deploy.env (RO), guest scratch (RW).
///
/// Scratch is the only guest-writable host path. There is no quota, so a
/// guest can fill that directory and the host disk. Bound the risk to this
/// dir; never share `cfg/`, `metadata.json`, or the service root as RW.
pub(crate) fn service_fs_mounts(
    sock_dir: &Path,
    cfg_dir: &Path,
    scratch_dir: &Path,
) -> Vec<FsMount> {
    vec![
        FsMount {
            tag: "nixstore".into(),
            socket: sock_dir.join("virtiofs-nixstore.sock"),
            shared_dir: PathBuf::from("/nix/store"),
            readonly: true,
            cache: FsCache::Always,
        },
        FsMount {
            tag: "russelcfg".into(),
            socket: sock_dir.join("virtiofs-cfg.sock"),
            shared_dir: cfg_dir.to_path_buf(),
            readonly: true,
            cache: FsCache::Always,
        },
        FsMount {
            tag: "russelscratch".into(),
            socket: sock_dir.join("virtiofs-scratch.sock"),
            shared_dir: scratch_dir.to_path_buf(),
            readonly: false,
            cache: FsCache::Never,
        },
    ]
}

/// Create `path` and set mode `0700` so it is not world-accessible on the host.
pub(crate) fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Centralized Cloud Hypervisor spawn configuration.
///
/// `--cpus boot=<cpus_boot>,max=<cpus_max>,nested=off` sets a boot count lower than
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
    /// NIC backend: a TAP device, or a passt vhost-user socket (#461).
    pub net: crate::network::VmNet,
    pub mac: String,
    pub api_socket: PathBuf,
    pub fs: Vec<FsMount>,
    /// "null" for no console, "tty" for debug.
    pub console: String,
    /// If true, use `--restore source_url=…` instead of `--kernel`.
    pub restore_url: Option<String>,
}

/// `--cpus` value for a cold boot.
///
/// `nested=off` is explicit because Cloud Hypervisor defaults to `nested=on`
/// on x86-64, which hands the guest VMX/SVM on any host with nested
/// virtualization enabled. No service needs it, and it exposes the host's
/// nested VMX/SVM emulation to a guest that loads its own KVM module (#494).
pub fn cpus_arg(cpus_boot: u8, cpus_max: u8) -> String {
    format!("boot={cpus_boot},max={cpus_max},nested=off")
}

#[cfg(test)]
#[test]
fn cpus_arg_disables_nested_virtualization() {
    assert_eq!(cpus_arg(2, 4), "boot=2,max=4,nested=off");
}

/// Result of kernel resolution.
#[derive(Debug, Clone)]
///
/// Only Russel's microVM kernel (virtio/fuse built in) is accepted, so the
/// initramfs never carries kernel modules.
pub struct KernelInfo {
    pub path: PathBuf,
}

/// Guest paths the agent itself mounts or needs. A volume at or above one
/// would hide it (`/nix` hides the store, `/run` hides the scratch share).
const RESERVED_GUEST_PATHS: &[&str] = &[
    "/nix/store",
    "/config",
    "/run/russel",
    "/proc",
    "/sys",
    "/dev",
];

/// Reject `[[volumes]]` guest paths that collide with the microVM agent's
/// own mounts. Containers have no such mounts, so this is microVM-only.
pub(crate) fn check_volume_guest_paths(
    volumes: &[russel_core::volumes::ResolvedVolume],
) -> anyhow::Result<()> {
    for vol in volumes {
        // Component-wise: `/nix/./store` and `//nix//store` are `/nix/store`.
        let guest = Path::new(&vol.guest);
        for reserved in RESERVED_GUEST_PATHS {
            if guest.starts_with(reserved) || Path::new(reserved).starts_with(guest) {
                anyhow::bail!(
                    "volume guest path {} collides with {reserved}, which the microVM guest mounts itself",
                    vol.guest
                );
            }
        }
    }
    Ok(())
}

/// virtiofs tag of the `i`th `[[volumes]]` row. `render_guest_mounts` uses
/// the same tags.
fn volume_tag(i: usize) -> String {
    format!("vol{i}")
}

/// One virtiofs share per `[[volumes]]` row, served from the resolved host path.
pub(crate) fn volume_fs_mounts(
    sock_dir: &Path,
    volumes: &[russel_core::volumes::ResolvedVolume],
) -> Vec<FsMount> {
    volumes
        .iter()
        .enumerate()
        .map(|(i, vol)| FsMount {
            tag: volume_tag(i),
            socket: sock_dir.join(format!("virtiofs-{}.sock", volume_tag(i))),
            shared_dir: vol.host_path.clone(),
            readonly: !vol.rw,
            cache: if vol.rw {
                FsCache::Auto
            } else {
                FsCache::Always
            },
        })
        .collect()
}

/// `/config/mounts` for the guest agent: `<tag> <ro|rw> <guest path>` per
/// line. Guest paths have no newline (core validation) and are the last
/// field, so spaces in them survive `read -r tag mode path`.
pub(crate) fn render_guest_mounts(volumes: &[russel_core::volumes::ResolvedVolume]) -> String {
    volumes
        .iter()
        .enumerate()
        .map(|(i, vol)| {
            format!(
                "{} {} {}\n",
                volume_tag(i),
                if vol.rw { "rw" } else { "ro" },
                vol.guest
            )
        })
        .collect()
}
