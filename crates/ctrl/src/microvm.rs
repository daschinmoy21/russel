use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
};

use crate::network::SubnetAllocation;

#[derive(Debug, Clone)]
pub struct MicrovmRunner {
    kernel_cache: Arc<Mutex<Option<PathBuf>>>,
    busybox_cache: Arc<Mutex<Option<PathBuf>>>,
    modules_cache: Arc<Mutex<Option<PathBuf>>>,
}

impl Default for MicrovmRunner {
    fn default() -> Self {
        Self {
            kernel_cache: Arc::new(Mutex::new(None)),
            busybox_cache: Arc::new(Mutex::new(None)),
            modules_cache: Arc::new(Mutex::new(None)),
        }
    }
}

impl MicrovmRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or build the Linux kernel (bzImage) from nixpkgs.
    /// Result is cached in-memory for the lifetime of the runner.
    pub async fn ensure_kernel(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = self.check_cache(&self.kernel_cache) {
            return Ok(path);
        }

        let system = crate::build::current_system().await;
        tracing::info!(system = %system, "building kernel from nixpkgs (cached after first run)");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
                "--argstr",
                "system",
                &system,
                "linux",
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to build kernel from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let kernel = PathBuf::from(format!("{}/bzImage", store_path));

        if let Ok(mut cache) = self.kernel_cache.lock() {
            *cache = Some(kernel.clone());
        } else {
            tracing::warn!("kernel cache lock poisoned, skipping cache update");
        }
        tracing::info!(kernel = %kernel.display(), "kernel cached");
        Ok(kernel)
    }

    /// Get or build busybox from nixpkgs (provides sh, mount, ip, etc.).
    pub async fn ensure_busybox(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = self.check_cache(&self.busybox_cache) {
            return Ok(path);
        }

        let system = crate::build::current_system().await;
        tracing::info!(system = %system, "building busybox from nixpkgs (cached after first run)");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
                "--argstr",
                "system",
                &system,
                "busybox",
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to build busybox from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if let Ok(mut cache) = self.busybox_cache.lock() {
            *cache = Some(PathBuf::from(store_path.clone()));
        } else {
            tracing::warn!("busybox cache lock poisoned, skipping cache update");
        }
        tracing::info!(busybox = %store_path, "busybox cached");
        Ok(PathBuf::from(store_path))
    }

    /// Get or resolve the kernel modules path matching the kernel.
    /// The stock nixpkgs kernel compiles virtio drivers as modules (=m),
    /// so we need these to load them in the initramfs init script.
    pub async fn ensure_kernel_modules(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = self.check_cache(&self.modules_cache) {
            return Ok(path);
        }

        let system = crate::build::current_system().await;
        tracing::info!(system = %system, "resolving kernel modules from nixpkgs");
        let expr = format!(
            "let pkgs = import <nixpkgs> {{ system = \"{}\"; }}; in pkgs.linux.modules",
            system
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

        if let Ok(mut cache) = self.modules_cache.lock() {
            *cache = Some(PathBuf::from(store_path.clone()));
        } else {
            tracing::warn!("kernel modules cache lock poisoned, skipping cache update");
        }
        tracing::info!(modules = %store_path, "kernel modules cached");
        Ok(PathBuf::from(store_path))
    }

    fn check_cache(&self, cache: &Mutex<Option<PathBuf>>) -> Option<PathBuf> {
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

    /// Build a minimal CPIO initramfs containing ONLY:
    ///   - /init   shell script (#!/bin/sh via busybox)
    ///   - /bin/   symlinks to busybox for sh, ip, mount, mkdir, insmod, xzcat
    ///   - busybox runtime closure (hard-linked from /nix/store)
    ///   - /modules/ — virtio .ko.xz kernel modules for networking + virtiofs
    ///
    /// The app binary is NOT included — it is accessed via a virtiofs mount
    /// of the host's /nix/store into the guest (set up in `boot()`).
    pub async fn build_initramfs(
        &self,
        service_id: &str,
        alloc: &SubnetAllocation,
        guest_port: u16,
        app_store_path: &Path,
        bin_name: &str,
        busybox_path: &Path,
        kernel_modules_path: &Path,
    ) -> anyhow::Result<PathBuf> {
        let deploy_dir = PathBuf::from(format!("/var/lib/russel/{}", service_id));
        let initramfs_file = deploy_dir.join("initramfs.cpio");
        let work = deploy_dir.join("initramfs.d");

        if let Err(e) = std::fs::remove_dir_all(&work)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %work.display(), error = %e, "failed to remove previous initramfs work dir");
        }
        std::fs::create_dir_all(&work)?;

        let needed_modules: &[&str] = &[
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
        self.copy_kernel_modules(kernel_modules_path, &work, needed_modules)?;

        let bb_bin = format!("{}/bin/busybox", busybox_path.display());
        let init =
            self.generate_init_script(alloc, guest_port, app_store_path, bin_name, needed_modules);
        use std::os::unix::fs::PermissionsExt;
        let init_path = work.join("init");
        std::fs::write(&init_path, &init)?;
        std::fs::set_permissions(&init_path, std::fs::Permissions::from_mode(0o755))?;

        self.create_busybox_symlinks(&work, &bb_bin)?;
        self.copy_closure_to(busybox_path, &work).await?;
        self.pack_cpio(&work, &initramfs_file, &bb_bin).await?;

        if let Err(e) = std::fs::remove_dir_all(&work) {
            tracing::warn!(dir = %work.display(), error = %e, "failed to remove initramfs work dir");
        }

        tracing::info!(
            initramfs = %initramfs_file.display(),
            size_bytes = std::fs::metadata(&initramfs_file).map(|m| m.len()).unwrap_or(0),
            "initramfs built (minimal: busybox + virtio modules, app via virtiofs)"
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
                tracing::warn!(module = %module_rel, "kernel module not found, skipping");
            }
        }
        Ok(())
    }

    fn generate_init_script(
        &self,
        alloc: &SubnetAllocation,
        guest_port: u16,
        app_store_path: &Path,
        bin_name: &str,
        needed_modules: &[&str],
    ) -> String {
        let app = app_store_path.display();

        let insmod_cmds: String = needed_modules
            .iter()
            .map(|m| {
                let xz_name = Path::new(m)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(m);
                let ko_name = xz_name.trim_end_matches(".xz");
                format!(
                    "if [ -f /modules/{xz_name} ]; then /bin/xzcat /modules/{xz_name} > /tmp/{ko_name} && /bin/insmod /tmp/{ko_name} || exit 1; fi"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"#!/bin/sh
/bin/mkdir -p /proc /sys /dev /nix/store /tmp
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs devtmpfs /dev

# Load the small set of virtio modules needed by this guest.
{insmod_cmds}

# Mount the host Nix store before configuring the network.
/bin/mount -t virtiofs nixstore /nix/store
if [ $? -ne 0 ]; then
  echo "ERROR: Failed to mount /nix/store via virtiofs"
  echo "Spawning emergency shell..."
  exec /bin/sh
fi

echo "=== Configuring networking ==="
/bin/ip addr add {vm_ip}/30 dev eth0
if [ $? -ne 0 ]; then
  echo "ERROR: Failed to assign IP {vm_ip}/30 to eth0"
fi

/bin/ip link set eth0 up
if [ $? -ne 0 ]; then
  echo "ERROR: Failed to bring eth0 UP"
fi

/bin/ip route add default via {host_ip}
if [ $? -ne 0 ]; then
  echo "ERROR: Failed to set default gateway {host_ip}"
fi

export PORT={port}
cd /
echo "exec {app}/bin/{bin}"
exec {app}/bin/{bin}
echo "ERROR: exec failed! Spawning emergency shell..."
exec /bin/sh
"#,
            insmod_cmds = insmod_cmds,
            vm_ip = alloc.vm_ip,
            host_ip = alloc.host_ip,
            port = guest_port,
            app = app,
            bin = bin_name,
        )
    }

    fn create_busybox_symlinks(&self, work: &Path, bb_bin: &str) -> anyhow::Result<()> {
        let bin_dir = work.join("bin");
        std::fs::create_dir_all(&bin_dir)?;
        for name in &["sh", "mount", "ip", "mkdir", "insmod", "xzcat", "cat"] {
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

    async fn pack_cpio(
        &self,
        work: &Path,
        initramfs_file: &Path,
        bb_bin: &str,
    ) -> anyhow::Result<()> {
        let script = format!(
            "cd '{}' && '{}' find . | '{}' cpio -o -H newc > '{}'",
            work.display(),
            bb_bin,
            bb_bin,
            initramfs_file.display(),
        );
        let status = Command::new(bb_bin)
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .await?;

        if !status.status.success() {
            anyhow::bail!(
                "cpio failed to create initramfs: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            );
        }
        Ok(())
    }

    /// Find the kernel version subdirectory in a modules store path.
    fn find_kver(&self, modules_path: &Path) -> anyhow::Result<String> {
        let mods_dir = modules_path.join("lib/modules");
        for entry in std::fs::read_dir(&mods_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                return Ok(name.to_string());
            }
        }
        anyhow::bail!(
            "no kernel version directory found in {}",
            mods_dir.display()
        )
    }

    /// Copy a Nix store path and all its runtime dependencies into `dest_root`
    /// using hard-links for files (instant, no data copy).
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
                continue; // already copied (shared dep between app + busybox)
            }
            self.copy_path_tree(Path::new(src), dest_root)?;
        }
        Ok(())
    }

    /// Recursively copy a directory tree from `src` into `dest_root`,
    /// hard-linking regular files and symlinks, creating directories as needed.
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
            // Hard-link regular files; fall back to copy on cross-device error
            if std::fs::hard_link(src, &dest).is_err() {
                std::fs::copy(src, &dest)?;
            }
        }
        Ok(())
    }

    /// Boot cloud-hypervisor directly (no systemd, no NixOS).
    /// Shares the host's /nix/store into the guest via virtiofs so the
    /// initramfs only needs busybox — the app binary is accessed from the mount.
    ///
    /// Spawns `virtiofsd` first (to serve /nix/store), then boots the VMM.
    /// Returns both processes — the caller must keep both alive.
    pub async fn boot(
        &self,
        service_id: &str,
        kernel_path: &Path,
        initramfs_path: &Path,
        alloc: &SubnetAllocation,
        memory_mb: u16,
    ) -> anyhow::Result<BootOutput> {
        let tap = &alloc.tap_id;
        let mac = &alloc.mac;

        tracing::info!(tap, mac, kernel = %kernel_path.display(), "booting cloud-hypervisor");

        // memory_mb must be at least 256 and use shared=on for virtiofs
        let mem_mb = memory_mb.max(256);

        // ── 1. Spawn virtiofsd to serve /nix/store via a per-VM socket ───
        let sock_dir = format!("/var/lib/russel/{}", service_id);
        std::fs::create_dir_all(&sock_dir)?;

        let virtiofs_sock = format!("{}/virtiofs.sock", sock_dir);
        if let Err(e) = std::fs::remove_file(&virtiofs_sock)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %virtiofs_sock, error = %e, "failed to remove stale virtiofs socket");
        }

        // Cloud Hypervisor owns the VM lifecycle. Keep its API socket alongside
        // the other per-service state so stop/destroy do not need process-wide
        // process matching in the normal case.
        let api_sock = format!("{}/cloud-hypervisor.sock", sock_dir);
        if let Err(e) = std::fs::remove_file(&api_sock)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %api_sock, error = %e, "failed to remove stale Cloud Hypervisor API socket");
        }

        // ponytail: --readonly ensures the guest cannot write to /nix/store.
        // Remove if a deployment workflow ever needs guest-side store mutations.
        tracing::info!(socket = %virtiofs_sock, "spawning virtiofsd for /nix/store (read-only)");
        let virtiofsd_child = Command::new("virtiofsd")
            .arg(format!("--socket-path={}", virtiofs_sock))
            .arg("--shared-dir=/nix/store")
            .arg("--readonly")
            .arg("--sandbox=none")
            .arg("--cache=always")
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to spawn virtiofsd: {}. \
                     Install with: nix-env -iA nixpkgs.virtiofsd",
                    e
                )
            })?;
        tracing::info!(pid = virtiofsd_child.id(), "virtiofsd started");

        // virtiofsd normally creates its socket immediately.  Do not add a
        // fixed boot delay here: wait only until it is actually ready and fail
        // clearly if it never comes up.
        let socket_deadline = Instant::now() + Duration::from_millis(200);
        while !std::path::Path::new(&virtiofs_sock).exists() && Instant::now() < socket_deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if !std::path::Path::new(&virtiofs_sock).exists() {
            anyhow::bail!("virtiofsd did not create socket {virtiofs_sock} within 200ms");
        }

        // ── 2. Boot cloud-hypervisor ─────────────────────────────────────
        let child = Command::new("cloud-hypervisor")
            .arg("--kernel")
            .arg(kernel_path)
            .arg("--initramfs")
            .arg(initramfs_path)
            .arg("--cmdline")
            // No serial console: kernel output is not part of readiness and
            // writing it to a per-VM file adds avoidable boot I/O.
            .arg("panic=-1 random.trust_cpu=on")
            .arg("--cpus")
            .arg("boot=1")
            .arg("--memory")
            .arg(format!("size={}M,shared=on", mem_mb))
            .arg("--net")
            .arg(format!("tap={},mac={}", tap, mac))
            .arg("--fs")
            .arg(format!(
                "tag=nixstore,socket={},num_queues=1,queue_size=512",
                virtiofs_sock
            ))
            .arg("--console")
            .arg("null")
            .arg("--api-socket")
            .arg(format!("path={}", api_sock))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to spawn cloud-hypervisor: {}. \
                     Install with: nix-env -iA nixpkgs.cloud-hypervisor \
                     (or add to your devShell)",
                    e
                )
            })?;

        tracing::info!(pid = child.id(), "cloud-hypervisor started");
        Ok(BootOutput {
            vm_child: child,
            virtiofsd_child,
        })
    }

    /// Gracefully stop a VM through Cloud Hypervisor's REST API.
    ///
    /// The VMM is deliberately not killed on the normal path. `vm.shutdown`
    /// asks the guest to power off and `vmm.shutdown` then asks Cloud Hypervisor
    /// itself to exit. If the API is unavailable or the process does not exit in
    /// time, cleanup falls back to a service-scoped `pkill`.
    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        Self::validate_service_id(service_id)?;

        let metadata = read_metadata(service_id);
        let api_socket = format!("/var/lib/russel/{service_id}/cloud-hypervisor.sock");
        // `destroy` is intentionally idempotent. A first cleanup attempt for a
        // benchmark VM normally has neither metadata nor a VMM socket; do not
        // turn that expected case into a shutdown warning or process scan.
        if metadata.is_none() && !Path::new(&api_socket).exists() {
            return Ok(());
        }
        let vm_pid = metadata.as_ref().and_then(|m| m.vm_pid);
        let api_result = self.shutdown_via_api(service_id).await;

        let vm_stopped = if api_result.is_ok() {
            match vm_pid {
                Some(pid) => wait_for_process_exit(pid, Duration::from_secs(5)).await,
                None => true,
            }
        } else {
            if let Err(error) = api_result {
                tracing::warn!(service_id, error = %error, "Cloud Hypervisor shutdown failed; using process fallback");
            }
            false
        };

        if !vm_stopped {
            let tap = crate::network::subnet_for(service_id).tap_id;
            self.pkill_service_process(
                service_id,
                "cloud-hypervisor",
                &format!("cloud-hypervisor.*tap={}(,|$)", escape_regex(&tap)),
            )
            .await?;
            if let Some(pid) = vm_pid {
                let _ = wait_for_process_exit(pid, Duration::from_secs(2)).await;
            }
        }

        // Cloud Hypervisor does not own these helper processes. Prefer the PIDs
        // recorded at boot, and only use the scoped pattern fallback if a helper
        // is still alive or the VM was recovered without metadata.
        for (pid, kind, pattern) in [
            (
                metadata.as_ref().and_then(|m| m.virtiofsd_pid),
                "virtiofsd",
                format!("virtiofsd.*russel/{}/", escape_regex(service_id)),
            ),
            (
                metadata.as_ref().and_then(|m| m.socat_pid),
                "socat",
                format!("socat-russel-{}", escape_regex(service_id)),
            ),
        ] {
            if let Some(pid) = pid
                && terminate_owned_process(pid, service_id).await?
            {
                continue;
            }
            self.pkill_service_process(service_id, kind, &pattern)
                .await?;
        }

        Ok(())
    }

    async fn shutdown_via_api(&self, service_id: &str) -> anyhow::Result<()> {
        self.cloud_hypervisor_api_request(service_id, "vm.shutdown")
            .await
            .map_err(|e| anyhow::anyhow!("vm.shutdown: {e}"))?;
        self.cloud_hypervisor_api_request(service_id, "vmm.shutdown")
            .await
            .map_err(|e| anyhow::anyhow!("vmm.shutdown: {e}"))?;
        Ok(())
    }

    async fn cloud_hypervisor_api_request(
        &self,
        service_id: &str,
        endpoint: &str,
    ) -> anyhow::Result<()> {
        let socket = format!("/var/lib/russel/{service_id}/cloud-hypervisor.sock");
        let timeout = Duration::from_secs(3);
        let mut stream = tokio::time::timeout(timeout, UnixStream::connect(&socket))
            .await
            .map_err(|_| anyhow::anyhow!("timed out connecting to {socket}"))??;
        let request = format!(
            "PUT /api/v1/{endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        tokio::time::timeout(timeout, stream.write_all(request.as_bytes()))
            .await
            .map_err(|_| anyhow::anyhow!("timed out writing {endpoint}"))??;

        // Cloud Hypervisor may keep the API connection open after sending a
        // shutdown response. Waiting for EOF here makes every shutdown pay the
        // full timeout even though the request succeeded. The status and headers
        // are enough for these action endpoints, so stop at the end of headers.
        let mut response = Vec::with_capacity(1024);
        tokio::time::timeout(timeout, async {
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..read]);
                if response.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if response.len() > 64 * 1024 {
                    anyhow::bail!("response headers from {endpoint} are too large");
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for {endpoint}"))??;
        let status_line = String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| anyhow::anyhow!("invalid response from {endpoint}: {status_line}"))?;
        if !(200..300).contains(&status) {
            anyhow::bail!("Cloud Hypervisor returned HTTP {status} for {endpoint}");
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
        // pkill uses exit code 1 when no process matched, which is a successful
        // outcome for idempotent stop/destroy operations.
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!(
                "pkill for {process_kind} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        tracing::debug!(service_id, process_kind, "process fallback completed");
        Ok(())
    }

    /// Verify that a PID still belongs to a process associated with service_id.
    /// This prevents a reused PID from causing an unrelated process to be killed.
    fn verify_process_ownership(pid: u32, service_id: &str) -> bool {
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let tap_arg = format!("tap={}", crate::network::subnet_for(service_id).tap_id);
        // `/proc/<pid>/cmdline` is NUL-separated bytes, not a text file. Read
        // bytes directly so an unexpected non-UTF-8 argument cannot make an
        // owned helper look unrelated and trigger a broad process fallback.
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
        // Validate service_id before using it in any paths
        Self::validate_service_id(service_id)?;

        self.stop(service_id).await?;

        let alloc = crate::network::subnet_for(service_id);
        crate::network::TapForwarder::teardown(&alloc).await?;

        crate::network::PortAllocator::release(service_id);

        for dir in &[
            format!("/var/lib/microvms/{}", service_id),
            format!("/var/lib/russel/{}", service_id),
        ] {
            let path = std::path::Path::new(dir);
            if path.exists() {
                let out = Command::new("rm").args(["-rf", dir]).output().await?;
                if !out.status.success() {
                    anyhow::bail!(
                        "failed to remove directory {}: {}",
                        dir,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }
        for file in &[
            format!("/nix/var/nix/gcroots/microvm/{}", service_id),
            format!("/nix/var/nix/gcroots/microvm/booted-{}", service_id),
        ] {
            let path = std::path::Path::new(file);
            if path.exists() {
                let out = Command::new("rm").args(["-f", file]).output().await?;
                if !out.status.success() {
                    anyhow::bail!(
                        "failed to remove gcroot {}: {}",
                        file,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }
        Ok(())
    }

    /// Validate service_id to prevent path traversal and ensure it's a safe identifier.
    pub fn validate_service_id(service_id: &str) -> anyhow::Result<()> {
        if service_id.is_empty() {
            anyhow::bail!("service_id cannot be empty");
        }
        if service_id.len() > 128 {
            anyhow::bail!("service_id too long (max 128 characters)");
        }
        // Check for path separators and traversal components
        if service_id.contains('/') || service_id.contains('\\') {
            anyhow::bail!("service_id cannot contain path separators");
        }
        if service_id.contains("..") || service_id == "." {
            anyhow::bail!("service_id cannot contain path traversal components");
        }
        // Ensure it only contains safe characters (alphanumeric, dash, underscore)
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

/// Output of `MicrovmRunner::boot()` — both child processes must be kept alive.
pub struct BootOutput {
    pub vm_child: tokio::process::Child,
    pub virtiofsd_child: tokio::process::Child,
}

#[derive(Debug, Default)]
struct ProcessMetadata {
    vm_pid: Option<u32>,
    virtiofsd_pid: Option<u32>,
    socat_pid: Option<u32>,
}

fn read_metadata(service_id: &str) -> Option<ProcessMetadata> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(ProcessMetadata {
        vm_pid: value
            .get("vm_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        virtiofsd_pid: value
            .get("virtiofsd_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        socat_pid: value
            .get("socat_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
    })
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
    // The state is the first field after the executable name in /proc/pid/stat.
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !process_is_alive(pid)
}

fn escape_regex(s: &str) -> String {
    let mut escaped = String::new();
    for c in s.chars() {
        if ".+*?^$()[]{}|\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
