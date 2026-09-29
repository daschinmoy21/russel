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
    BootOutput, cloud_hypervisor_stop_pattern, escape_pkill_literal, network_alloc_for_service,
    read_metadata, stop_tap_identity, terminate_owned_process, virtiofsd_stop_pattern,
    wait_for_process_exit,
};
use super::spec::{FsMount, KernelInfo, VmSpec};

/// Shared runner singleton — kernel/busybox/modules caches live across deploys.
pub(crate) fn shared_runner() -> MicrovmRunner {
    static RUNNER: std::sync::LazyLock<MicrovmRunner> =
        std::sync::LazyLock::new(MicrovmRunner::new);
    RUNNER.clone()
}

/// virtiofsd sandbox mode from RUSSEL_VIRTIOFS_SANDBOX. Default: chroot as
/// root, namespace otherwise (chroot needs root; namespace uses an
/// unprivileged user namespace). Set RUSSEL_VIRTIOFS_SANDBOX=none for escape hatch.
pub(crate) fn virtiofsd_sandbox() -> String {
    std::env::var("RUSSEL_VIRTIOFS_SANDBOX")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Safety: geteuid is a pure POSIX query of this process.
            if unsafe { libc::geteuid() } == 0 {
                "chroot".to_string()
            } else {
                "namespace".to_string()
            }
        })
}

#[derive(Debug, Clone)]
pub struct MicrovmRunner {
    kernel_cache: Arc<Mutex<Option<KernelInfo>>>,
    busybox_cache: Arc<Mutex<Option<PathBuf>>>,
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
    ///   2. `RUSSEL_KERNEL_PATH` env var
    ///   3. `RUSSEL_KERNEL_POOL` env var, else `/var/lib/russel/_pool/kernel/bzImage`
    ///   4. `nix build .#microvm-kernel` (flake attr, detected via flake.nix)
    ///   5. Hard error naming the pool path, `RUSSEL_KERNEL_PATH`, and
    ///      `nix build .#microvm-kernel` — never falls back to the stock
    ///      nixpkgs linux kernel (drivers =m breaks microVM boot)
    ///
    /// Result is cached in-memory for the lifetime of the runner.
    pub async fn ensure_kernel(&self) -> anyhow::Result<KernelInfo> {
        if let Some(info) = self.check_kernel_cache() {
            return Ok(info);
        }

        if let Ok(env_path) = std::env::var("RUSSEL_KERNEL_PATH") {
            let p = PathBuf::from(&env_path);
            if p.exists() {
                let info = KernelInfo { path: p };
                self.store_kernel_cache(info.clone());
                tracing::info!(
                    kernel = %info.path.display(),
                    source = "RUSSEL_KERNEL_PATH",
                    "kernel cached"
                );
                return Ok(info);
            }
            tracing::warn!("RUSSEL_KERNEL_PATH={env_path} does not exist, continuing");
        }

        let pool = match std::env::var("RUSSEL_KERNEL_POOL") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
            _ => crate::paths::data_root().join("_pool/kernel/bzImage"),
        };
        if pool.is_file() {
            let info = KernelInfo { path: pool };
            self.store_kernel_cache(info.clone());
            tracing::info!(
                kernel = %info.path.display(),
                source = "kernel-pool",
                "microvm kernel cached"
            );
            return Ok(info);
        }
        tracing::debug!(pool = %pool.display(), "kernel pool has no kernel file");

        if let Some(repo_root) = Self::find_repo_root()
            && repo_root.join("flake.nix").exists()
        {
            match self.build_flake_kernel(&repo_root).await {
                Ok(path) => {
                    let info = KernelInfo { path };
                    self.store_kernel_cache(info.clone());
                    tracing::info!(
                        kernel = %info.path.display(),
                        source = "flake",
                        "microvm kernel cached"
                    );
                    return Ok(info);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "flake kernel build failed");
                }
            }
        }

        anyhow::bail!(
            "no microVM kernel available: there is no kernel file at {pool}, \
             RUSSEL_KERNEL_PATH is unset or points to a missing file, and \
             `nix build .#microvm-kernel` did not produce a kernel. Place a \
             bzImage at {pool} (override the location with RUSSEL_KERNEL_POOL), \
             set RUSSEL_KERNEL_PATH, or run `nix build .#microvm-kernel` in the \
             repo root. Refusing to fall back to the stock nixpkgs linux kernel \
             (virtio drivers =m breaks microVM boot).",
            pool = pool.display()
        );
    }

    /// Find the repo root by looking for flake.nix upward from
    /// CARGO_MANIFEST_DIR (compile-time) or cwd (runtime).
    fn find_repo_root() -> Option<PathBuf> {
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
    ///   2. mounts virtiofs `nixstore` → `/nix/store` (RO)
    ///   3. mounts virtiofs `russelcfg` → `/config` (RO, host `deploy.env`)
    ///   4. mounts virtiofs `russelscratch` → `/run/russel` (RW)
    ///   5. echoes `ready` into `/run/russel/.agent_ready`
    ///   6. waits until `/config/deploy.env` exists
    ///   7. sources `/config/deploy.env` (expects `VM_IP`, `HOST_IP`, `PORT`, `APP`)
    ///   8. configures eth0 + default route; `exec $APP`
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
        let pool_dir = crate::paths::data_root().join("_pool");
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

        let bb_bin = format!("{}/bin/busybox", busybox_path.display());
        let init = AGENT_INIT_SCRIPT;

        use std::os::unix::fs::PermissionsExt;
        let init_path = work.join("init");
        std::fs::write(&init_path, init)?;
        std::fs::set_permissions(&init_path, std::fs::Permissions::from_mode(0o755))?;

        self.create_busybox_symlinks(&work, &bb_bin)?;
        self.copy_closure_to(&busybox_path, &work).await?;
        // Build to a temp file, then atomic rename (F-18).
        let tmp_cpio = pool_dir.join(format!(".{AGENT_INITRAMFS_BASENAME}.{pid}.{nanos}.tmp"));
        self.pack_cpio(&work, &tmp_cpio, &bb_bin).await?;
        std::fs::rename(&tmp_cpio, &initramfs_file)?;

        self.store_path_cache(&self.agent_initramfs_cache, initramfs_file.clone());

        tracing::info!(
            initramfs = %initramfs_file.display(),
            "agent initramfs built (config-driven, no app baked in)"
        );
        Ok(initramfs_file)
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
            net = ?spec.net,
            mac = %spec.mac,
            kernel = %spec.kernel.display(),
            cpus = %super::spec::cpus_arg(spec.cpus_boot, spec.cpus_max),
            memory = format!("{}M,hotplug_size={}M", spec.memory_mb, spec.memory_hotplug_mb),
            "booting cloud-hypervisor"
        );
        // Before any virtiofsd starts, so a refused boot leaves nothing behind.
        // TAP VMs boot fine on older CH (#510); only the vhost-user NIC hits it.
        if spec.restore_url.is_none() && matches!(spec.net, crate::network::VmNet::VhostUser(_)) {
            super::preflight::check_ch_supports_cpus(spec.cpus_boot).await?;
        }
        crate::paths::check_unix_socket_path(&spec.api_socket)?;
        for fs in &spec.fs {
            crate::paths::check_unix_socket_path(&fs.socket)?;
        }

        let sock_dir = spec
            .api_socket
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(crate::paths::data_root);

        std::fs::create_dir_all(&sock_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut virtiofsd_children: Vec<tokio::process::Child> = Vec::new();
        match spec.fs.as_slice() {
            [] => {}
            [a, b] => {
                let (ra, rb) = tokio::join!(self.spawn_virtiofsd(a), self.spawn_virtiofsd(b),);
                virtiofsd_children.push(ra?);
                virtiofsd_children.push(rb?);
            }
            [a, b, c] => {
                let (ra, rb, rc) = tokio::join!(
                    self.spawn_virtiofsd(a),
                    self.spawn_virtiofsd(b),
                    self.spawn_virtiofsd(c),
                );
                virtiofsd_children.push(ra?);
                virtiofsd_children.push(rb?);
                virtiofsd_children.push(rc?);
            }
            _ => {
                for fs in &spec.fs {
                    let child = self.spawn_virtiofsd(fs).await?;
                    virtiofsd_children.push(child);
                }
            }
        }
        let mem_mb = spec.memory_mb.max(256);

        // A VM that died leaves its API socket behind, and cloud-hypervisor
        // refuses to start on it (EADDRINUSE). Relaunches in place hit this.
        if let Err(e) = std::fs::remove_file(&spec.api_socket)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %spec.api_socket.display(), error = %e, "failed to remove stale API socket");
        }

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
                .arg(super::spec::cpus_arg(spec.cpus_boot, spec.cpus_max))
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
                .arg(spec.net.ch_arg(&spec.mac))
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

    /// Spawn virtiofsd for one share and wait for readiness.
    pub(crate) async fn spawn_virtiofsd(
        &self,
        fs: &FsMount,
    ) -> anyhow::Result<tokio::process::Child> {
        let (socket, shared_dir, readonly) = (&fs.socket, &fs.shared_dir, fs.readonly);
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
        let cache_policy = fs.cache.as_str();
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
        russel_core::ids::validate_service_id(service_id)?;

        let metadata = read_metadata(service_id);
        let api_socket = crate::paths::service_dir(service_id)
            .join("cloud-hypervisor.sock")
            .display()
            .to_string();
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

        let vm_stopped = match api_result {
            Ok(()) => match vm_pid {
                Some(pid) => wait_for_process_exit(pid, Duration::from_secs(5)).await,
                None => true,
            },
            Err(error) => {
                // Cloud Hypervisor can exit on vm.shutdown before it answers, so
                // a failed request does not mean the VM is still running.
                let exited = match vm_pid {
                    Some(pid) => wait_for_process_exit(pid, Duration::from_secs(2)).await,
                    None => false,
                };
                if !exited {
                    tracing::warn!(service_id, error = %error, "CH API shutdown failed; using process fallback");
                }
                exited
            }
        };

        if !vm_stopped {
            // Metadata TAP → registry lease → service-path marker. Never
            // preferred_subnet / subnet_for (wrong TAP or phantom lease).
            // A passt VM has no TAP; match it by its service path instead.
            let tap = match metadata.as_ref().map(|m| m.net_mode) {
                Some(crate::network::MicrovmNetMode::Passt) => None,
                _ => stop_tap_identity(
                    service_id,
                    metadata.as_ref().and_then(|m| m.tap_id.as_deref()),
                ),
            };
            let pattern = cloud_hypervisor_stop_pattern(service_id, tap.as_deref());
            self.pkill_service_process(service_id, "cloud-hypervisor", &pattern)
                .await?;
            if let Some(pid) = vm_pid {
                let _ = wait_for_process_exit(pid, Duration::from_secs(2)).await;
            }
        }

        // Kill virtiofsd processes (plural — deploy writes an array).
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
            let virtiofsd_pattern = virtiofsd_stop_pattern(service_id);
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

        // Kill passt (unprivileged network mode, #461).
        // Exact service dir only: during a dual-live cutover the candidate's
        // passt lives under `<id>_g<gen>/` and must survive the old
        // generation's stop. passt exits by itself when its VM goes away.
        let passt_socket = escape_pkill_literal(
            &crate::paths::service_dir(service_id)
                .join(crate::network::PASST_SOCKET)
                .display()
                .to_string(),
        );
        let passt_pattern = format!("(^|[[:space:]])passt .*{passt_socket}( |$)");
        let passt_killed = match metadata.as_ref().and_then(|m| m.passt_pid) {
            Some(pid) => terminate_owned_process(pid, service_id).await?,
            None => false,
        };
        if !passt_killed {
            self.pkill_service_process(service_id, "passt", &passt_pattern)
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
    ///
    /// Best-effort multi-stage cleanup: each step runs even if a prior step
    /// failed. Port and subnet leases are **always** released (idempotent
    /// inventory ownership) even when stop or TAP teardown fails — partial
    /// destroy must not permanently hold the service's port/subnet. Returns
    /// `Ok` only when every step succeeded; otherwise aggregates failures so
    /// callers can leave status `failed` for retry of remaining cleanup.
    ///
    /// Keeps every managed volume: redeploy and failed-deploy cleanup use
    /// this while the next generation still mounts that data.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        self.destroy_inner(service_id, None).await
    }

    /// Operator destroy: managed volumes follow `policy` (each row's `keep`,
    /// or the keep/delete override), the same as a container's (#386).
    pub async fn destroy_with_policy(
        &self,
        service_id: &str,
        policy: russel_core::VolumeDestroyPolicy,
    ) -> anyhow::Result<()> {
        self.destroy_inner(service_id, Some(policy)).await
    }

    async fn destroy_inner(
        &self,
        service_id: &str,
        policy: Option<russel_core::VolumeDestroyPolicy>,
    ) -> anyhow::Result<()> {
        russel_core::ids::validate_service_id(service_id)?;
        // Read before stop; the recorded rows decide which dirs survive.
        let volumes = crate::container::volumes_recorded_for(service_id);

        // Metadata or this service's registry lease only — never invent a
        // hash-preferred TAP that may belong to another collision owner.
        let alloc = network_alloc_for_service(service_id);
        // Without metadata (a deploy that failed before writing it), assume
        // the mode this host would have used.
        let net_mode = match read_metadata(service_id) {
            Some(meta) => meta.net_mode,
            None => crate::network::MicrovmNetMode::for_host().unwrap_or_default(),
        };
        let mut errors: Vec<String> = Vec::new();

        if let Err(e) = self.stop(service_id).await {
            tracing::warn!(service_id, error = %e, "destroy: stop failed; continuing cleanup");
            errors.push(format!("stop: {e}"));
        }

        match alloc {
            Some(ref alloc) => {
                if let Err(e) = crate::network::MicrovmNet::teardown(net_mode, alloc).await {
                    tracing::warn!(
                        service_id,
                        error = %e,
                        "destroy: TAP teardown failed; continuing"
                    );
                    errors.push(format!("tap teardown: {e}"));
                }
            }
            None => {
                tracing::debug!(
                    service_id,
                    "destroy: no metadata or registry lease; skipping TAP teardown"
                );
            }
        }

        // Always free network inventory (idempotent). Control-plane ownership
        // ends with destroy even if process/TAP cleanup was incomplete — the
        // operator retries destroy for residual runtime state without needing
        // the port/subnet leases held indefinitely.
        crate::network::PortAllocator::release(service_id);
        crate::network::release_subnet(service_id);

        // Both dirs keep `volumes/`: with `RUSSEL_DATA_DIR=/var/lib/microvms`
        // the legacy marker dir *is* the service dir, and a plain
        // remove_dir_all there would wipe a destroyed container's kept data.
        let marker_dir = crate::paths::microvm_dir(service_id);
        let service_dir = crate::paths::service_dir(service_id);
        let mut dirs = vec![marker_dir];
        if service_dir != dirs[0] {
            dirs.push(service_dir.clone());
        }
        for dir in &dirs {
            let removed = match policy {
                Some(policy) if *dir == service_dir => {
                    crate::container::cleanup_service_dir_in(dir, &volumes, policy).await
                }
                _ => remove_service_dir_keep_volumes(dir).await,
            };
            if let Err(e) = removed {
                tracing::warn!(service_id, dir = %dir.display(), error = %e, "destroy: directory remove failed");
                errors.push(format!("remove {}: {e}", dir.display()));
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
                tracing::warn!(service_id, file, error = %e, "destroy: gcroot remove failed");
                errors.push(format!("remove gcroot {file}: {e}"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "destroy completed with partial failures: {}",
                errors.join("; ")
            ))
        }
    }
}
/// Remove a microVM service dir, but never its `volumes/` child.
///
/// MicroVMs have no managed volumes. A `volumes/` dir here is data a
/// destroyed container kept (`keep = true`). Without metadata, lifecycle
/// runtime resolution defaults to microVM, so a second destroy of that
/// leftover dir reaches this path and must not wipe the kept data.
pub(super) async fn remove_service_dir_keep_volumes(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if !dir.join("volumes").is_dir() {
        return Ok(crate::container::remove_tree(dir).await?);
    }
    crate::container::remove_service_payload_keep_volumes(dir).await?;
    tracing::warn!(
        dir = %dir.display(),
        "destroy: left kept volumes/ in place (remove it by hand to delete the data)"
    );
    Ok(())
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
