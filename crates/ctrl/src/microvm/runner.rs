//! MicrovmRunner: kernel/busybox/initramfs caches, boot, stop, destroy.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::process::Command;

use crate::ch_api;

use super::agent::{AGENT_BUSYBOX_APPLETS, AGENT_INIT_SCRIPT, AGENT_INITRAMFS_BASENAME};
use super::process::{
    BootOutput, network_alloc_for_service, read_metadata, terminate_owned_process,
    wait_for_process_exit,
};
use super::spec::{KernelInfo, VmSpec};

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
        if let Some(info) = self.check_kernel_cache() {
            return Ok(info);
        }

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

    // Busybox (still needed for initramfs).

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

    // Kernel modules (only needed when falling back to stock kernel).

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

    // Agent initramfs (generic, config-driven via deploy.env).

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

        tokio::task::spawn_blocking(move || {
            super::agent::pack_cpio_blocking(&work, &initramfs_file, &bb_bin)
        })
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
        let socat_killed = match metadata.as_ref().and_then(|m| m.socat_pid) {
            Some(pid) => terminate_owned_process(pid, service_id).await?,
            None => false,
        };
        if !socat_killed {
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
        // Reject host state trees (secrets/traefik/_pool/*.bak) so deploy/destroy
        // cannot wipe /var/lib/russel/{secrets,traefik,_pool} or backup dirs.
        if crate::metadata::is_reserved_service_dir(service_id) {
            anyhow::bail!("service_id is reserved: {service_id}");
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
/// Select the greatest kernel version from a list of version strings.
/// Sorts lexicographically and returns the last; exported for unit testing.
pub(super) fn select_kernel_version(versions: &mut [String]) -> anyhow::Result<String> {
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

#[async_trait::async_trait]
impl crate::runtime::RuntimeLifecycle for MicrovmRunner {
    async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        self.stop(service_id).await
    }

    async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        self.destroy(service_id).await
    }
}
