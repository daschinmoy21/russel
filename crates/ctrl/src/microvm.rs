use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::process::Command;

use crate::ch_api;

// ── VmSpec: centralized, hotplug-ready Cloud Hypervisor spawn config ────────

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

// ── Kernel info ──────────────────────────────────────────────────────────────

/// Result of kernel resolution.
#[derive(Debug, Clone)]
pub struct KernelInfo {
    pub path: PathBuf,
    /// True when the kernel has virtio/fuse drivers built-in (=y).
    /// When true, the initramfs does NOT need kernel modules or insmod.
    pub drivers_builtin: bool,
}

// ── MicrovmRunner ────────────────────────────────────────────────────────────

/// Shared runner singleton — kernel/busybox/modules caches live across deploys.
pub(crate) fn shared_runner() -> MicrovmRunner {
    static RUNNER: std::sync::LazyLock<MicrovmRunner> =
        std::sync::LazyLock::new(MicrovmRunner::new);
    RUNNER.clone()
}

/// virtiofsd sandbox mode from RUSSEL_VIRTIOFS_SANDBOX (default: chroot).
/// Set RUSSEL_VIRTIOFS_SANDBOX=none for escape hatch.
pub(crate) fn virtiofsd_sandbox() -> String {
    std::env::var("RUSSEL_VIRTIOFS_SANDBOX")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "chroot".to_string())
}

#[derive(Debug, Clone)]
pub struct MicrovmRunner {
    kernel_cache: Arc<Mutex<Option<KernelInfo>>>,
    busybox_cache: Arc<Mutex<Option<PathBuf>>>,
    modules_cache: Arc<Mutex<Option<PathBuf>>>,
    agent_initramfs_cache: Arc<Mutex<Option<PathBuf>>>,
    /// Serializes agent initramfs builds so concurrent cold deploys
    /// don't race on the shared work directory (F-18).
    build_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for MicrovmRunner {
    fn default() -> Self {
        Self {
            kernel_cache: Arc::new(Mutex::new(None)),
            busybox_cache: Arc::new(Mutex::new(None)),
            modules_cache: Arc::new(Mutex::new(None)),
            agent_initramfs_cache: Arc::new(Mutex::new(None)),
            build_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl MicrovmRunner {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Kernel resolution ────────────────────────────────────────────────

    /// Get or build the microVM-optimised kernel (virtio/fuse built-in).
    ///
    /// Resolution order:
    ///   1. In-memory cache
    ///   2. `RUSSEL_KERNEL_PATH` env var → drivers_builtin=true
    ///   3. `nix build .#microvm-kernel` (flake attr, detected via flake.nix)
    ///   4. `./result/bzImage` relative file (user ran nix build without --no-link)
    ///   5. Stock nixpkgs.linux fallback (drivers =m)
    ///
    /// Result is cached in-memory for the lifetime of the runner.
    pub async fn ensure_kernel(&self) -> anyhow::Result<KernelInfo> {
        // 1. In-memory cache
        if let Some(info) = self.check_kernel_cache() {
            return Ok(info);
        }

        // 2. RUSSEL_KERNEL_PATH env var
        if let Ok(env_path) = std::env::var("RUSSEL_KERNEL_PATH") {
            let p = PathBuf::from(&env_path);
            if p.exists() {
                let info = KernelInfo {
                    path: p,
                    drivers_builtin: true,
                };
                self.store_kernel_cache(info.clone());
                tracing::info!(
                    kernel = %info.path.display(),
                    source = "RUSSEL_KERNEL_PATH",
                    "kernel cached (drivers built-in)"
                );
                return Ok(info);
            }
            tracing::warn!("RUSSEL_KERNEL_PATH={env_path} does not exist, continuing");
        }

        // 3. Flake package: nix build .#microvm-kernel
        if let Some(repo_root) = self.find_repo_root()
            && repo_root.join("flake.nix").exists()
        {
            match self.build_flake_kernel(&repo_root).await {
                Ok(path) => {
                    let info = KernelInfo {
                        path,
                        drivers_builtin: true,
                    };
                    self.store_kernel_cache(info.clone());
                    tracing::info!(
                        kernel = %info.path.display(),
                        source = "flake",
                        "microvm kernel cached (drivers built-in)"
                    );
                    return Ok(info);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "flake kernel build failed; trying next source"
                    );
                }
            }
        }

        // 4. Relative result/bzImage (user ran nix build without --no-link)
        let result_bzimage = PathBuf::from("result/bzImage");
        if result_bzimage.exists() {
            let abs =
                std::fs::canonicalize(&result_bzimage).unwrap_or_else(|_| result_bzimage.clone());
            let info = KernelInfo {
                path: abs,
                drivers_builtin: true,
            };
            self.store_kernel_cache(info.clone());
            tracing::info!(
                kernel = %info.path.display(),
                source = "result/bzImage",
                "microvm kernel cached (drivers built-in)"
            );
            return Ok(info);
        }

        // 5. Stock kernel fallback (drivers as modules)
        let path = self.build_stock_kernel().await?;
        let info = KernelInfo {
            path,
            drivers_builtin: false,
        };
        self.store_kernel_cache(info.clone());
        tracing::info!(
            kernel = %info.path.display(),
            source = "stock",
            "stock kernel cached (drivers =m)"
        );
        Ok(info)
    }

    /// Find the repo root by looking for flake.nix upward from
    /// CARGO_MANIFEST_DIR (compile-time) or cwd (runtime).
    fn find_repo_root(&self) -> Option<PathBuf> {
        // Compile-time: CARGO_MANIFEST_DIR is crates/ctrl → ../../ is repo root
        let compile_time = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        if compile_time.join("flake.nix").exists() {
            return Some(std::fs::canonicalize(&compile_time).unwrap_or(compile_time));
        }
        // Runtime: try cwd
        if let Ok(cwd) = std::env::current_dir()
            && cwd.join("flake.nix").exists()
        {
            return Some(cwd);
        }
        None
    }

    async fn build_flake_kernel(&self, repo_root: &Path) -> anyhow::Result<PathBuf> {
        tracing::info!(
            repo = %repo_root.display(),
            "building microvm kernel via flake"
        );
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                ".#microvm-kernel",
            ])
            .current_dir(repo_root)
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("nix build .#microvm-kernel failed");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let kernel = PathBuf::from(format!("{}/bzImage", store_path));
        if !kernel.exists() {
            anyhow::bail!(
                "microvm kernel built but bzImage not found at {}",
                kernel.display()
            );
        }
        Ok(kernel)
    }

    async fn build_stock_kernel(&self) -> anyhow::Result<PathBuf> {
        let system = crate::build::current_system();
        tracing::info!(system = %system, "building stock kernel from nixpkgs");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
                "--argstr",
                "system",
                system,
                "linux",
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to build stock kernel from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let kernel = PathBuf::from(format!("{}/bzImage", store_path));
        Ok(kernel)
    }

    fn check_kernel_cache(&self) -> Option<KernelInfo> {
        if let Ok(cache) = self.kernel_cache.lock()
            && let Some(ref info) = *cache
            && info.path.exists()
        {
            return Some(info.clone());
        }
        None
    }

    fn store_kernel_cache(&self, info: KernelInfo) {
        if let Ok(mut cache) = self.kernel_cache.lock() {
            *cache = Some(info);
        } else {
            tracing::warn!("kernel cache lock poisoned, skipping cache update");
        }
    }

    // ── Busybox (still needed for initramfs) ─────────────────────────────

    pub async fn ensure_busybox(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = self.check_path_cache(&self.busybox_cache) {
            return Ok(path);
        }

        let system = crate::build::current_system();
        tracing::info!(system = %system, "building busybox from nixpkgs");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
                "--argstr",
                "system",
                system,
                "busybox",
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to build busybox from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let path = PathBuf::from(store_path);
        self.store_path_cache(&self.busybox_cache, path.clone());
        tracing::info!(busybox = %path.display(), "busybox cached");
        Ok(path)
    }

    // ── Kernel modules (only needed when falling back to stock kernel) ───

    /// Resolve kernel modules for the stock kernel fallback path.
    /// Returns `None` when the microvm kernel is in use (drivers built-in).
    pub async fn ensure_kernel_modules(&self) -> anyhow::Result<Option<PathBuf>> {
        let kernel = self.ensure_kernel().await?;
        if kernel.drivers_builtin {
            tracing::debug!("microvm kernel: drivers built-in, no modules needed");
            return Ok(None);
        }

        if let Some(path) = self.check_path_cache(&self.modules_cache) {
            return Ok(Some(path));
        }

        let system = crate::build::current_system();
        tracing::info!(system = %system, "resolving kernel modules from nixpkgs");
        let expr = format!(
            "let pkgs = import <nixpkgs> {{ system = \"{system}\"; }}; in pkgs.linux.modules"
        );
        let output = Command::new("nix")
            .args([
                "build",
                "--impure",
                "--no-link",
                "--print-out-paths",
                "--expr",
                &expr,
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to resolve kernel modules from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let path = PathBuf::from(store_path);
        self.store_path_cache(&self.modules_cache, path.clone());
        tracing::info!(modules = %path.display(), "kernel modules cached");
        Ok(Some(path))
    }

    // ── Path cache helpers ───────────────────────────────────────────────

    fn check_path_cache(&self, cache: &Mutex<Option<PathBuf>>) -> Option<PathBuf> {
        if let Ok(cache) = cache.lock() {
            if let Some(ref path) = *cache
                && path.exists()
            {
                return Some(path.clone());
            }
        } else {
            tracing::warn!("cache lock poisoned, re-building from scratch");
        }
        None
    }

    fn store_path_cache(&self, cache: &Mutex<Option<PathBuf>>, path: PathBuf) {
        if let Ok(mut c) = cache.lock() {
            *c = Some(path);
        } else {
            tracing::warn!("cache lock poisoned, skipping cache update");
        }
    }

    // ── Agent initramfs (generic, config-driven via deploy.env) ──────────

    /// Build a **generic** agent initramfs once (cached).
    ///
    /// The `/init` script:
    ///   1. mounts proc/sys/devtmpfs
    ///   2. mounts virtiofs `nixstore` → `/nix/store`
    ///   3. mounts virtiofs `russelcfg` → `/config`
    ///   4. echoes `ready` into `/config/.agent_ready`
    ///   5. waits until `/config/deploy.env` exists
    ///   6. sources `/config/deploy.env` (expects `VM_IP`, `HOST_IP`, `PORT`, `APP`)
    ///   7. configures eth0 + default route; `exec $APP`
    ///
    /// No app binary is baked in — it is resolved at deploy time via `deploy.env`.
    pub async fn build_agent_initramfs(&self) -> anyhow::Result<PathBuf> {
        // Fast path: cache hit without lock.
        if let Some(path) = self.check_path_cache(&self.agent_initramfs_cache) {
            return Ok(path);
        }

        // Serialize concurrent builds (F-18).
        let _guard = self.build_lock.lock().await;

        // Double-check cache after acquiring lock.
        if let Some(path) = self.check_path_cache(&self.agent_initramfs_cache) {
            return Ok(path);
        }

        let busybox_path = self.ensure_busybox().await?;
        let kernel_modules_path = self.ensure_kernel_modules().await?;
        let pool_dir = PathBuf::from("/var/lib/russel/_pool");
        std::fs::create_dir_all(&pool_dir)?;

        // Unique temp dir so concurrent deploys never share a work dir (F-18).
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let work = pool_dir.join(format!("agent-initramfs.d.{pid}.{nanos}"));

        // Guard always removes the temp dir on any exit path (#44).
        struct TempDirGuard(PathBuf);
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                if self.0.exists()
                    && let Err(e) = std::fs::remove_dir_all(&self.0)
                {
                    tracing::warn!(
                        dir = %self.0.display(),
                        error = %e,
                        "failed to remove agent initramfs work dir"
                    );
                }
            }
        }
        let _cleanup = TempDirGuard(work.clone());
        std::fs::create_dir_all(&work)?;

        // Bump AGENT_INITRAMFS_BASENAME when AGENT_INIT_SCRIPT changes so disk cache cannot serve a stale CPIO.
        let initramfs_file = pool_dir.join(AGENT_INITRAMFS_BASENAME);

        // Copy kernel modules when using stock kernel (drivers not built-in).
        // ponytail: same virtio/fuse list as legacy per-service initramfs.
        if let Some(ref mod_path) = kernel_modules_path {
            let mods: &[&str] = &[
                "drivers/virtio/virtio_ring.ko.xz",
                "drivers/virtio/virtio.ko.xz",
                "drivers/virtio/virtio_pci_modern_dev.ko.xz",
                "drivers/virtio/virtio_pci_legacy_dev.ko.xz",
                "drivers/virtio/virtio_pci.ko.xz",
                "net/core/failover.ko.xz",
                "drivers/net/net_failover.ko.xz",
                "drivers/net/virtio_net.ko.xz",
                "fs/fuse/fuse.ko.xz",
                "fs/fuse/virtiofs.ko.xz",
            ];
            self.copy_kernel_modules(mod_path, &work, mods)?;
        }

        let bb_bin = format!("{}/bin/busybox", busybox_path.display());
        let init = AGENT_INIT_SCRIPT;

        use std::os::unix::fs::PermissionsExt;
        let init_path = work.join("init");
        std::fs::write(&init_path, init)?;
        std::fs::set_permissions(&init_path, std::fs::Permissions::from_mode(0o755))?;

        self.create_busybox_symlinks(&work, &bb_bin)?;
        self.copy_closure_to(&busybox_path, &work).await?;
        // Build to a temp file, then atomic rename (F-18).
        let tmp_cpio = pool_dir.join(format!(".agent-initramfs-v3.{pid}.{nanos}.cpio.tmp"));
        self.pack_cpio(&work, &tmp_cpio, &bb_bin).await?;
        std::fs::rename(&tmp_cpio, &initramfs_file)?;

        self.store_path_cache(&self.agent_initramfs_cache, initramfs_file.clone());

        tracing::info!(
            initramfs = %initramfs_file.display(),
            "agent initramfs built (config-driven, no app baked in)"
        );
        Ok(initramfs_file)
    }

    fn copy_kernel_modules(
        &self,
        kernel_modules_path: &Path,
        work: &Path,
        needed_modules: &[&str],
    ) -> anyhow::Result<()> {
        let modules_dest = work.join("modules");
        std::fs::create_dir_all(&modules_dest)?;

        let kver = self.find_kver(kernel_modules_path)?;
        let kmod_base = kernel_modules_path
            .join("lib/modules")
            .join(&kver)
            .join("kernel");

        let mut missing: Vec<&str> = Vec::new();
        for module_rel in needed_modules {
            let src = kmod_base.join(module_rel);
            let name = Path::new(module_rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(module_rel);
            let dest = modules_dest.join(name);
            if src.exists() {
                std::fs::copy(&src, &dest)?;
                tracing::debug!(module = %name, "copied kernel module to initramfs");
            } else {
                tracing::warn!(module = %module_rel, "kernel module not found");
                missing.push(module_rel);
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "kernel modules not found in {} (kver={kver}): {}. \
                 The guest would boot without virtio_net and be unreachable. \
                 Check that your kernel provides these modules or use a microvm-kernel with drivers built-in.",
                kmod_base.display(),
                missing.join(", ")
            );
        }
        Ok(())
    }

    fn create_busybox_symlinks(&self, work: &Path, bb_bin: &str) -> anyhow::Result<()> {
        let bin_dir = work.join("bin");
        std::fs::create_dir_all(&bin_dir)?;
        // Agent init needs `cat` + `sleep` in addition to basic tools.
        for name in AGENT_BUSYBOX_APPLETS {
            let dest = bin_dir.join(name);
            if let Err(e) = std::fs::remove_file(&dest)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(file = %dest.display(), error = %e, "failed to remove previous symlink");
            }
            std::os::unix::fs::symlink(bb_bin, &dest)?;
        }
        Ok(())
    }

    /// Pack the work directory into a newc-format CPIO via busybox.
    /// Spawns `find` and `cpio` directly — no shell (F-27).
    ///
    /// Both sides must use `current_dir(work)`: find emits relative paths like
    /// `./bin/...`, and busybox cpio opens them relative to its own cwd.
    /// Do **not** use `Command::output()` for cpio — it forces stdout to a pipe
    /// and discards the file we create for the archive.
    async fn pack_cpio(
        &self,
        work: &Path,
        initramfs_file: &Path,
        bb_bin: &str,
    ) -> anyhow::Result<()> {
        let work = work.to_path_buf();
        let initramfs_file = initramfs_file.to_path_buf();
        let bb_bin = bb_bin.to_string();

        tokio::task::spawn_blocking(move || pack_cpio_blocking(&work, &initramfs_file, &bb_bin))
            .await??;
        Ok(())
    }

    fn find_kver(&self, modules_path: &Path) -> anyhow::Result<String> {
        let mods_dir = modules_path.join("lib/modules");
        let mut versions: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&mods_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                versions.push(name.to_string());
            }
        }
        if versions.is_empty() {
            anyhow::bail!(
                "no kernel version directory found in {}",
                mods_dir.display()
            );
        }
        if versions.len() > 1 {
            tracing::warn!(
                versions = ?versions,
                "multiple kernel version directories found; selecting greatest (last by version sort)"
            );
        }
        select_kernel_version(&mut versions)
    }

    async fn copy_closure_to(&self, store_path: &Path, dest_root: &Path) -> anyhow::Result<()> {
        let output = Command::new("nix")
            .args(["path-info", "-r", &store_path.display().to_string()])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "nix path-info -r failed for {}: {}",
                store_path.display(),
                stderr.trim()
            );
        }

        let closure = String::from_utf8_lossy(&output.stdout);
        for line in closure.lines() {
            let src = line.trim();
            if src.is_empty() {
                continue;
            }
            let rel = src.trim_start_matches('/');
            if dest_root.join(rel).exists() {
                continue;
            }
            self.copy_path_tree(Path::new(src), dest_root)?;
        }
        Ok(())
    }

    fn copy_path_tree(&self, src: &Path, dest_root: &Path) -> anyhow::Result<()> {
        let rel = src.strip_prefix("/").unwrap_or(src);
        let dest = dest_root.join(rel);

        let meta = std::fs::symlink_metadata(src)?;
        if meta.is_symlink() {
            if dest.exists() {
                return Ok(());
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let target = std::fs::read_link(src)?;
            std::os::unix::fs::symlink(&target, &dest)?;
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dest)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                self.copy_path_tree(&entry.path(), dest_root)?;
            }
        } else {
            if dest.exists() {
                return Ok(());
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::hard_link(src, &dest).is_err() {
                std::fs::copy(src, &dest)?;
            }
        }
        Ok(())
    }

    // ── Boot ─────────────────────────────────────────────────────────────

    /// Boot a Cloud Hypervisor microVM using a `VmSpec`.
    ///
    /// Spawns virtiofsd for each `FsMount`, waits for their sockets, then
    /// boots CH (or restores from snapshot if `restore_url` is set).
    ///
    /// Returns `BootOutput` with the VM child and all virtiofsd children.
    pub async fn boot_vm(&self, spec: &VmSpec) -> anyhow::Result<BootOutput> {
        tracing::info!(
            tap = %spec.tap,
            mac = %spec.mac,
            kernel = %spec.kernel.display(),
            cpus = format!("boot={},max={}", spec.cpus_boot, spec.cpus_max),
            memory = format!("{}M,hotplug_size={}M", spec.memory_mb, spec.memory_hotplug_mb),
            "booting cloud-hypervisor"
        );

        let sock_dir = spec
            .api_socket
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/var/lib/russel"));

        std::fs::create_dir_all(&sock_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        // ── Spawn virtiofsd for each fs mount ──────────────────────────
        let mut virtiofsd_children: Vec<tokio::process::Child> = Vec::new();
        match spec.fs.as_slice() {
            [] => {}
            [a] => {
                let child = self
                    .spawn_virtiofsd(&a.socket, &a.shared_dir, a.readonly)
                    .await?;
                virtiofsd_children.push(child);
            }
            [a, b] => {
                let (ra, rb) = tokio::join!(
                    self.spawn_virtiofsd(&a.socket, &a.shared_dir, a.readonly),
                    self.spawn_virtiofsd(&b.socket, &b.shared_dir, b.readonly),
                );
                virtiofsd_children.push(ra?);
                virtiofsd_children.push(rb?);
            }
            _ => {
                for fs in &spec.fs {
                    let child = self
                        .spawn_virtiofsd(&fs.socket, &fs.shared_dir, fs.readonly)
                        .await?;
                    virtiofsd_children.push(child);
                }
            }
        }

        // ── Build CH command line ──────────────────────────────────────
        let mem_mb = spec.memory_mb.max(256);

        let mut cmd = Command::new("cloud-hypervisor");
        cmd.arg("--api-socket")
            .arg(format!("path={}", spec.api_socket.display()));

        // Restore: only api-socket + restore; vm config lives in snapshot.
        // Cold boot: pass full topology + kernel + initramfs + fs mounts.
        if let Some(ref restore_url) = spec.restore_url {
            cmd.arg("--restore")
                .arg(format!("source_url={restore_url},resume=true"));
        } else {
            cmd.arg("--cpus")
                .arg(format!("boot={},max={}", spec.cpus_boot, spec.cpus_max))
                .arg("--memory")
                .arg(format!(
                    "size={mem_mb}M,shared=on,hotplug_size={}M",
                    spec.memory_hotplug_mb
                ))
                .arg("--cmdline")
                .arg(&spec.cmdline)
                .arg("--console")
                .arg(&spec.console)
                .arg("--net")
                .arg(format!("tap={},mac={}", spec.tap, spec.mac))
                .arg("--kernel")
                .arg(&spec.kernel)
                .arg("--initramfs")
                .arg(&spec.initramfs);

            // Cloud Hypervisor (clap) takes one `--fs` with multiple values:
            //   --fs tag=a,socket=… tag=b,socket=…
            // Repeating `--fs` fails with "cannot be used multiple times".
            if !spec.fs.is_empty() {
                cmd.arg("--fs");
                for fs in &spec.fs {
                    cmd.arg(format!(
                        "tag={},socket={},num_queues=1,queue_size=512",
                        fs.tag,
                        fs.socket.display()
                    ));
                }
            }

            // Serial console for guest diagnosis on cold boot.
            let serial_path = sock_dir.join("console.log");
            cmd.arg("--serial")
                .arg(format!("file={}", serial_path.display()));
        }

        cmd.kill_on_drop(true);

        let mut vm_child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn cloud-hypervisor: {e}. \
                 Install with: nix-env -iA nixpkgs.cloud-hypervisor \
                 (or add to your devShell)"
            )
        })?;

        // Fail fast if CH rejects argv (e.g. bad --fs) and exits immediately.
        tokio::time::sleep(Duration::from_millis(50)).await;
        match vm_child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!(
                    "cloud-hypervisor exited immediately with {status} \
                     (check --fs/--kernel/--initramfs args; multi-fs must be one --fs with multiple values)"
                );
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "could not poll cloud-hypervisor status"),
        }

        tracing::info!(pid = vm_child.id(), "cloud-hypervisor started");

        // F-02: wait for the API socket to appear, then lock it down.
        {
            let api_socket = &spec.api_socket;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
            while !api_socket.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            if api_socket.exists() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(e) =
                        std::fs::set_permissions(api_socket, std::fs::Permissions::from_mode(0o600))
                    {
                        tracing::warn!(
                            socket = %api_socket.display(),
                            error = %e,
                            "failed to chmod cloud-hypervisor API socket"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    socket = %api_socket.display(),
                    "cloud-hypervisor API socket did not appear within 2s"
                );
            }
        }
        Ok(BootOutput {
            vm_child,
            virtiofsd_children,
        })
    }

    /// Spawn virtiofsd for a socket/shared_dir pair and wait for readiness.
    pub(crate) async fn spawn_virtiofsd(
        &self,
        socket: &Path,
        shared_dir: &Path,
        readonly: bool,
    ) -> anyhow::Result<tokio::process::Child> {
        // Remove stale socket
        if let Err(e) = std::fs::remove_file(socket)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %socket.display(), error = %e, "failed to remove stale virtiofs socket");
        }

        // Ensure the shared directory exists
        std::fs::create_dir_all(shared_dir)?;

        tracing::info!(
            socket = %socket.display(),
            shared_dir = %shared_dir.display(),
            readonly,
            "spawning virtiofsd"
        );

        let mut cmd = Command::new("virtiofsd");
        // virtiofsd --cache accepts: auto | always | never | metadata (not "none").
        // readonly nixstore: always cache; rw cfg: never cache so guest sees host writes promptly.
        let cache_policy = if readonly { "always" } else { "never" };
        cmd.arg(format!("--socket-path={}", socket.display()))
            .arg(format!("--shared-dir={}", shared_dir.display()))
            .arg(format!("--sandbox={}", virtiofsd_sandbox()))
            .arg(format!("--cache={cache_policy}"));

        if readonly {
            cmd.arg("--readonly");
        }

        let child = cmd.kill_on_drop(true).spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn virtiofsd: {e}. \
                 Install with: nix-env -iA nixpkgs.virtiofsd"
            )
        })?;

        tracing::info!(pid = child.id(), "virtiofsd started");

        // Wait for socket to be created
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
        while !socket.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if !socket.exists() {
            anyhow::bail!(
                "virtiofsd did not create socket {} within 2s",
                socket.display()
            );
        }

        Ok(child)
    }

    // ── Stop / destroy ───────────────────────────────────────────────────

    /// Gracefully stop a VM through Cloud Hypervisor's REST API.
    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        Self::validate_service_id(service_id)?;

        let metadata = read_metadata(service_id);
        let api_socket = format!("/var/lib/russel/{service_id}/cloud-hypervisor.sock");
        if metadata.is_none() && !Path::new(&api_socket).exists() {
            return Ok(());
        }

        let api_socket_path = Path::new(&api_socket);
        let vm_pid = metadata.as_ref().and_then(|m| m.vm_pid);

        // Try API-driven shutdown.
        let api_result = async {
            ch_api::vm_shutdown(api_socket_path).await?;
            ch_api::vmm_shutdown(api_socket_path).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let vm_stopped = if api_result.is_ok() {
            match vm_pid {
                Some(pid) => wait_for_process_exit(pid, Duration::from_secs(5)).await,
                None => true,
            }
        } else {
            if let Err(ref error) = api_result {
                tracing::warn!(service_id, error = %error, "CH API shutdown failed; using process fallback");
            }
            false
        };

        if !vm_stopped {
            let tap = metadata
                .as_ref()
                .and_then(|m| m.tap_id.clone())
                .unwrap_or_else(|| crate::network::subnet_for(service_id).tap_id);
            self.pkill_service_process(
                service_id,
                "cloud-hypervisor",
                &format!("(^|[[:space:]])cloud-hypervisor .*tap={tap}(,|$)",),
            )
            .await?;
            if let Some(pid) = vm_pid {
                let _ = wait_for_process_exit(pid, Duration::from_secs(2)).await;
            }
        }

        // Kill virtiofsd processes (plural — deploy writes an array).
        let virtiofsd_pattern = format!("(^|[[:space:]])virtiofsd .*russel/{service_id}/");
        let mut virtiofsd_killed = false;
        if let Some(ref meta) = metadata {
            for &pid in &meta.virtiofsd_pids {
                if terminate_owned_process(pid, service_id)
                    .await
                    .unwrap_or(false)
                {
                    virtiofsd_killed = true;
                }
            }
        }
        if !virtiofsd_killed {
            self.pkill_service_process(service_id, "virtiofsd", &virtiofsd_pattern)
                .await?;
        }

        // Kill socat
        let socat_pattern = format!("^socat-russel-{service_id}( |$)");
        if let Some(pid) = metadata.as_ref().and_then(|m| m.socat_pid)
            && terminate_owned_process(pid, service_id).await?
        {
            // killed by PID
        } else {
            self.pkill_service_process(service_id, "socat", &socat_pattern)
                .await?;
        }

        Ok(())
    }

    async fn pkill_service_process(
        &self,
        service_id: &str,
        process_kind: &str,
        pattern: &str,
    ) -> anyhow::Result<()> {
        let output = Command::new("pkill")
            .args(["-TERM", "-f", pattern])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run pkill for {process_kind}: {e}"))?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!(
                "pkill for {process_kind} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        tracing::debug!(service_id, process_kind, "process fallback completed");
        Ok(())
    }

    /// Verify PID ownership via /proc/<pid>/cmdline.
    fn verify_process_ownership(pid: u32, service_id: &str) -> bool {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let tap_arg = format!("tap={}", crate::network::subnet_for(service_id).tap_id);
        std::fs::read(cmdline_path)
            .map(|cmdline| {
                cmdline.split(|byte| *byte == 0).any(|arg| {
                    let arg = String::from_utf8_lossy(arg);
                    arg == tap_arg
                        || arg.starts_with(&format!("{tap_arg},"))
                        || arg.contains(&format!("russel/{service_id}/"))
                        || arg.contains(&format!("socat-russel-{service_id}"))
                })
            })
            .unwrap_or(false)
    }

    /// Destroy all state for a microVM.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        Self::validate_service_id(service_id)?;

        // Prefer on-disk network identity (generation-scoped TAP) before stop
        // clears processes; fall back to deterministic subnet_for.
        let alloc = network_alloc_for_service(service_id);

        self.stop(service_id).await?;

        crate::network::TapForwarder::teardown(&alloc).await?;
        crate::network::PortAllocator::release(service_id);
        crate::network::release_subnet(service_id);

        for dir in &[
            format!("/var/lib/microvms/{service_id}"),
            format!("/var/lib/russel/{service_id}"),
        ] {
            let path = std::path::Path::new(dir);
            if path.exists() {
                // F-46: use std::fs::remove_dir_all instead of subprocess rm -rf.
                if let Err(e) = std::fs::remove_dir_all(path) {
                    anyhow::bail!("failed to remove directory {dir}: {e}");
                }
            }
        }
        for file in &[
            format!("/nix/var/nix/gcroots/microvm/{service_id}"),
            format!("/nix/var/nix/gcroots/microvm/booted-{service_id}"),
        ] {
            let path = std::path::Path::new(file);
            if path.exists()
                && let Err(e) = std::fs::remove_file(path)
            {
                anyhow::bail!("failed to remove gcroot {file}: {e}");
            }
        }
        Ok(())
    }

    /// Validate service_id for safe filesystem use.
    pub fn validate_service_id(service_id: &str) -> anyhow::Result<()> {
        if service_id.is_empty() {
            anyhow::bail!("service_id cannot be empty");
        }
        if service_id.len() > 128 {
            anyhow::bail!("service_id too long (max 128 characters)");
        }
        if service_id.contains('/') || service_id.contains('\\') {
            anyhow::bail!("service_id cannot contain path separators");
        }
        if service_id.contains("..") || service_id == "." {
            anyhow::bail!("service_id cannot contain path traversal components");
        }
        if !service_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!(
                "service_id can only contain alphanumeric characters, dashes, and underscores"
            );
        }
        Ok(())
    }

    /// List registered microVMs (from /var/lib/microvms).
    #[allow(dead_code)] // admin/status helper; not yet exposed via API
    pub async fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut vms = Vec::new();
        let state_dir = Path::new("/var/lib/microvms");
        if state_dir.exists()
            && let Ok(mut entries) = tokio::fs::read_dir(state_dir).await
        {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await?.is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    vms.push(name.to_string());
                }
            }
        }
        Ok(vms)
    }
}

// ── Agent init script (config-driven guest, no app baked in) ────────────────

/// Basename for the agent initramfs CPIO file. Bump when `AGENT_INIT_SCRIPT` changes
/// so stale disk caches cannot serve an old init.
const AGENT_INITRAMFS_BASENAME: &str = "agent-initramfs-v3.cpio";

/// Busybox applets symlinked into the agent initramfs.
const AGENT_BUSYBOX_APPLETS: &[&str] = &[
    "sh", "mount", "ip", "mkdir", "insmod", "xzcat", "cat", "sleep", "usleep", "ls",
];

const AGENT_INIT_SCRIPT: &str = r#"#!/bin/sh
/bin/mkdir -p /proc /sys /dev /nix/store /config /tmp
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs devtmpfs /dev

# Load virtio/fuse modules if present (stock kernel fallback).
# ponytail: ordered list matches legacy per-service init, xzcat+insmod.
if [ -d /modules ] && ls /modules/*.ko.xz >/dev/null 2>&1; then
  echo "Loading kernel modules from /modules..."
  for mod in /modules/virtio_ring.ko.xz /modules/virtio.ko.xz \
             /modules/virtio_pci_modern_dev.ko.xz /modules/virtio_pci_legacy_dev.ko.xz \
             /modules/virtio_pci.ko.xz \
             /modules/failover.ko.xz /modules/net_failover.ko.xz \
             /modules/virtio_net.ko.xz \
             /modules/fuse.ko.xz /modules/virtiofs.ko.xz; do
    [ -f "$mod" ] || continue
    ko="/tmp/${mod##*/}"
    ko="${ko%.xz}"
    if /bin/xzcat "$mod" > "$ko" 2>/dev/null && /bin/insmod "$ko" 2>/dev/null; then
      true
    else
      echo "WARN: failed to load ${mod##*/}"
    fi
  done
fi

# Mount host store (read-only). Virtio devices can lag a few hundred ms.
echo "Mounting /nix/store via virtiofs..."
i=0
mounted=0
while [ $i -lt 30 ]; do
  if /bin/mount -t virtiofs nixstore /nix/store; then
    echo "nixstore mounted"
    mounted=1
    break
  fi
  i=$((i + 1))
  /bin/usleep 5000
done
if [ "$mounted" -ne 1 ]; then
  echo "ERROR: Failed to mount /nix/store via virtiofs after retries"
  /bin/ls -l /sys/bus/virtio/devices 2>/dev/null || true
  exec /bin/sh
fi

# Mount config (read-write) for deploy.env + readiness marker.
echo "Mounting /config via virtiofs..."
i=0
mounted=0
while [ $i -lt 30 ]; do
  if /bin/mount -t virtiofs russelcfg /config; then
    echo "russelcfg mounted"
    mounted=1
    break
  fi
  i=$((i + 1))
  /bin/usleep 5000
done
if [ "$mounted" -ne 1 ]; then
  echo "ERROR: Failed to mount /config via virtiofs after retries"
  exec /bin/sh
fi

# Signal readiness to host.
echo "ready" > /config/.agent_ready

# Wait for deploy.env to be written by the host.
echo "Waiting for /config/deploy.env..."
while [ ! -f /config/deploy.env ]; do
  /bin/usleep 10000
done

# Source deployment config.
set -a
. /config/deploy.env
set +a

# Find network interface (net.ifnames=0 friendly).
IFACE="eth0"
if ! /bin/ip link show eth0 >/dev/null 2>&1; then
  for iface in /sys/class/net/*; do
    ifname="${iface##*/}"
    if [ "$ifname" != "lo" ]; then
      IFACE="$ifname"
      break
    fi
  done
fi

echo "Configuring $IFACE: ip=$VM_IP gw=$HOST_IP port=$PORT"
/bin/ip addr add $VM_IP/30 dev $IFACE
/bin/ip link set $IFACE up
/bin/ip route add default via $HOST_IP

export PORT
cd /
echo "exec $APP"
exec "$APP"
echo "ERROR: exec failed! Spawning emergency shell..."
exec /bin/sh
"#;

// ── BootOutput ───────────────────────────────────────────────────────────────

/// Output of `MicrovmRunner::boot()` / `boot_vm()`.
pub struct BootOutput {
    pub vm_child: tokio::process::Child,
    /// All virtiofsd children (one for nixstore, optionally one for config).
    pub virtiofsd_children: Vec<tokio::process::Child>,
}

// ── Process metadata (reused by stop/destroy) ───────────────────────────────

#[derive(Debug, Default)]
struct ProcessMetadata {
    vm_pid: Option<u32>,
    virtiofsd_pids: Vec<u32>,
    socat_pid: Option<u32>,
    tap_id: Option<String>,
    host_ip: Option<String>,
    vm_ip: Option<String>,
}

fn read_metadata(service_id: &str) -> Option<ProcessMetadata> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let virtiofsd_pids = value
        .get("virtiofsd_pids")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .or_else(|| {
            // Legacy: singular virtiofsd_pid field.
            value
                .get("virtiofsd_pid")
                .and_then(|pid| pid.as_u64())
                .map(|pid| vec![pid as u32])
        })
        .unwrap_or_default();
    Some(ProcessMetadata {
        vm_pid: value
            .get("vm_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        virtiofsd_pids,
        socat_pid: value
            .get("socat_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        tap_id: value
            .get("tap_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        host_ip: value
            .get("host_ip")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        vm_ip: value
            .get("vm_ip")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Prefer generation-recorded TAP / IPs so destroy still works after promote.
fn network_alloc_for_service(service_id: &str) -> crate::network::SubnetAllocation {
    let fallback = crate::network::subnet_for(service_id);
    let Some(meta) = read_metadata(service_id) else {
        return fallback;
    };
    match (meta.tap_id, meta.host_ip, meta.vm_ip) {
        (Some(tap_id), Some(host_ip), Some(vm_ip)) => crate::network::SubnetAllocation {
            host_ip,
            vm_ip,
            mac: fallback.mac,
            tap_id,
        },
        _ => fallback,
    }
}

async fn terminate_owned_process(pid: u32, service_id: &str) -> anyhow::Result<bool> {
    if !MicrovmRunner::verify_process_ownership(pid, service_id) {
        return Ok(false);
    }
    let output = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await?;
    if !output.status.success() && process_is_alive(pid) {
        anyhow::bail!(
            "failed to terminate process {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(wait_for_process_exit(pid, Duration::from_secs(2)).await)
}

fn process_is_alive(pid: u32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(path) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while process_is_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !process_is_alive(pid)
}

/// Pack `work` into a newc CPIO at `initramfs_file` using busybox multi-call
/// `find` + `cpio`. Both processes share `current_dir(work)` so relative paths
/// from find resolve correctly for cpio. Used by agent initramfs builds.
fn pack_cpio_blocking(work: &Path, initramfs_file: &Path, bb_bin: &str) -> anyhow::Result<()> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let out_file = std::fs::File::create(initramfs_file)?;

    let mut find = std::process::Command::new(bb_bin)
        .arg("find")
        .arg(".")
        .current_dir(work)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn busybox find: {e}"))?;

    let find_stdout = find
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture find stdout"))?;

    // cpio must run with the same cwd as find: find emits paths like `./bin/sh`
    // which cpio opens relative to its cwd. Also use spawn+wait — not
    // Command::output() — so stdout stays the archive file (output() forces a pipe).
    let cpio = std::process::Command::new(bb_bin)
        .arg("cpio")
        .arg("-o")
        .arg("-H")
        .arg("newc")
        .current_dir(work)
        .stdin(find_stdout)
        .stdout(out_file)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn busybox cpio: {e}"))?;

    // Wait consumer first so the pipe drains; then the producer.
    let cpio_out = cpio
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("wait cpio: {e}"))?;
    let find_status = find
        .wait()
        .map_err(|e| anyhow::anyhow!("wait find: {e}"))?;

    if !cpio_out.status.success() {
        anyhow::bail!(
            "cpio failed: exit {:?} — {}",
            cpio_out.status.code(),
            String::from_utf8_lossy(&cpio_out.stderr).trim()
        );
    }
    if !find_status.success() {
        anyhow::bail!(
            "find failed: exit code {:?} signal {:?}",
            find_status.code(),
            find_status.signal()
        );
    }

    let meta = std::fs::metadata(initramfs_file)?;
    if meta.len() == 0 {
        anyhow::bail!("cpio produced empty initramfs at {}", initramfs_file.display());
    }
    Ok(())
}

/// Select the greatest kernel version from a list of version strings.
/// Sorts lexicographically (natural version sort) and returns the last.
/// Exported for unit testing.
fn select_kernel_version(versions: &mut [String]) -> anyhow::Result<String> {
    if versions.is_empty() {
        anyhow::bail!("no kernel versions provided");
    }
    versions.sort();
    // Safe: we bail above if versions is empty.
    versions
        .last()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unreachable: versions is empty"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn select_kernel_version_single() {
        let mut versions = vec!["6.1.0".to_string()];
        let result = select_kernel_version(&mut versions).unwrap();
        assert_eq!(result, "6.1.0");
    }

    #[test]
    fn select_kernel_version_lexicographic_selects_greatest() {
        // Lexicographic sort: "6.1.0" < "6.10.0" < "6.2.0"
        // (NOT numeric: 6.10.0 would be between 6.1.0 and 6.2.0 numerically,
        //  but lexicographically "6.10.0" < "6.2.0" because '1' < '2')
        let mut versions = vec![
            "6.1.0".to_string(),
            "6.10.0".to_string(),
            "6.2.0".to_string(),
        ];
        let result = select_kernel_version(&mut versions).unwrap();
        // Lexicographic sort: "6.1.0", "6.10.0", "6.2.0" → last is "6.2.0"
        assert_eq!(result, "6.2.0");
    }

    #[test]
    fn select_kernel_version_with_dash_suffixes() {
        // Versions like "6.6.60-rt" vs "6.6.60".
        // Shorter string sorts first: "6.6.60" < "6.6.60-rt".
        let mut versions = vec!["6.6.60-rt".to_string(), "6.6.60".to_string()];
        let result = select_kernel_version(&mut versions).unwrap();
        // After sort: "6.6.60", "6.6.60-rt" → last is "6.6.60-rt"
        assert_eq!(result, "6.6.60-rt");
    }

    #[test]
    fn select_kernel_version_empty_is_error() {
        let mut versions: Vec<String> = vec![];
        let result = select_kernel_version(&mut versions);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no kernel versions")
        );
    }

    #[test]
    fn agent_init_script_uses_short_usleep_retries() {
        // Mount retries use 5ms.
        assert!(AGENT_INIT_SCRIPT.contains("/bin/usleep 5000"));
        // Deploy.env wait uses 10ms.
        assert!(AGENT_INIT_SCRIPT.contains("/bin/usleep 10000"));

        // Old 100ms sleep must not be present.
        assert!(!AGENT_INIT_SCRIPT.contains("usleep 100000"));
        // Old sleep 0.01 must not be present.
        assert!(!AGENT_INIT_SCRIPT.contains("sleep 0.01"));

        // Old fallback pattern must not be present.
        assert!(!AGENT_INIT_SCRIPT.contains("/bin/sleep 0.01 2>/dev/null || /bin/sleep 1"));
        assert!(!AGENT_INIT_SCRIPT.contains("/bin/usleep 100000 2>/dev/null || /bin/sleep 1"));
    }

    #[test]
    fn agent_initramfs_cache_version_is_v3() {
        assert_eq!(AGENT_INITRAMFS_BASENAME, "agent-initramfs-v3.cpio");
    }

    #[test]
    fn busybox_agent_symlinks_include_usleep() {
        assert!(AGENT_BUSYBOX_APPLETS.contains(&"usleep"));
        // Spot-check a few other expected applets.
        assert!(AGENT_BUSYBOX_APPLETS.contains(&"sh"));
        assert!(AGENT_BUSYBOX_APPLETS.contains(&"mount"));
        assert!(AGENT_BUSYBOX_APPLETS.contains(&"ip"));
        assert!(AGENT_BUSYBOX_APPLETS.contains(&"sleep"));
    }

    /// Regression: F-27 pack_cpio must set cwd on cpio and must not use
    /// Command::output() (which would discard the archive file and SIGPIPE find).
    #[test]
    fn pack_cpio_blocking_writes_nonempty_archive() {
        let bb = std::env::var_os("RUSSEL_TEST_BUSYBOX")
            .map(PathBuf::from)
            .or_else(|| {
                // Prefer a nix-built busybox if present on PATH as multi-call.
                which_busybox()
            });
        let Some(bb) = bb else {
            eprintln!("skip pack_cpio_blocking test: no busybox (set RUSSEL_TEST_BUSYBOX)");
            return;
        };

        let work = tempfile::tempdir().unwrap();
        let bin = work.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("hello"), b"hi").unwrap();
        std::fs::write(work.path().join("init"), b"#!/bin/sh\n").unwrap();

        let out = work.path().join("out.cpio");
        pack_cpio_blocking(work.path(), &out, bb.to_str().unwrap()).unwrap();
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 64, "expected non-trivial cpio, got {size} bytes");
    }

    fn which_busybox() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("busybox");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // Known store path from recent deploys (optional local convenience).
        let store = PathBuf::from(
            "/nix/store/4s514kmhnmncvcsvjh3d17y7y0psbyc1-busybox-1.37.0/bin/busybox",
        );
        store.is_file().then_some(store)
    }
}
