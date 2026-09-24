//! Snapshot/restore warm pool for Cloud Hypervisor microVMs.
//!
//! On `prepare()` (called from `main` in the background):
//!   1. ensure kernel + busybox
//!   2. build agent initramfs once (no app baked in)
//!   3. create template TAP under reserved id `pooltpl`
//!   4. start virtiofsd for /nix/store + RO cfg + RW scratch
//!   5. boot CH with VmSpec (hotplug-ready), agent initramfs, three fs shares
//!   6. poll host for scratch/.agent_ready (timeout 15s)
//!   7. API vm.pause → vm.snapshot to golden/
//!   8. tear down template VM/TAP/fsd cleanly
//!   9. mark pool ready
//!
//! On deploy `restore_or_boot`:
//!   - Pool not ready → cold `boot()` path (agent initramfs + deploy.env)
//!   - Pool ready → restore from golden snapshot with service config injected
//!   - Restore fails → cold boot fallback
//!
//! Env: `RUSSEL_WARM_POOL=1` enables prepare + restore (experimental).
//! Default is **off**: snapshot-after-virtiofs leaves stale guest FUSE state on
//! restore, which presents as "VM not reachable in 10s".

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use crate::{
    ch_api,
    microvm::{self, BootOutput, MicrovmRunner, VmSpec, ensure_private_dir, service_fs_mounts},
    network::{self, SubnetAllocation, TapForwarder},
};

/// Pool state directories (under `/var/lib/russel/_pool/`).
fn pool_base() -> std::path::PathBuf {
    russel_core::paths::data_root().join("_pool")
}

fn golden_dir() -> std::path::PathBuf {
    pool_base().join("golden")
}

/// Internal service_id used for the template VM during prepare.
/// Must pass `validate_service_id` (alphanumeric + dash + underscore only).
const POOL_TEMPLATE_ID: &str = "pooltpl";

/// Timeout for the agent to write `.agent_ready` inside the template VM.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// `RUSSEL_CPU_MAX` (default 8) — upper bound for CPU hotplug topology.
fn env_cpu_max() -> u8 {
    std::env::var("RUSSEL_CPU_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

/// `RUSSEL_MEM_HOTPLUG_MB` (default 2048) — memory hotplug headroom.
fn env_mem_hotplug_mb() -> u16 {
    std::env::var("RUSSEL_MEM_HOTPLUG_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048)
}

/// Drop a leftover `scratch/.agent_ready` from a previous template boot.
///
/// Missing file is fine. Any other error is returned so prepare does not
/// treat a stale marker as a live agent.
fn clear_agent_ready(scratch_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(scratch_dir.join(".agent_ready")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
pub struct WarmPool {
    runner: MicrovmRunner,
    ready: AtomicBool,
    /// Guards concurrent access to golden snapshot files during restore.
    restore_mutex: tokio::sync::Mutex<()>,
}

impl WarmPool {
    pub fn new(runner: MicrovmRunner) -> Self {
        Self {
            runner,
            ready: AtomicBool::new(false),
            restore_mutex: tokio::sync::Mutex::new(()),
        }
    }

    /// True once prepare has finished successfully.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Build the warm pool snapshot in the background.
    ///
    /// Idempotent: if the pool is already ready, returns immediately.
    /// Errors are logged but never propagated to the caller (main spawn).
    pub async fn prepare(&self) -> anyhow::Result<()> {
        if self.is_ready() {
            return Ok(());
        }

        // Opt-in only: restore after virtiofs mount is not safe yet.
        if std::env::var("RUSSEL_WARM_POOL").as_deref() != Ok("1") {
            tracing::info!(
                "warm pool disabled (set RUSSEL_WARM_POOL=1 for experimental snapshot restore)"
            );
            return Ok(());
        }

        // Hold the restore mutex across the entire prepare so a concurrent
        // restore cannot race the golden snapshot being written, and a second
        // prepare task cannot run prepare_inner concurrently.
        let _lock = self.restore_mutex.lock().await;
        if self.is_ready() {
            return Ok(());
        }

        self.prepare_inner().await
    }

    async fn prepare_inner(&self) -> anyhow::Result<()> {
        tracing::info!("preparing warm pool snapshot...");

        // 1. Ensure kernel + busybox.
        let kernel_info = self.runner.ensure_kernel().await?;
        let _busybox = self.runner.ensure_busybox().await?;

        // 2. Build agent initramfs once (cached).
        let agent_initramfs = self.runner.build_agent_initramfs().await?;

        // 3. Create template TAP + config dir.
        let alloc = network::subnet_for(POOL_TEMPLATE_ID)?;
        let sock_dir = pool_base().join("template").display().to_string();
        std::fs::create_dir_all(&sock_dir)?;

        let cfg_dir = format!("{sock_dir}/cfg");
        let scratch_dir = format!("{sock_dir}/scratch");
        ensure_private_dir(Path::new(&cfg_dir))?;
        ensure_private_dir(Path::new(&scratch_dir))?;
        clear_agent_ready(Path::new(&scratch_dir))?;

        // Spin up a short-lived TAP — no socat needed for template.
        Self::create_template_tap(&alloc).await?;

        // 4. Boot template CH with hotplug-ready VmSpec (boot_vm spawns virtiofsd).
        let api_socket = PathBuf::from(format!("{sock_dir}/cloud-hypervisor.sock"));

        if let Err(e) = std::fs::remove_file(&api_socket)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %api_socket.display(), error = %e, "failed to remove stale CH API socket");
        }

        let spec = VmSpec {
            kernel: kernel_info.path.clone(),
            initramfs: agent_initramfs,
            cmdline: "console=ttyS0 panic=-1 random.trust_cpu=on net.ifnames=0".into(),
            cpus_boot: 1,
            cpus_max: env_cpu_max(),
            memory_mb: 256, // minimum viable; hotplug adds headroom
            memory_hotplug_mb: env_mem_hotplug_mb(),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket: api_socket.clone(),
            fs: service_fs_mounts(
                Path::new(&sock_dir),
                Path::new(&cfg_dir),
                Path::new(&scratch_dir),
            ),
            console: "null".into(),
            restore_url: None,
        };

        let boot = self.runner.boot_vm(&spec).await?;
        let children: Vec<tokio::process::Child> = boot.virtiofsd_children;
        let vm_child = boot.vm_child;

        // 6. Poll for .agent_ready in scratch dir (cfg is read-only).
        let agent_ready = PathBuf::from(format!("{scratch_dir}/.agent_ready"));
        let deadline = tokio::time::Instant::now() + AGENT_READY_TIMEOUT;
        let mut agent_seen = false;
        while tokio::time::Instant::now() < deadline {
            if agent_ready.exists() {
                agent_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        if !agent_seen {
            tracing::error!(
                "template VM agent did not write .agent_ready within {:?}",
                AGENT_READY_TIMEOUT
            );
            Self::cleanup_template(&alloc, &sock_dir, vm_child, children).await;
            anyhow::bail!("warm pool prepare failed: agent not ready");
        }

        tracing::info!("template VM agent ready — pausing for snapshot");

        // 7. Pause + snapshot.
        std::fs::create_dir_all(golden_dir())?;

        ch_api::vm_pause(&api_socket).await?;

        let snapshot_url = format!("file://{}", golden_dir().display());
        ch_api::vm_snapshot(&api_socket, &snapshot_url).await?;

        tracing::info!("warm pool snapshot saved to {}", golden_dir().display());

        // 8. Tear down template cleanly.
        Self::cleanup_template(&alloc, &sock_dir, vm_child, children).await;

        // 9. Mark ready.
        self.ready.store(true, Ordering::Release);

        tracing::info!("warm pool ready");
        Ok(())
    }

    async fn create_template_tap(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        let host_ip = &alloc.host_ip;
        tracing::info!(tap, "creating template TAP");
        let _ = network::run_ip(&["link", "del", tap]).await;
        network::run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"]).await?;
        network::run_ip(&["link", "set", tap, "up"]).await?;
        network::run_ip(&["addr", "replace", &format!("{host_ip}/30"), "dev", tap]).await?;
        Ok(())
    }

    async fn cleanup_template(
        alloc: &SubnetAllocation,
        sock_dir: &str,
        mut vm_child: tokio::process::Child,
        children: Vec<tokio::process::Child>,
    ) {
        // Try API shutdown first, then kill.
        let api_socket = format!("{sock_dir}/cloud-hypervisor.sock");
        let api_path = Path::new(&api_socket);
        if api_path.exists() {
            let _ = ch_api::vm_shutdown(api_path).await;
            let _ = ch_api::vmm_shutdown(api_path).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = vm_child.kill().await;
        let _ = vm_child.wait().await;
        for mut child in children {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let _ = TapForwarder::teardown(alloc).await;
        let _ = tokio::fs::remove_dir_all(sock_dir).await;
    }

    /// Acquire a VM: restore from warm pool snapshot if ready, else cold boot.
    ///
    /// Returns `BootOutput` with VM + virtiofsd children.
    /// The `config_dir` must already contain `deploy.env` before calling.
    pub async fn restore_or_boot(
        &self,
        service_id: &str,
        kernel_path: &Path,
        initramfs_path: &Path,
        alloc: &SubnetAllocation,
        memory_mb: u16,
        cpus_boot: u8,
        config_dir: &Path,
    ) -> anyhow::Result<BootOutput> {
        if !self.is_ready() {
            tracing::info!(service_id, "warm pool not ready — cold booting");
            return self
                .cold_boot(
                    service_id,
                    kernel_path,
                    initramfs_path,
                    alloc,
                    memory_mb,
                    cpus_boot,
                    config_dir,
                )
                .await;
        }

        if cpus_boot != 1 {
            tracing::info!(
                service_id,
                cpus_boot,
                "cpus mismatch with template (template=1) — cold booting"
            );
            return self
                .cold_boot(
                    service_id,
                    kernel_path,
                    initramfs_path,
                    alloc,
                    memory_mb,
                    cpus_boot,
                    config_dir,
                )
                .await;
        }

        match self
            .restore_from_pool(service_id, alloc, memory_mb, config_dir)
            .await
        {
            Ok(output) => {
                tracing::info!(service_id, "restored from warm pool");
                Ok(output)
            }
            Err(e) => {
                tracing::warn!(service_id, error = %e, "warm pool restore failed — falling back to cold boot");
                self.cold_boot(
                    service_id,
                    kernel_path,
                    initramfs_path,
                    alloc,
                    memory_mb,
                    cpus_boot,
                    config_dir,
                )
                .await
            }
        }
    }

    /// Cold boot using agent initramfs + deploy.env (deploy.env must already exist).
    pub(crate) async fn cold_boot(
        &self,
        service_id: &str,
        kernel_path: &Path,
        initramfs_path: &Path,
        alloc: &SubnetAllocation,
        memory_mb: u16,
        cpus_boot: u8,
        config_dir: &Path,
    ) -> anyhow::Result<BootOutput> {
        let sock_dir = russel_core::paths::service_dir(service_id)
            .display()
            .to_string();
        std::fs::create_dir_all(&sock_dir)?;
        let scratch_dir = PathBuf::from(format!("{sock_dir}/scratch"));
        ensure_private_dir(&scratch_dir)?;

        let api_socket = PathBuf::from(format!("{sock_dir}/cloud-hypervisor.sock"));

        let cpus_max: u8 = env_cpu_max().max(cpus_boot);

        let spec = VmSpec {
            kernel: kernel_path.to_path_buf(),
            initramfs: initramfs_path.to_path_buf(),
            cmdline: "console=ttyS0 panic=-1 random.trust_cpu=on net.ifnames=0".into(),
            cpus_boot,
            cpus_max,
            memory_mb,
            memory_hotplug_mb: env_mem_hotplug_mb(),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket,
            fs: service_fs_mounts(Path::new(&sock_dir), config_dir, &scratch_dir),
            console: "null".into(),
            restore_url: None,
        };

        self.runner.boot_vm(&spec).await
    }

    /// Restore a VM from the golden snapshot.
    async fn restore_from_pool(
        &self,
        service_id: &str,
        alloc: &SubnetAllocation,
        memory_mb: u16,
        config_dir: &Path,
    ) -> anyhow::Result<BootOutput> {
        // Hold the restore mutex for the whole restore so a concurrent prepare
        // (or another restore) cannot race the golden snapshot being read.
        let _lock = self.restore_mutex.lock().await;

        let sock_dir = russel_core::paths::service_dir(service_id)
            .display()
            .to_string();
        std::fs::create_dir_all(&sock_dir)?;

        let restore_dir = format!("{sock_dir}/restore");
        if let Err(e) = std::fs::remove_dir_all(&restore_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %restore_dir, error = %e, "failed to clean restore dir");
        }
        std::fs::create_dir_all(&restore_dir)?;

        // Symlink memory-ranges and state.json (read-only, shareable).
        for file in &["memory-ranges", "state.json"] {
            let src = golden_dir().join(file);
            let dst = format!("{restore_dir}/{file}");
            if Path::new(&src).exists() {
                std::os::unix::fs::symlink(&src, &dst)?;
            } else {
                anyhow::bail!("golden snapshot missing {file}");
            }
        }

        // Copy and patch config.json for this service.
        let golden_config = golden_dir().join("config.json");
        let restore_config = format!("{restore_dir}/config.json");

        if !Path::new(&golden_config).exists() {
            anyhow::bail!("golden snapshot missing config.json");
        }

        let nixstore_sock = format!("{sock_dir}/virtiofs-nixstore.sock");
        let cfg_sock = format!("{sock_dir}/virtiofs-cfg.sock");
        let scratch_sock = format!("{sock_dir}/virtiofs-scratch.sock");
        let api_socket_path = format!("{sock_dir}/cloud-hypervisor.sock");
        let scratch_dir = PathBuf::from(format!("{sock_dir}/scratch"));
        ensure_private_dir(&scratch_dir)?;

        let raw = std::fs::read_to_string(&golden_config)?;
        let patched = self.patch_config_json(
            &raw,
            service_id,
            &alloc.tap_id,
            &alloc.mac,
            &nixstore_sock,
            &cfg_sock,
            &scratch_sock,
            &api_socket_path,
        )?;
        std::fs::write(&restore_config, patched)?;

        // Use boot_vm with restore_url — virtiofsd is spawned by boot_vm.
        let spec = VmSpec {
            kernel: PathBuf::from("/dev/null"), // not used when restoring
            initramfs: PathBuf::from("/dev/null"),
            cmdline: "console=ttyS0 panic=-1 random.trust_cpu=on net.ifnames=0".into(),
            cpus_boot: 1,
            cpus_max: env_cpu_max(),
            memory_mb: memory_mb.max(256),
            memory_hotplug_mb: env_mem_hotplug_mb(),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket: PathBuf::from(&api_socket_path),
            fs: service_fs_mounts(Path::new(&sock_dir), config_dir, &scratch_dir),
            console: "null".into(),
            restore_url: Some(format!("file://{restore_dir}")),
        };

        self.runner.boot_vm(&spec).await
    }

    /// Patch the golden config.json for a specific service deployment.
    ///
    /// Cloud Hypervisor allows editing `config.json` between snapshot and
    /// restore.  We replace the API socket, TAP name, MAC, and virtiofs
    /// socket paths so the restored VM uses the new service's resources.
    fn patch_config_json(
        &self,
        raw: &str,
        _service_id: &str,
        tap: &str,
        mac: &str,
        nixstore_sock: &str,
        cfg_sock: &str,
        scratch_sock: &str,
        api_socket: &str,
    ) -> anyhow::Result<String> {
        let mut config: serde_json::Value = serde_json::from_str(raw)?;

        // Patch TAP device in `net` array.
        if let Some(net_arr) = config.get_mut("net").and_then(|n| n.as_array_mut()) {
            for net in net_arr.iter_mut() {
                if let Some(tap_field) = net.get_mut("tap") {
                    *tap_field = serde_json::Value::String(tap.to_string());
                }
                if let Some(mac_field) = net.get_mut("mac") {
                    *mac_field = serde_json::Value::String(mac.to_string());
                }
            }
        }

        // Patch virtiofs socket paths in `fs` array.
        // Golden snapshots have tags nixstore, russelcfg, russelscratch.
        if let Some(fs_arr) = config.get_mut("fs").and_then(|f| f.as_array_mut()) {
            for fs_entry in fs_arr.iter_mut() {
                let tag = fs_entry.get("tag").and_then(|t| t.as_str()).unwrap_or("");
                match tag {
                    "nixstore" => {
                        fs_entry["socket"] = serde_json::Value::String(nixstore_sock.to_string());
                    }
                    "russelcfg" => {
                        fs_entry["socket"] = serde_json::Value::String(cfg_sock.to_string());
                    }
                    "russelscratch" => {
                        fs_entry["socket"] = serde_json::Value::String(scratch_sock.to_string());
                    }
                    _ => {}
                }
            }
        }

        // Patch API socket path if present.
        if let Some(api) = config.get_mut("api_socket") {
            *api = serde_json::Value::String(api_socket.to_string());
        }

        // Ensure the console is null.
        if let Some(console) = config.get_mut("console")
            && let Some(mode) = console.get_mut("mode")
        {
            *mode = serde_json::Value::String("null".to_string());
        }

        let patched = serde_json::to_string_pretty(&config)?;
        Ok(patched)
    }
}

use std::sync::LazyLock;

static WARM_POOL: LazyLock<WarmPool> = LazyLock::new(|| WarmPool::new(microvm::shared_runner()));

/// Shared warm pool singleton (like `shared_runner()` for deploy.rs).
pub fn shared_warm_pool() -> &'static WarmPool {
    &WARM_POOL
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn pool() -> WarmPool {
        WarmPool::new(MicrovmRunner::new())
    }

    /// Representative Cloud Hypervisor `config.json` (golden snapshot).
    const GOLDEN_CONFIG: &str = r#"{
        "cpus": {"boot_vcpus": 1, "max_vcpus": 8},
        "memory": {"size": 268435456},
        "net": [
            {"id": "net0", "tap": "pooltpl-tap0", "mac": "aa:bb:cc:dd:ee:ff"}
        ],
        "fs": [
            {"tag": "nixstore", "socket": "/var/lib/russel/_pool/template/virtiofs-nixstore.sock"},
            {"tag": "russelcfg", "socket": "/var/lib/russel/_pool/template/virtiofs-cfg.sock"},
            {"tag": "russelscratch", "socket": "/var/lib/russel/_pool/template/virtiofs-scratch.sock"},
            {"tag": "other", "socket": "/keep/me.sock"}
        ],
        "api_socket": "/var/lib/russel/_pool/template/cloud-hypervisor.sock",
        "console": {"mode": "tty"}
    }"#;

    #[test]
    fn patch_config_json_rewrites_service_resources() {
        let patched = pool()
            .patch_config_json(
                GOLDEN_CONFIG,
                "svc-1",
                "svc-1-tap0",
                "02:00:00:00:00:01",
                "/var/lib/russel/svc-1/virtiofs-nixstore.sock",
                "/var/lib/russel/svc-1/virtiofs-cfg.sock",
                "/var/lib/russel/svc-1/virtiofs-scratch.sock",
                "/var/lib/russel/svc-1/cloud-hypervisor.sock",
            )
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&patched).unwrap();

        assert_eq!(config["net"][0]["tap"], "svc-1-tap0");
        assert_eq!(config["net"][0]["mac"], "02:00:00:00:00:01");
        assert_eq!(
            config["fs"][0]["socket"],
            "/var/lib/russel/svc-1/virtiofs-nixstore.sock"
        );
        assert_eq!(
            config["fs"][1]["socket"],
            "/var/lib/russel/svc-1/virtiofs-cfg.sock"
        );
        assert_eq!(
            config["fs"][2]["socket"],
            "/var/lib/russel/svc-1/virtiofs-scratch.sock"
        );
        // Unrelated fs tags are left untouched.
        assert_eq!(config["fs"][3]["socket"], "/keep/me.sock");
        assert_eq!(
            config["api_socket"],
            "/var/lib/russel/svc-1/cloud-hypervisor.sock"
        );
        assert_eq!(config["console"]["mode"], "null");
    }

    #[test]
    fn patch_config_json_preserves_cpu_and_memory() {
        // CPU/memory are configured on VmSpec (cpus_boot / memory_mb), not in
        // the snapshot config.json; the patch must not corrupt either field.
        let patched = pool()
            .patch_config_json(
                GOLDEN_CONFIG,
                "svc-1",
                "tap",
                "02:00:00:00:00:01",
                "/nix.sock",
                "/cfg.sock",
                "/scratch.sock",
                "/ch.sock",
            )
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&patched).unwrap();
        assert_eq!(config["cpus"]["boot_vcpus"], 1);
        assert_eq!(config["cpus"]["max_vcpus"], 8);
        assert_eq!(config["memory"]["size"], 268435456);
    }

    #[test]
    fn patch_config_json_is_idempotent() {
        let once = pool()
            .patch_config_json(
                GOLDEN_CONFIG,
                "svc-1",
                "svc-1-tap0",
                "02:00:00:00:00:01",
                "/nix.sock",
                "/cfg.sock",
                "/scratch.sock",
                "/ch.sock",
            )
            .unwrap();
        let twice = pool()
            .patch_config_json(
                &once,
                "svc-1",
                "svc-1-tap0",
                "02:00:00:00:00:01",
                "/nix.sock",
                "/cfg.sock",
                "/scratch.sock",
                "/ch.sock",
            )
            .unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn patch_config_json_tolerates_missing_fields() {
        let minimal = r#"{"net": [{"tap": "a", "mac": "b"}]}"#;
        let patched = pool()
            .patch_config_json(minimal, "svc-1", "tap", "mac", "/n", "/c", "/s", "/ch")
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&patched).unwrap();
        assert_eq!(config["net"][0]["tap"], "tap");
        assert_eq!(config["net"][0]["mac"], "mac");
        // No fs / api_socket / console keys present — nothing to patch, no panic.
        assert!(config.get("fs").is_none());
        assert!(config.get("api_socket").is_none());
    }

    #[test]
    fn patch_config_json_rejects_invalid_json() {
        assert!(
            pool()
                .patch_config_json("not json", "svc-1", "t", "m", "/n", "/c", "/s", "/ch")
                .is_err()
        );
    }

    #[test]
    fn golden_dir_layout_is_stable() {
        // The pool + golden snapshot live under a reserved `_pool` service dir;
        // a template id must pass `validate_service_id`. Assert the default
        // layout without reading live RUSSEL_DATA_DIR.
        let root = russel_core::paths::data_root_from(None);
        let expected = PathBuf::from(russel_core::paths::DEFAULT_DATA_ROOT);
        assert_eq!(root.join("_pool"), expected.join("_pool"));
        assert_eq!(
            root.join("_pool").join("golden"),
            expected.join("_pool").join("golden")
        );
        assert_eq!(POOL_TEMPLATE_ID, "pooltpl");
    }

    #[test]
    fn clear_agent_ready_ok_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        clear_agent_ready(dir.path()).unwrap();
        assert!(!dir.path().join(".agent_ready").exists());
    }

    #[test]
    fn clear_agent_ready_deletes_stale_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".agent_ready");
        std::fs::write(&marker, "ready").unwrap();
        assert!(marker.exists());
        clear_agent_ready(dir.path()).unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn clear_agent_ready_propagates_non_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".agent_ready");
        std::fs::create_dir(&marker).unwrap();
        let err = clear_agent_ready(dir.path()).unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(marker.is_dir());
    }
}
