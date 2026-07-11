use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::process::Command;

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

        tracing::info!("building kernel from nixpkgs (cached after first run)");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
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

        tracing::info!("building busybox from nixpkgs (cached after first run)");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
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

        tracing::info!("resolving kernel modules from nixpkgs");
        let output = Command::new("nix")
            .args([
                "build",
                "--impure",
                "--no-link",
                "--print-out-paths",
                "--expr",
                "let pkgs = import <nixpkgs> {}; in pkgs.linux.modules",
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
            if let Some(ref path) = *cache {
                if path.exists() {
                    return Some(path.clone());
                }
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

        if let Err(e) = std::fs::remove_dir_all(&work) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %work.display(), error = %e, "failed to remove previous initramfs work dir");
            }
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
        let init = self.generate_init_script(alloc, guest_port, app_store_path, bin_name, needed_modules);
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
                    "echo \"Loading module {ko_name}...\"\n\
                     /bin/xzcat /modules/{xz_name} > /tmp/{ko_name} && /bin/insmod /tmp/{ko_name} || echo \"FAILED to load {ko_name}\""
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"#!/bin/sh
echo "=== RUSSEL INIT STARTING ==="
/bin/mkdir -p /proc /sys /dev /nix/store /tmp
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs devtmpfs /dev

# Load virtio kernel modules
{insmod_cmds}

echo "=== Mounting /nix/store via virtiofs ==="
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

# Print interface details
echo "=== Network Interfaces ==="
/bin/ip addr show
echo "=== Routes ==="
/bin/ip route show

echo "=== Launching application ==="
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
            if let Err(e) = std::fs::remove_file(&dest) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(file = %dest.display(), error = %e, "failed to remove previous symlink");
                }
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
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    return Ok(name.to_string());
                }
            }
        }
        anyhow::bail!("no kernel version directory found in {}", mods_dir.display())
    }

    /// Copy a Nix store path and all its runtime dependencies into `dest_root`
    /// using hard-links for files (instant, no data copy).
    async fn copy_closure_to(&self, store_path: &Path, dest_root: &Path) -> anyhow::Result<()> {
        let output = Command::new("nix")
            .args([
                "path-info",
                "-r",
                &store_path.display().to_string(),
            ])
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
        let console_log = format!("{}/console.log", sock_dir);
        if let Err(e) = std::fs::remove_file(&console_log) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(file = %console_log, error = %e, "failed to remove previous console log");
            }
        }
        if let Ok(file) = std::fs::File::create(&console_log) {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o666)) {
                tracing::warn!(file = %console_log, error = %e, "failed to set permissions on console log");
            }
        }

        let virtiofs_sock = format!("{}/virtiofs.sock", sock_dir);
        if let Err(e) = std::fs::remove_file(&virtiofs_sock) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(file = %virtiofs_sock, error = %e, "failed to remove stale virtiofs socket");
            }
        }

        tracing::info!(socket = %virtiofs_sock, "spawning virtiofsd for /nix/store");
        let virtiofsd_child = Command::new("virtiofsd")
            .arg(format!("--socket-path={}", virtiofs_sock))
            .arg("--shared-dir=/nix/store")
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

        // Give virtiofsd a moment to create the socket
        for _ in 0..20 {
            if std::path::Path::new(&virtiofs_sock).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // ── 2. Boot cloud-hypervisor ─────────────────────────────────────
        let child = Command::new("cloud-hypervisor")
            .arg("--kernel")
            .arg(kernel_path)
            .arg("--initramfs")
            .arg(initramfs_path)
            .arg("--cmdline")
            .arg("console=ttyS0 panic=-1 random.trust_cpu=on")
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
            .arg("--serial")
            .arg(format!("file=/var/lib/russel/{}/console.log", service_id))
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

    /// Stop a running VM by killing the cloud-hypervisor, virtiofsd, and socat processes.
    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        let alloc = crate::network::subnet_for(service_id);
        let unit = format!("microvm@{}.service", service_id);
        match Command::new("systemctl")
            .args(["stop", &unit])
            .output()
            .await
        {
            Ok(out) if !out.status.success() => {
                tracing::warn!(unit = %unit, "systemctl stop failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Err(e) => {
                tracing::warn!(unit = %unit, error = %e, "failed to run systemctl stop");
            }
            _ => {}
        }

        // Try to read metadata first
        let metadata_path = format!("/var/lib/russel/{}/metadata.json", service_id);
        let mut killed_any = false;
        if let Ok(content) = std::fs::read_to_string(&metadata_path) {
            if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(vm_pid) = metadata.get("vm_pid").and_then(|v| v.as_u64()) {
                    let _ = Command::new("kill").arg(vm_pid.to_string()).output().await;
                }
                if let Some(virtiofsd_pid) = metadata.get("virtiofsd_pid").and_then(|v| v.as_u64()) {
                    let _ = Command::new("kill").arg(virtiofsd_pid.to_string()).output().await;
                }
                if let Some(socat_pid) = metadata.get("socat_pid").and_then(|v| v.as_u64()) {
                    let _ = Command::new("kill").arg(socat_pid.to_string()).output().await;
                }
                killed_any = true;
            }
        }

        if !killed_any {
            let escaped_id = escape_regex(service_id);
            // Kill cloud-hypervisor
            let _ = Command::new("pkill")
                .args(["-f", &format!("cloud-hypervisor.*tap=vm-{}(,|$)", escaped_id)])
                .output()
                .await;

            // Kill virtiofsd
            let _ = Command::new("pkill")
                .args(["-f", &format!("virtiofsd.*russel/{}/", escaped_id)])
                .output()
                .await;

            // Kill socat
            let _ = Command::new("pkill")
                .args(["-f", &format!("socat.*TCP:{}:", alloc.vm_ip)])
                .output()
                .await;
        }

        Ok(())
    }

    /// Destroy all state for a microVM.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        let alloc = crate::network::subnet_for(service_id);

        if let Err(e) = self.stop(service_id).await {
            tracing::warn!(service_id = %service_id, error = %e, "stop during destroy failed");
        }

        // Teardown the TAP device
        if let Err(e) = crate::network::TapForwarder::teardown(&alloc).await {
            tracing::warn!(service_id = %service_id, error = %e, "tap teardown during destroy failed");
        }

        // Release port and subnet
        crate::network::PortAllocator::release(service_id);
        crate::network::release_subnet(service_id);

        for dir in &[
            format!("/var/lib/microvms/{}", service_id),
            format!("/var/lib/russel/{}", service_id),
        ] {
            if let Err(e) = Command::new("rm")
                .args(["-rf", dir])
                .output()
                .await
            {
                tracing::warn!(dir = %dir, error = %e, "failed to rm dir during destroy");
            }
        }
        for file in &[
            format!("/nix/var/nix/gcroots/microvm/{}", service_id),
            format!("/nix/var/nix/gcroots/microvm/booted-{}", service_id),
        ] {
            if let Err(e) = Command::new("rm")
                .args(["-f", file])
                .output()
                .await
            {
                tracing::warn!(file = %file, error = %e, "failed to rm gcroot during destroy");
            }
        }
        Ok(())
    }

    /// List registered microVMs (from /var/lib/microvms).
    pub async fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut vms = Vec::new();
        let state_dir = Path::new("/var/lib/microvms");
        if state_dir.exists() {
            if let Ok(mut entries) = tokio::fs::read_dir(state_dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if entry.file_type().await?.is_dir() {
                        if let Some(name) = entry.file_name().to_str() {
                            vms.push(name.to_string());
                        }
                    }
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
