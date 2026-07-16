use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::process::Command;

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployResponse, DeployTiming, PortMapping},
    config::{Russelfile, resolve_runtime},
};

use crate::{
    build::NixBuilder,
    database::DatabaseProvisioner,
    git::GitClient,
    microvm::{self, BootOutput, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    traefik::TraefikClient,
    warm_pool::shared_warm_pool,
};

#[derive(Debug)]
pub struct DeployPipeline {
    state: AppState,
    git: GitClient,
    builder: NixBuilder,
    #[allow(dead_code)]
    database: DatabaseProvisioner,
    runner: MicrovmRunner,
    ports: PortAllocator,
    traefik: TraefikClient,
}

impl DeployPipeline {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            git: GitClient,
            builder: NixBuilder,
            database: DatabaseProvisioner,
            // DeployPipeline is constructed per request.  Keep the expensive
            // kernel/busybox/module resolution cache alive across requests so
            // the benchmark's later VMs measure VM work rather than repeated
            // `nix build --no-link` evaluations.
            runner: microvm::shared_runner(),
            ports: PortAllocator,
            traefik: TraefikClient,
        }
    }

    pub async fn deploy(
        &self,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> DeployResponse {
        let started = Instant::now();
        let service_id = request.vm_id.clone().unwrap_or_else(|| "api".to_string());
        let vm_id = service_id.clone();

        tracing::info!(service_id = %service_id, repo = %request.repo_url, "deploy started");
        if let Err(e) = self.state.mark_building(&service_id) {
            tracing::error!(service_id = %service_id, error = %e, "deploy rejected: service busy");
            return DeployResponse {
                service_id,
                vm_id,
                status: "failed".to_string(),
                store_path: None,
                microvm_config_path: None,
                runner_path: None,
                port: None,
                elapsed_ms: started.elapsed().as_millis(),
                timing: None,
                vm_ip: None,
                runtime: request.runtime,
                message: e.to_string(),
            };
        }

        let request_runtime = request.runtime;
        let result = self.deploy_inner(&service_id, request, tx).await;

        match result {
            Ok(output) => {
                let elapsed = started.elapsed().as_millis();
                let host_port = output.port.host;
                let guest_port = output.port.guest;
                tracing::info!(
                    service_id = %service_id,
                    elapsed_ms = elapsed,
                    host_port,
                    guest_port,
                    "deploy succeeded"
                );

                let mut aux = vec![output.socat_child];
                aux.extend(output.virtiofsd_children);
                self.state
                    .mark_deployed_with_aux(&service_id, output.vm_child, aux);

                DeployResponse {
                    service_id,
                    vm_id,
                    status: "deployed".to_string(),
                    store_path: Some(output.store_path.display().to_string()),
                    microvm_config_path: Some(output.initramfs_path.display().to_string()),
                    runner_path: None,
                    port: Some(output.port),
                    elapsed_ms: elapsed,
                    timing: Some(output.timing),
                    vm_ip: Some(output.alloc.vm_ip.clone()),
                    runtime: output.runtime,
                    message: format!(
                        "microVM running. localhost:{host_port} -> {}:{guest_port}",
                        output.alloc.vm_ip
                    ),
                }
            }
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
                let error_msg = error.to_string();

                // Check if this is a successful rollback case
                if error_msg.starts_with("ROLLBACK_SUCCESS:") {
                    tracing::info!(
                        service_id = %service_id,
                        elapsed_ms = elapsed,
                        "deploy failed but successfully rolled back to previous VM"
                    );
                    let original_error = error_msg.trim_start_matches("ROLLBACK_SUCCESS:").trim();
                    DeployResponse {
                        service_id,
                        vm_id,
                        status: "deployed".to_string(),
                        store_path: None,
                        microvm_config_path: None,
                        runner_path: None,
                        port: None,
                        elapsed_ms: elapsed,
                        timing: None,
                        vm_ip: None,
                        runtime: request_runtime,
                        message: format!(
                            "deployment failed but rolled back successfully: {}",
                            original_error
                        ),
                    }
                } else {
                    tracing::error!(
                        service_id = %service_id,
                        elapsed_ms = elapsed,
                        error = %error,
                        "deploy failed"
                    );
                    self.state.mark_failed(&service_id, error.to_string());
                    DeployResponse {
                        service_id,
                        vm_id,
                        status: "failed".to_string(),
                        store_path: None,
                        microvm_config_path: None,
                        runner_path: None,
                        port: None,
                        elapsed_ms: elapsed,
                        timing: None,
                        vm_ip: None,
                        runtime: request_runtime,
                        message: error.to_string(),
                    }
                }
            }
        }
    }

    async fn deploy_inner(
        &self,
        service_id: &str,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<DeployOutput> {
        // Validate service_id early, before any expensive operations
        MicrovmRunner::validate_service_id(service_id)?;

        // ── 1. Resolve repo ──────────────────────────────────────────────────
        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "resolve".into(),
                description: "Resolving source & Russelfile".into(),
            })
            .await;
        let repo_path = self.git.clone_or_use_local(&request.repo_url).await?;
        let config_path = repo_path.join(PathBuf::from(&request.config_path));
        let config = Russelfile::load(&config_path)?;
        resolve_runtime(config.service.runtime, request.runtime)?;
        let resolve_ms = t.elapsed().as_millis();
        tracing::info!(service_id, service_name = %config.service.name, resolve_ms, "repo resolved");

        // ── 2. Nix build app + ensure kernel + busybox ──────────────────────
        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "build".into(),
                description: "Building Nix package + ensuring kernel/busybox".into(),
            })
            .await;
        let build = self.builder.build(&repo_path).await?;
        let kernel_info = self.runner.ensure_kernel().await?;
        let _busybox = self.runner.ensure_busybox().await?;
        let _kernel_modules = self.runner.ensure_kernel_modules().await?;
        let build_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            store = %build.store_path.display(),
            kernel = %kernel_info.path.display(),
            builtin = kernel_info.drivers_builtin,
            build_ms,
            "build complete (kernel + busybox cached)"
        );

        // Now that the build has succeeded, we can prepare the backup and rollback path
        let russel_dir = format!("/var/lib/russel/{}", service_id);
        let microvms_dir = format!("/var/lib/microvms/{}", service_id);
        let russel_bak = format!("{}.bak", russel_dir);
        let microvms_bak = format!("{}.bak", microvms_dir);
        let has_backup = std::path::Path::new(&russel_dir).exists();
        if has_backup {
            if let Err(e) = tokio::fs::rename(&russel_dir, &russel_bak).await {
                anyhow::bail!("failed to backup russel directory: {}", e);
            }
            if let Err(e) = tokio::fs::rename(&microvms_dir, &microvms_bak).await {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                anyhow::bail!("failed to backup microvms directory: {}", e);
            }
        }

        // Take the old processes from state
        let (old_vm_proc, old_aux_procs) = self
            .state
            .take_processes(service_id)
            .unwrap_or((None, Vec::new()));

        // Teardown the old VM
        if let Err(e) = self.runner.destroy(service_id).await {
            self.state
                .restore_processes(service_id, old_vm_proc, old_aux_procs);
            if has_backup {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                let _ = tokio::fs::rename(&microvms_bak, &microvms_dir).await;
            }
            anyhow::bail!("failed to teardown old VM: {}", e);
        }

        let mut port_reservation = None;
        let deploy_result = async {
            port_reservation = Some(PortReservation::new(service_id));
            let port = match request.port.clone() {
                Some(p) => {
                    PortAllocator::reserve(service_id, p.host)?;
                    p
                }
                None => {
                    let ports = self.ports.clone();
                    let service_id = service_id.to_string();
                    let host =
                        tokio::task::spawn_blocking(move || ports.next(&service_id)).await??;
                    PortMapping {
                        host,
                        guest: config.service.port,
                    }
                }
            };
            let alloc: SubnetAllocation = subnet_for(service_id);
            tracing::info!(
                service_id,
                host = port.host,
                guest = port.guest,
                vm_ip = %alloc.vm_ip,
                "allocated"
            );

            // ── 4. Create phase: write deploy.env + get agent initramfs ────
            let t = Instant::now();
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "create".into(),
                    description: "Writing deploy config".into(),
                })
                .await;

            let bin_name = config.service.bin_name().to_string();
            let mem_mb = config.service.memory.as_mebibytes();
            let app_path = format!("{}/bin/{bin_name}", build.store_path.display());

            // Write deploy.env for the agent init.
            let cfg_dir = format!("{russel_dir}/cfg");
            std::fs::create_dir_all(&cfg_dir)?;
            let deploy_env = format!(
                "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
                alloc.vm_ip, alloc.host_ip, port.guest, app_path
            );
            std::fs::write(format!("{cfg_dir}/deploy.env"), deploy_env)?;

            // Get the agent initramfs (cached after first use).
            let initramfs_path = self.runner.build_agent_initramfs().await?;

            let create_ms = t.elapsed().as_millis();
            tracing::info!(service_id, create_ms, "deploy.env written, agent initramfs ready");

            // ── 5. TAP + socat + boot/restore VM ───────────────────────────
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "start".into(),
                    description: "Setting up network + booting/restoring VM".into(),
                })
                .await;
            tracing::info!(service_id, "setting up TAP + socat + booting VM");

            // Step A: create TAP + setup port forwarding (socat)
            let t_net = Instant::now();
            let socat_child =
                TapForwarder::setup(service_id, &alloc, port.host, port.guest).await?;
            let network_ms = t_net.elapsed().as_millis();

            // Step B: restore from warm pool or cold boot.
            let t_start = Instant::now();
            let cfg_dir_path = PathBuf::from(&cfg_dir);
            let pool = shared_warm_pool();
            let BootOutput {
                vm_child,
                virtiofsd_children,
            } = pool
                .restore_or_boot(
                    service_id,
                    &kernel_info.path,
                    &initramfs_path,
                    &alloc,
                    mem_mb,
                    &cfg_dir_path,
                )
                .await?;

            // Write metadata.json for precise and secure cleanup
            let metadata_path = format!("{russel_dir}/metadata.json");
            let virtiofsd_pids: Vec<serde_json::Value> = virtiofsd_children
                .iter()
                .filter_map(|c| c.id().map(|id| serde_json::Value::Number(id.into())))
                .collect();
            let metadata = serde_json::json!({
                "service_id": service_id,
                "host_port": port.host,
                "guest_port": port.guest,
                "vm_ip": alloc.vm_ip,
                "host_ip": alloc.host_ip,
                "vm_pid": vm_child.id(),
                "virtiofsd_pids": virtiofsd_pids,
                "socat_pid": socat_child.id(),
                "kernel_path": kernel_info.path.to_string_lossy(),
                "mem_mb": mem_mb,
                "app_path": app_path,
            });
            let content = serde_json::to_string_pretty(&metadata)
                .map_err(|e| anyhow::anyhow!("failed to serialize metadata: {}", e))?;
            std::fs::write(&metadata_path, content).map_err(|e| {
                anyhow::anyhow!("failed to write metadata to {}: {}", metadata_path, e)
            })?;

            // Create marker directory for MicrovmRunner::list() discovery
            let microvms_marker = format!("/var/lib/microvms/{}", service_id);
            std::fs::create_dir_all(&microvms_marker).map_err(|e| {
                anyhow::anyhow!("failed to create marker dir {}: {}", microvms_marker, e)
            })?;

            let start_ms = t_start.elapsed().as_millis();
            tracing::info!(service_id, network_ms, start_ms, "network + VM booted/restored");

            // ── 6. Wait for VM service to be reachable ─────────────────────
            let t = Instant::now();
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "ready".into(),
                    description: "Waiting for VM service to be reachable".into(),
                })
                .await;
            tracing::info!(
                service_id,
                vm_ip = %alloc.vm_ip,
                guest_port = port.guest,
                "polling VM readiness"
            );
            let up =
                TapForwarder::wait_for_vm_port(&alloc.vm_ip, port.guest, Duration::from_secs(10))
                    .await;
            let ready_ms = t.elapsed().as_millis();
            if !up {
                anyhow::bail!("VM not reachable in 10s");
            }
            tracing::info!(service_id, ready_ms, "VM service reachable");

            Ok::<_, anyhow::Error>((
                port,
                alloc,
                vm_child,
                virtiofsd_children,
                socat_child,
                initramfs_path,
                create_ms,
                start_ms,
                network_ms,
                ready_ms,
            ))
        }
        .await;

        let (
            port,
            alloc,
            vm_child,
            virtiofsd_children,
            socat_child,
            initramfs_path,
            create_ms,
            start_ms,
            network_ms,
            ready_ms,
        ) = match deploy_result {
            Ok(val) => val,
            Err(deploy_err) => {
                tracing::error!(service_id, error = %deploy_err, "Deployment failed — cleaning up and attempting rollback");
                let _ = self.runner.destroy(service_id).await;
                if has_backup {
                    let rollback_res = async {
                        tokio::fs::rename(&russel_bak, &russel_dir).await?;
                        tokio::fs::rename(&microvms_bak, &microvms_dir).await?;
                        let old_metadata_path = format!("{}/metadata.json", russel_dir);
                        let content = std::fs::read_to_string(&old_metadata_path)?;
                        let old_meta: serde_json::Value = serde_json::from_str(&content)?;
                        let old_host_port = old_meta["host_port"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("missing host_port"))?
                            as u16;
                        let old_guest_port = old_meta["guest_port"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("missing guest_port"))?
                            as u16;
                        let old_mem_mb = old_meta["mem_mb"].as_u64().unwrap_or(512) as u16;
                        let old_kernel_path = PathBuf::from(
                            old_meta["kernel_path"]
                                .as_str()
                                .unwrap_or("/nix/store/kernel"),
                        );
                        let old_app_path = old_meta["app_path"]
                            .as_str()
                            .unwrap_or("/nix/store/fallback");
                        let old_vm_ip = old_meta["vm_ip"]
                            .as_str()
                            .unwrap_or("10.0.0.2");
                        let old_host_ip = old_meta.get("host_ip")
                            .and_then(|v| v.as_str())
                            .unwrap_or("10.0.0.1");
                        let old_alloc = subnet_for(service_id);

                        // Write deploy.env from old metadata so the agent init works.
                        let cfg_dir = format!("{russel_dir}/cfg");
                        std::fs::create_dir_all(&cfg_dir)?;
                        let deploy_env = format!(
                            "VM_IP={old_vm_ip}\nHOST_IP={old_host_ip}\nPORT={old_guest_port}\nAPP={old_app_path}\n",
                        );
                        std::fs::write(format!("{cfg_dir}/deploy.env"), deploy_env)?;

                        // Use agent initramfs (cached), not the old per-service cpio.
                        let agent_initramfs = self.runner.build_agent_initramfs().await?;

                        PortAllocator::reserve(service_id, old_host_port)?;
                        let old_socat = TapForwarder::setup(
                            service_id,
                            &old_alloc,
                            old_host_port,
                            old_guest_port,
                        )
                        .await?;
                        let old_socat_pid = old_socat.id();

                        let cfg_dir_path = PathBuf::from(&cfg_dir);
                        let pool = shared_warm_pool();
                        let old_boot = pool
                            .cold_boot(
                                service_id,
                                &old_kernel_path,
                                &agent_initramfs,
                                &old_alloc,
                                old_mem_mb,
                                &cfg_dir_path,
                            )
                            .await?;

                        let old_vm_pid = old_boot.vm_child.id();
                        let virtiofsd_pids: Vec<serde_json::Value> = old_boot
                            .virtiofsd_children
                            .iter()
                            .filter_map(|c| c.id().map(|id| serde_json::Value::Number(id.into())))
                            .collect();
                        let mut old_aux = vec![old_socat];
                        old_aux.extend(old_boot.virtiofsd_children);

                        self.state.mark_deployed_with_aux(
                            service_id,
                            old_boot.vm_child,
                            old_aux,
                        );

                        let new_metadata = serde_json::json!({
                            "service_id": service_id,
                            "host_port": old_host_port,
                            "guest_port": old_guest_port,
                            "vm_ip": old_vm_ip,
                            "host_ip": old_host_ip,
                            "vm_pid": old_vm_pid,
                            "virtiofsd_pids": virtiofsd_pids,
                            "socat_pid": old_socat_pid,
                            "kernel_path": old_kernel_path.to_string_lossy(),
                            "mem_mb": old_mem_mb,
                            "app_path": old_app_path,
                        });
                        if let Ok(c) = serde_json::to_string_pretty(&new_metadata) {
                            let _ = std::fs::write(old_metadata_path, c);
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;

                    match rollback_res {
                        Ok(()) => {
                            tracing::info!(service_id, "Rollback to previous VM succeeded");
                            port_reservation
                                .as_mut()
                                .expect("port reservation exists")
                                .disarm();
                            return Err(anyhow::anyhow!("ROLLBACK_SUCCESS: {}", deploy_err));
                        }
                        Err(rollback_err) => {
                            tracing::error!(service_id, error = %rollback_err, "CRITICAL: Rollback failed. Old VM could not be restored.");
                        }
                    }
                }
                return Err(deploy_err);
            }
        };

        // Register with Traefik before committing the deployment.
        if let Err(error) = self.traefik.register(service_id, port.host).await {
            if let Err(teardown_error) = TapForwarder::teardown(&alloc).await {
                tracing::warn!(service_id, error = %teardown_error, "failed to tear down TAP after Traefik registration failure");
            }
            PortAllocator::release(service_id);
            return Err(error);
        }

        // Clean up old resources
        if has_backup {
            let _ = Command::new("rm")
                .args(["-rf", &format!("{}.bak", russel_dir)])
                .output()
                .await;
            let _ = Command::new("rm")
                .args(["-rf", &format!("{}.bak", microvms_dir)])
                .output()
                .await;
        }
        if let Some(mut p) = old_vm_proc {
            let _ = p.kill().await;
        }
        for mut p in old_aux_procs {
            let _ = p.kill().await;
        }

        self.state.attach_flake_path(service_id, repo_path.clone());
        port_reservation
            .as_mut()
            .expect("port reservation exists")
            .disarm();

        Ok(DeployOutput {
            store_path: build.store_path,
            initramfs_path,
            alloc,
            vm_child,
            virtiofsd_children,
            socat_child,
            port,
            runtime: Some(config.service.runtime),
            timing: DeployTiming {
                resolve_ms,
                build_ms,
                create_ms,
                start_ms,
                network_ms,
                ready_ms,
            },
        })
    }
}

/// Maximum accepted size for a Russelfile (prevents huge-file DoS via config_path).
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Resolve `config_path` strictly under `repo_path`.
fn resolve_config_path(repo_path: &Path, config_path: &str) -> anyhow::Result<PathBuf> {
    if config_path.is_empty() {
        anyhow::bail!("config_path must not be empty");
    }

    let cfg = Path::new(config_path);
    if cfg.is_absolute() {
        anyhow::bail!("config_path must be relative to the repository root (got absolute path)");
    }

    for component in cfg.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("config_path must not contain '..' path components");
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("config_path must be relative to the repository root");
            }
        }
    }

    let repo_canon = repo_path.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "cannot resolve repository path {}: {e}",
            repo_path.display()
        )
    })?;

    let joined = repo_canon.join(cfg);
    let config_canon = joined.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "config_path '{}' not found under repository: {e}",
            config_path
        )
    })?;

    if !config_canon.starts_with(&repo_canon) {
        anyhow::bail!("config_path escapes repository root");
    }

    let meta = std::fs::metadata(&config_canon).map_err(|e| {
        anyhow::anyhow!("cannot stat config_path {}: {e}", config_canon.display())
    })?;
    if !meta.is_file() {
        anyhow::bail!("config_path must be a regular file");
    }
    if meta.len() > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "config_path exceeds maximum size of {} bytes",
            MAX_CONFIG_BYTES
        );
    }

    Ok(config_canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "russel-config-path-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_config(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn resolve_config_path_accepts_relative_file() {
        let repo = TempRepo::new();
        write_config(
            repo.path(),
            "Russelfile.toml",
            r#"[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#,
        );
        let resolved = resolve_config_path(repo.path(), "Russelfile.toml").unwrap();
        assert!(resolved.ends_with("Russelfile.toml"));
        assert!(resolved.starts_with(repo.path().canonicalize().unwrap()));
    }

    #[test]
    fn resolve_config_path_accepts_nested_relative() {
        let repo = TempRepo::new();
        write_config(
            repo.path(),
            "deploy/Russelfile.toml",
            r#"[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#,
        );
        let resolved = resolve_config_path(repo.path(), "deploy/Russelfile.toml").unwrap();
        assert!(resolved.ends_with("deploy/Russelfile.toml"));
    }

    #[test]
    fn resolve_config_path_rejects_absolute() {
        let repo = TempRepo::new();
        let err = resolve_config_path(repo.path(), "/etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("relative"), "{err}");
    }

    #[test]
    fn resolve_config_path_rejects_parent_dir() {
        let repo = TempRepo::new();
        let err = resolve_config_path(repo.path(), "../outside.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn resolve_config_path_rejects_missing() {
        let repo = TempRepo::new();
        let err = resolve_config_path(repo.path(), "missing.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn resolve_config_path_rejects_directory() {
        let repo = TempRepo::new();
        std::fs::create_dir(repo.path().join("subdir")).unwrap();
        let err = resolve_config_path(repo.path(), "subdir")
            .unwrap_err()
            .to_string();
        assert!(err.contains("regular file"), "{err}");
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn resolve_config_path_rejects_symlink_escape() {
        let repo = TempRepo::new();
        let outside = std::env::temp_dir().join(format!(
            "russel-config-path-outside-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.toml");
        std::fs::write(&outside_file, b"[service]\nname=\"x\"\nsource=\".\"\nport=1\nmemory=\"1mb\"\n").unwrap();
        std::os::unix::fs::symlink(&outside_file, repo.path().join("escape.toml")).unwrap();
        let err = resolve_config_path(repo.path(), "escape.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes repository root"), "{err}");
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn resolve_config_path_rejects_oversized() {
        let repo = TempRepo::new();
        let path = repo.path().join("huge.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"[service]\n").unwrap();
            let pad = vec![b'#'; MAX_CONFIG_BYTES as usize + 1];
            f.write_all(&pad).unwrap();
        }
        let err = resolve_config_path(repo.path(), "huge.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds maximum size"), "{err}");
    }
}

struct PortReservation {
    service_id: String,
    armed: bool,
}

impl PortReservation {
    fn new(service_id: &str) -> Self {
        Self {
            service_id: service_id.to_string(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        if self.armed {
            PortAllocator::release(&self.service_id);
        }
    }
}

struct DeployOutput {
    store_path: PathBuf,
    initramfs_path: PathBuf,
    alloc: SubnetAllocation,
    vm_child: tokio::process::Child,
    virtiofsd_children: Vec<tokio::process::Child>,
    socat_child: tokio::process::Child,
    port: PortMapping,
    runtime: Option<russel_core::config::RuntimeKind>,
    timing: DeployTiming,
}
