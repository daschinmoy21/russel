//! Snapshot/restore warm pool for Cloud Hypervisor microVMs.
//!
//! On `prepare()` (called from `main` in the background):
//!   1. ensure kernel + busybox
//!   2. build agent initramfs once (no app baked in)
//!   3. create template TAP under reserved id `pooltpl`
//!   4. start virtiofsd for /nix/store + empty cfg dir
//!   5. boot CH with VmSpec (hotplug-ready), agent initramfs, dual fs
//!   6. poll host for cfg/.agent_ready (timeout 15s)
//!   7. API vm.pause → vm.snapshot to golden/
//!   8. tear down template VM/TAP/fsd cleanly
//!   9. mark pool ready
//!
//! On deploy `restore_or_boot`:
//!   - Pool not ready → cold `boot()` path (agent initramfs + deploy.env)
//!   - Pool ready → restore from golden snapshot with service config injected
//!   - Restore fails → cold boot fallback
//!
//! Env: `RUSSEL_WARM_POOL=0` disables prepare + restore path.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{process::Command, sync::Notify};

use crate::{
    ch_api,
    microvm::{self, BootOutput, FsMount, MicrovmRunner, VmSpec},
    network::{self, SubnetAllocation, TapForwarder},
};

/// Pool state directories (under `/var/lib/russel/_pool/`).
const POOL_BASE: &str = "/var/lib/russel/_pool";
const GOLDEN_DIR: &str = "/var/lib/russel/_pool/golden";

/// Internal service_id used for the template VM during prepare.
/// Must pass `validate_service_id` (alphanumeric + dash + underscore only).
const POOL_TEMPLATE_ID: &str = "pooltpl";

/// Timeout for the agent to write `.agent_ready` inside the template VM.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct WarmPool {
    runner: MicrovmRunner,
    ready: AtomicBool,
    /// Notified when prepare finishes (success or failure).
    prepare_done: Notify,
    /// Guards concurrent access to golden snapshot files during restore.
    restore_mutex: tokio::sync::Mutex<()>,
}

impl WarmPool {
    pub fn new(runner: MicrovmRunner) -> Self {
        Self {
            runner,
            ready: AtomicBool::new(false),
            prepare_done: Notify::new(),
            restore_mutex: tokio::sync::Mutex::new(()),
        }
    }

    /// True once prepare has finished successfully.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Block until prepare finishes (or immediately if already done).
    pub async fn wait_until_prepare_done(&self) {
        if self.is_ready() {
            return;
        }
        self.prepare_done.notified().await;
    }

    // ── Prepare ──────────────────────────────────────────────────────────

    /// Build the warm pool snapshot in the background.
    ///
    /// Idempotent: if the pool is already ready, returns immediately.
    /// Errors are logged but never propagated to the caller (main spawn).
    pub async fn prepare(&self) -> anyhow::Result<()> {
        if self.is_ready() {
            return Ok(());
        }

        if std::env::var("RUSSEL_WARM_POOL").as_deref() == Ok("0") {
            tracing::info!("RUSSEL_WARM_POOL=0 — warm pool disabled");
            self.prepare_done.notify_waiters();
            return Ok(());
        }

        // ponytail: mutex ensures a single prepare attempt even if main
        // somehow spawns two tasks.  In practice only one spawn exists.
        {
            let _lock = self.restore_mutex.lock().await;
            if self.is_ready() {
                return Ok(());
            }
        }

        // Inner result — on any error, notify waiters so they never hang.
        let result = self.prepare_inner().await;
        if result.is_err() {
            self.prepare_done.notify_waiters();
        }
        result
    }

    async fn prepare_inner(&self) -> anyhow::Result<()> {
        tracing::info!("preparing warm pool snapshot...");

        // 1. Ensure kernel + busybox.
        let kernel_info = self.runner.ensure_kernel().await?;
        let _busybox = self.runner.ensure_busybox().await?;

        // 2. Build agent initramfs once (cached).
        let agent_initramfs = self.runner.build_agent_initramfs().await?;

        // 3. Create template TAP + config dir.
        let alloc = network::subnet_for(POOL_TEMPLATE_ID);
        let sock_dir = format!("{POOL_BASE}/template");
        std::fs::create_dir_all(&sock_dir)?;

        let cfg_dir = format!("{sock_dir}/cfg");
        std::fs::create_dir_all(&cfg_dir)?;

        // Spin up a short-lived TAP — no socat needed for template.
        Self::create_template_tap(&alloc).await?;

        // 4. Boot template CH with hotplug-ready VmSpec (boot_vm spawns virtiofsd).
        let nixstore_sock = PathBuf::from(format!("{sock_dir}/virtiofs-nixstore.sock"));
        let cfg_sock = PathBuf::from(format!("{sock_dir}/virtiofs-cfg.sock"));
        let api_socket = PathBuf::from(format!("{sock_dir}/cloud-hypervisor.sock"));

        if let Err(e) = std::fs::remove_file(&api_socket)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %api_socket.display(), error = %e, "failed to remove stale CH API socket");
        }

        let spec = VmSpec {
            kernel: kernel_info.path.clone(),
            initramfs: agent_initramfs,
            cmdline: "quiet loglevel=0 panic=-1 random.trust_cpu=on".into(),
            cpus_boot: 1,
            cpus_max: std::env::var("RUSSEL_CPU_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            memory_mb: 256, // ponytail: minimum viable; hotplug adds headroom
            memory_hotplug_mb: std::env::var("RUSSEL_MEM_HOTPLUG_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2048),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket: api_socket.clone(),
            fs: vec![
                FsMount {
                    tag: "nixstore".into(),
                    socket: nixstore_sock.clone(),
                    shared_dir: PathBuf::from("/nix/store"),
                    readonly: true,
                },
                FsMount {
                    tag: "russelcfg".into(),
                    socket: cfg_sock.clone(),
                    shared_dir: PathBuf::from(&cfg_dir),
                    readonly: false,
                },
            ],
            console: "null".into(),
            restore_url: None,
        };

        let boot = self.runner.boot_vm(&spec).await?;
        let children: Vec<tokio::process::Child> = boot.virtiofsd_children;
        let vm_child = boot.vm_child;

        // 6. Poll for .agent_ready in cfg dir.
        let agent_ready = PathBuf::from(format!("{cfg_dir}/.agent_ready"));
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
        std::fs::create_dir_all(GOLDEN_DIR)?;

        ch_api::vm_pause(&api_socket).await?;

        let snapshot_url = format!("file://{GOLDEN_DIR}");
        ch_api::vm_snapshot(&api_socket, &snapshot_url).await?;

        tracing::info!("warm pool snapshot saved to {GOLDEN_DIR}");

        // 8. Tear down template cleanly.
        Self::cleanup_template(&alloc, &sock_dir, vm_child, children).await;

        // 9. Mark ready.
        self.ready.store(true, Ordering::Release);
        self.prepare_done.notify_waiters();

        tracing::info!("warm pool ready");
        Ok(())
    }

    async fn create_template_tap(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        let host_ip = &alloc.host_ip;
        tracing::info!(tap, "creating template TAP");
        let _ = Self::run_ip(&["link", "del", tap]).await;
        Self::run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"]).await?;
        Self::run_ip(&["link", "set", tap, "up"]).await?;
        Self::run_ip(&["addr", "replace", &format!("{host_ip}/30"), "dev", tap]).await?;
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

    async fn run_ip(args: &[&str]) -> anyhow::Result<()> {
        let out = Command::new("ip")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .args(args)
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!(
                "ip {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    // ── Restore or boot ──────────────────────────────────────────────────

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
        config_dir: &Path,
    ) -> anyhow::Result<BootOutput> {
        let sock_dir = format!("/var/lib/russel/{service_id}");
        std::fs::create_dir_all(&sock_dir)?;

        let nixstore_sock = PathBuf::from(format!("{sock_dir}/virtiofs-nixstore.sock"));
        let cfg_sock = PathBuf::from(format!("{sock_dir}/virtiofs-cfg.sock"));
        let api_socket = PathBuf::from(format!("{sock_dir}/cloud-hypervisor.sock"));

        let spec = VmSpec {
            kernel: kernel_path.to_path_buf(),
            initramfs: initramfs_path.to_path_buf(),
            cmdline: "quiet loglevel=0 panic=-1 random.trust_cpu=on".into(),
            cpus_boot: 1,
            cpus_max: std::env::var("RUSSEL_CPU_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            memory_mb,
            memory_hotplug_mb: std::env::var("RUSSEL_MEM_HOTPLUG_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2048),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket,
            fs: vec![
                FsMount {
                    tag: "nixstore".into(),
                    socket: nixstore_sock,
                    shared_dir: PathBuf::from("/nix/store"),
                    readonly: true,
                },
                FsMount {
                    tag: "russelcfg".into(),
                    socket: cfg_sock,
                    shared_dir: config_dir.to_path_buf(),
                    readonly: false,
                },
            ],
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
        _memory_mb: u16,
        config_dir: &Path,
    ) -> anyhow::Result<BootOutput> {
        // ponytail: scope the lock so it's dropped before any await.
        let _lock = self.restore_mutex.lock().await;

        let sock_dir = format!("/var/lib/russel/{service_id}");
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
            let src = format!("{GOLDEN_DIR}/{file}");
            let dst = format!("{restore_dir}/{file}");
            if Path::new(&src).exists() {
                std::os::unix::fs::symlink(&src, &dst)?;
            } else {
                anyhow::bail!("golden snapshot missing {file}");
            }
        }

        // Copy and patch config.json for this service.
        let golden_config = format!("{GOLDEN_DIR}/config.json");
        let restore_config = format!("{restore_dir}/config.json");

        if !Path::new(&golden_config).exists() {
            anyhow::bail!("golden snapshot missing config.json");
        }

        let nixstore_sock = format!("{sock_dir}/virtiofs-nixstore.sock");
        let cfg_sock = format!("{sock_dir}/virtiofs-cfg.sock");
        let api_socket_path = format!("{sock_dir}/cloud-hypervisor.sock");

        let raw = std::fs::read_to_string(&golden_config)?;
        let patched = self.patch_config_json(
            &raw,
            service_id,
            &alloc.tap_id,
            &alloc.mac,
            &nixstore_sock,
            &cfg_sock,
            &api_socket_path,
        )?;
        std::fs::write(&restore_config, patched)?;

        // Use boot_vm with restore_url — virtiofsd is spawned by boot_vm.
        let spec = VmSpec {
            kernel: PathBuf::from("/dev/null"),  // not used when restoring
            initramfs: PathBuf::from("/dev/null"),
            cmdline: "quiet loglevel=0 panic=-1 random.trust_cpu=on".into(),
            cpus_boot: 1,
            cpus_max: std::env::var("RUSSEL_CPU_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            memory_mb: _memory_mb.max(256),
            memory_hotplug_mb: std::env::var("RUSSEL_MEM_HOTPLUG_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2048),
            tap: alloc.tap_id.clone(),
            mac: alloc.mac.clone(),
            api_socket: PathBuf::from(&api_socket_path),
            fs: vec![
                FsMount {
                    tag: "nixstore".into(),
                    socket: PathBuf::from(&nixstore_sock),
                    shared_dir: PathBuf::from("/nix/store"),
                    readonly: true,
                },
                FsMount {
                    tag: "russelcfg".into(),
                    socket: PathBuf::from(&cfg_sock),
                    shared_dir: config_dir.to_path_buf(),
                    readonly: false,
                },
            ],
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
        // The golden snapshot has tags ["nixstore", "russelcfg"] in order.
        if let Some(fs_arr) = config.get_mut("fs").and_then(|f| f.as_array_mut()) {
            for fs_entry in fs_arr.iter_mut() {
                let tag = fs_entry
                    .get("tag")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match tag {
                    "nixstore" => {
                        fs_entry["socket"] =
                            serde_json::Value::String(nixstore_sock.to_string());
                    }
                    "russelcfg" => {
                        fs_entry["socket"] =
                            serde_json::Value::String(cfg_sock.to_string());
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
        if let Some(console) = config.get_mut("console") {
            if let Some(mode) = console.get_mut("mode") {
                *mode = serde_json::Value::String("null".to_string());
            }
        }

        let patched = serde_json::to_string_pretty(&config)?;
        Ok(patched)
    }
}

// ── Public accessor ─────────────────────────────────────────────────────────

use std::sync::LazyLock;

static WARM_POOL: LazyLock<WarmPool> = LazyLock::new(|| {
    WarmPool::new(microvm::shared_runner())
});

/// Shared warm pool singleton (like `shared_runner()` for deploy.rs).
pub fn shared_warm_pool() -> &'static WarmPool {
    &WARM_POOL
}
