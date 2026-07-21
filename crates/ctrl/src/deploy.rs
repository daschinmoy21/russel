use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::process::Command;

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use std::collections::HashMap;

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployResponse, DeployTiming, PortMapping},
    config::{
        RuntimeKind, Russelfile, merge_env_maps, resolve_runtime, validate_env_map,
    },
};

use crate::{
    build::NixBuilder,
    container::{
        ContainerRunner, ContainerStartSpec, PreparedRootfs, RootfsSpec, default_base_dir,
        validate_podman_args_for_runtime,
    },
    database::DatabaseProvisioner,
    git::GitClient,
    metadata::{build_container_metadata, build_microvm_metadata, write_metadata},
    microvm::{self, BootOutput, KernelInfo, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    traefik::TraefikClient,
    warm_pool::shared_warm_pool,
};

pub use crate::metadata::prior_runtime_from_disk;

/// Typed outcome of `deploy_inner` — success, rollback, or hard failure.
enum DeployInnerResult {
    // Box large success payload (clippy large_enum_variant).
    Success(Box<DeployOutput>),
    RolledBack { runtime: RuntimeKind, error: String },
}

#[derive(Debug)]
pub struct DeployPipeline {
    state: AppState,
    git: GitClient,
    builder: NixBuilder,
    #[allow(dead_code)]
    database: DatabaseProvisioner,
    runner: MicrovmRunner,
    containers: ContainerRunner,
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
            containers: ContainerRunner::new(),
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

        // Validate service_id before touching any state (#4).
        if let Err(e) = MicrovmRunner::validate_service_id(&service_id) {
            tracing::error!(service_id = %service_id, error = %e, "deploy rejected: invalid service_id");
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
            Ok(DeployInnerResult::Success(output)) => {
                let elapsed = started.elapsed().as_millis();
                let host_port = output.port.host;
                let guest_port = output.port.guest;
                tracing::info!(
                    service_id = %service_id,
                    elapsed_ms = elapsed,
                    host_port,
                    guest_port,
                    runtime = %output.runtime,
                    "deploy succeeded"
                );
                let (message, vm_ip, microvm_config_path) = match output.workload {
                    DeployWorkload::Microvm {
                        alloc,
                        initramfs_path,
                        vm_child,
                        virtiofsd_children,
                        socat_child,
                        ..
                    } => {
                        let mut aux = vec![*socat_child];
                        aux.extend(virtiofsd_children);
                        self.state
                            .mark_deployed_with_aux(&service_id, *vm_child, aux);
                        (
                            format!(
                                "microVM running. localhost:{host_port} -> {}:{guest_port}",
                                alloc.vm_ip
                            ),
                            Some(alloc.vm_ip),
                            Some(initramfs_path.display().to_string()),
                        )
                    }
                    DeployWorkload::Container {
                        container_id,
                        container_name,
                        rootfs_path,
                        ..
                    } => {
                        self.state
                            .mark_deployed_container(&service_id, &container_id);
                        (
                            format!(
                                "container {container_name} running. localhost:{host_port} -> guest:{guest_port}"
                            ),
                            None,
                            Some(rootfs_path.display().to_string()),
                        )
                    }
                };
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "deployed".to_string(),
                    store_path: Some(output.store_path.display().to_string()),
                    microvm_config_path,
                    runner_path: None,
                    port: Some(output.port),
                    elapsed_ms: elapsed,
                    timing: Some(output.timing),
                    vm_ip,
                    runtime: Some(output.runtime),
                    message,
                }
            }
            Ok(DeployInnerResult::RolledBack {
                runtime: restored_runtime,
                error: original_error,
            }) => {
                let elapsed = started.elapsed().as_millis();
                tracing::info!(
                    service_id = %service_id,
                    elapsed_ms = elapsed,
                    runtime = ?restored_runtime,
                    "deploy failed but successfully rolled back"
                );
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "rolled_back".to_string(),
                    store_path: None,
                    microvm_config_path: None,
                    runner_path: None,
                    port: None,
                    elapsed_ms: elapsed,
                    timing: None,
                    vm_ip: None,
                    runtime: Some(restored_runtime),
                    message: format!(
                        "deployment failed but rolled back successfully: {original_error}"
                    ),
                }
            }
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
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

    async fn deploy_inner(
        &self,
        service_id: &str,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<DeployInnerResult> {
        // ── 1. Resolve repo ──────────────────────────────────────────────────
        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "resolve".into(),
                description: "Resolving source & Russelfile".into(),
            })
            .await;
        let repo_path = self.git.clone_or_use_local(&request.repo_url).await?;
        let config = load_russelfile_under_repo(&repo_path, &request.config_path)?;
        let runtime = resolve_runtime(config.service.runtime, request.runtime)?;
        validate_podman_args_for_runtime(runtime, &request.podman_args)?;

        // Merge env: file < request (request wins on key conflict).
        let merged_env = merge_env_maps(&config.service.env, &request.env);
        validate_env_map(&merged_env)?;

        let resolve_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            service_name = %config.service.name,
            runtime = %runtime,
            resolve_ms,
            "repo resolved"
        );

        // ── 2. Nix build (+ kernel stack only for microVM) ───────────────────
        let t = Instant::now();
        let build_description = if runtime == RuntimeKind::Microvm {
            "Building Nix package + ensuring kernel/busybox/modules"
        } else {
            "Building Nix package"
        };
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "build".into(),
                description: build_description.into(),
            })
            .await;
        let build = self.builder.build(&repo_path).await?;
        let kernel = if runtime == RuntimeKind::Microvm {
            Some(self.runner.ensure_kernel().await?)
        } else {
            None
        };
        let _busybox = if runtime == RuntimeKind::Microvm {
            Some(self.runner.ensure_busybox().await?)
        } else {
            None
        };
        let _kernel_modules = if runtime == RuntimeKind::Microvm {
            Some(self.runner.ensure_kernel_modules().await?)
        } else {
            None
        };
        let build_ms = t.elapsed().as_millis();
        if runtime == RuntimeKind::Microvm {
            let ki = kernel.as_ref().unwrap();
            tracing::info!(
                service_id,
                store = %build.store_path.display(),
                kernel = %ki.path.display(),
                builtin = ki.drivers_builtin,
                build_ms,
                "build complete (kernel + busybox + modules cached)"
            );
        } else {
            tracing::info!(
                service_id,
                store = %build.store_path.display(),
                build_ms,
                "build complete"
            );
        }

        let prior_runtime = resolve_prior_runtime(service_id).await;

        let russel_dir = format!("/var/lib/russel/{}", service_id);
        let microvms_dir = format!("/var/lib/microvms/{}", service_id);
        let russel_bak = format!("{}.bak", russel_dir);
        let microvms_bak = format!("{}.bak", microvms_dir);
        let has_russel_dir = Path::new(&russel_dir).exists();
        let has_microvms_dir = Path::new(&microvms_dir).exists();
        let has_backup = has_russel_dir;

        // Order (fixes #120):
        // 1. take_processes — disarm supervisor first
        // 2. kill+wait old children so ports are freed
        // 3. rename dirs to .bak (for rollback)  
        // 4. destroy_prior_runtime — cleans TAP/ports without needing metadata
        let (old_vm_proc, old_aux_procs) = self
            .state
            .take_processes(service_id)
            .unwrap_or((None, Vec::new()));

        // Kill+wait taken children first so ports are free before destroy.
        kill_and_wait_children(old_vm_proc, old_aux_procs).await;

        // Now rename dirs to .bak for rollback content.
        if has_russel_dir {
            if let Err(e) = tokio::fs::rename(&russel_dir, &russel_bak).await {
                anyhow::bail!("failed to backup russel directory: {}", e);
            }
            if has_microvms_dir
                && let Err(e) = tokio::fs::rename(&microvms_dir, &microvms_bak).await
            {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                anyhow::bail!("failed to backup microvms directory: {}", e);
            }
        }

        // Destroy prior runtime: processes already reaped, so stop is no-op;
        // destroy still tears down TAP, releases port, removes .bak dirs.
        if let Some(prior_kind) = prior_runtime {
            if let Err(e) =
                destroy_prior_runtime(prior_kind, service_id, &self.runner, &self.containers).await
            {
                if has_backup {
                    let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                    if has_microvms_dir {
                        let _ = tokio::fs::rename(&microvms_bak, &microvms_dir).await;
                    }
                }
                anyhow::bail!("failed to teardown prior {}: {}", prior_kind, e);
            }
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

            match runtime {
                RuntimeKind::Microvm => {
                    self.deploy_microvm(
                        service_id,
                        &config,
                        &build.store_path,
                        &port,
                        kernel.as_ref().unwrap(),
                        &merged_env,
                        &tx,
                    )
                    .await
                }
                RuntimeKind::Container => {
                    self.deploy_container(
                        service_id,
                        &config,
                        &build.store_path,
                        &port,
                        &request.podman_args,
                        &merged_env,
                        &tx,
                    )
                    .await
                }
            }
        }
        .await;

        let (workload, create_ms, start_ms, network_ms, ready_ms) = match deploy_result {
            Ok(val) => val,
            Err(deploy_err) => {
                tracing::error!(service_id, error = %deploy_err, "Deployment failed — cleaning up and attempting rollback (prior runtime was {prior_runtime:?})");
                cleanup_failed_deploy(runtime, service_id, &self.runner, &self.containers).await;
                if has_backup && prior_runtime == Some(RuntimeKind::Microvm) {
                    let rollback_res = attempt_microvm_rollback(
                        service_id,
                        &russel_dir,
                        &microvms_dir,
                        &russel_bak,
                        &microvms_bak,
                        has_microvms_dir,
                        &self.runner,
                        &self.state,
                    )
                    .await;

                    match rollback_res {
                        Ok(()) => {
                            tracing::info!(service_id, "Rollback to previous VM succeeded");
                            port_reservation
                                .as_mut()
                                .expect("port reservation exists")
                                .disarm();
                            return Ok(DeployInnerResult::RolledBack {
                                runtime: prior_runtime.unwrap_or(RuntimeKind::Microvm),
                                error: deploy_err.to_string(),
                            });
                        }
                        Err(rollback_err) => {
                            tracing::error!(service_id, error = %rollback_err, "CRITICAL: Rollback failed. Old VM could not be restored.");
                        }
                    }
                } else if has_backup && prior_runtime == Some(RuntimeKind::Container) {
                    let rollback_res = attempt_container_rollback(
                        service_id,
                        &russel_dir,
                        &russel_bak,
                        &self.containers,
                        &self.state,
                    )
                    .await;

                    match rollback_res {
                        Ok(()) => {
                            tracing::info!(service_id, "Rollback to previous container succeeded");
                            port_reservation
                                .as_mut()
                                .expect("port reservation exists")
                                .disarm();
                            return Ok(DeployInnerResult::RolledBack {
                                runtime: prior_runtime.unwrap_or(RuntimeKind::Container),
                                error: deploy_err.to_string(),
                            });
                        }
                        Err(rollback_err) => {
                            tracing::error!(service_id, error = %rollback_err, "CRITICAL: Container rollback failed. Old container could not be restored.");
                        }
                    }
                } else if has_backup {
                    restore_backup_dirs(
                        &russel_dir,
                        &microvms_dir,
                        &russel_bak,
                        &microvms_bak,
                        has_microvms_dir,
                    )
                    .await;
                }
                return Err(deploy_err);
            }
        };

        if let Err(error) = self
            .traefik
            .register(service_id, workload.port().host)
            .await
        {
            // Full workload teardown on traefik registration failure (#116).
            workload.teardown_network().await;
            match &workload {
                DeployWorkload::Microvm { .. } => {
                    if let Err(e) = self.runner.destroy(service_id).await {
                        tracing::warn!(service_id, error = %e, "failed to destroy microVM after traefik failure");
                    }
                }
                DeployWorkload::Container { .. } => {
                    if let Err(e) = self.containers.destroy(service_id).await {
                        tracing::warn!(service_id, error = %e, "failed to destroy container after traefik failure");
                    }
                }
            }
            PortAllocator::release(service_id);
            return Err(error);
        }

        if has_backup {
            let _ = Command::new("rm")
                .args(["-rf", &format!("{}.bak", russel_dir)])
                .output()
                .await;
            if has_microvms_dir {
                let _ = Command::new("rm")
                    .args(["-rf", &format!("{}.bak", microvms_dir)])
                    .output()
                    .await;
            }
        }

        self.state.attach_flake_path(service_id, repo_path.clone());
        port_reservation
            .as_mut()
            .expect("port reservation exists")
            .disarm();

        Ok(DeployInnerResult::Success(Box::new(DeployOutput {
            store_path: build.store_path,
            port: workload.port().clone(),
            runtime,
            timing: DeployTiming {
                resolve_ms,
                build_ms,
                create_ms,
                start_ms,
                network_ms,
                ready_ms,
            },
            workload,
        })))
    }

    async fn deploy_microvm(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        port: &PortMapping,
        kernel_info: &KernelInfo,
        env: &HashMap<String, String>,
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        let alloc: SubnetAllocation = subnet_for(service_id);
        tracing::info!(
            service_id,
            host = port.host,
            guest = port.guest,
            vm_ip = %alloc.vm_ip,
            "allocated"
        );

        let bin_name = config.service.bin_name().to_string();
        validate_bin_name(&bin_name)?;
        let mem_mb = config.service.memory.as_mebibytes();
        let app_path = format!("{}/bin/{bin_name}", store_path.display());

        // ── Write deploy.env for the agent init ─────────────────────────
        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "create".into(),
                description: "Writing deploy config".into(),
            })
            .await;

        let cfg_dir = format!("/var/lib/russel/{}/cfg", service_id);
        std::fs::create_dir_all(&cfg_dir)
            .map_err(|e| anyhow::anyhow!("failed to create config dir {}: {}", cfg_dir, e))?;
        // Shell-quote APP path to prevent injection through deploy.env
        let mut deploy_env = format!(
            "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
            alloc.vm_ip,
            alloc.host_ip,
            port.guest,
            shell_quote(&app_path)
        );
        // Append user env vars, shell-quoted.
        for (key, value) in env {
            deploy_env.push_str(&format!("{}={}\n", key, shell_quote(value)));
        }
        std::fs::write(format!("{cfg_dir}/deploy.env"), deploy_env)?;

        // Agent initramfs is cached after first use — no app baked in.
        let initramfs_path = self.runner.build_agent_initramfs().await?;

        let create_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            create_ms,
            "deploy.env written, agent initramfs ready"
        );

        // ── TAP + socat + boot/restore VM ──────────────────────────────
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "start".into(),
                description: "Setting up network + booting/restoring VM".into(),
            })
            .await;
        tracing::info!(service_id, "setting up TAP + socat + booting VM");

        let t_net = Instant::now();
        let socat_child = TapForwarder::setup(service_id, &alloc, port.host, port.guest).await?;
        let network_ms = t_net.elapsed().as_millis();

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

        // ── Write metadata via build_microvm_metadata ────────────────
        let metadata_path = format!("/var/lib/russel/{}/metadata.json", service_id);
        let v_pids: Vec<u32> = virtiofsd_children.iter().filter_map(|c| c.id()).collect();
        let meta = build_microvm_metadata(
            service_id,
            port.host,
            port.guest,
            &alloc.vm_ip,
            &alloc.host_ip,
            vm_child.id(),
            &v_pids,
            socat_child.id(),
            &kernel_info.path.display().to_string(),
            &store_path.display().to_string(),
            mem_mb,
            Some(&app_path),
            Some(&bin_name),
            Some(&initramfs_path.display().to_string()),
        );
        write_metadata(&metadata_path, &meta)?;

        // Create marker directory for MicrovmRunner::list() discovery
        let microvms_marker = format!("/var/lib/microvms/{}", service_id);
        std::fs::create_dir_all(&microvms_marker).map_err(|e| {
            anyhow::anyhow!("failed to create marker dir {}: {}", microvms_marker, e)
        })?;

        let start_ms = t_start.elapsed().as_millis();
        tracing::info!(
            service_id,
            network_ms,
            start_ms,
            "network + VM booted/restored"
        );

        // ── Wait for VM service to be reachable ────────────────────────
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
            TapForwarder::wait_for_vm_port(&alloc.vm_ip, port.guest, Duration::from_secs(10)).await;
        let ready_ms = t.elapsed().as_millis();
        if !up {
            let console_log = format!("/var/lib/russel/{}/console.log", service_id);
            let cfg_env = format!("{cfg_dir}/deploy.env");
            let mut detail = String::from("VM not reachable in 10s");
            detail.push_str(&format!(
                "\nvm_ip={}:{} deploy.env_exists={} console={}",
                alloc.vm_ip,
                port.guest,
                Path::new(&cfg_env).exists(),
                console_log
            ));
            detail.push_str(
                "\nhint: snapshot warm pool is off unless RUSSEL_WARM_POOL=1 (experimental)",
            );
            if let Ok(raw) = std::fs::read_to_string(&console_log) {
                let lines: Vec<&str> = raw.lines().collect();
                let start = lines.len().saturating_sub(80);
                let tail = if start < lines.len() {
                    lines[start..].join("\n")
                } else {
                    raw
                };
                detail.push_str("\n--- guest console tail ---\n");
                detail.push_str(&tail);
                detail.push_str("\n--- end console ---");
            } else {
                detail.push_str("\n(no console.log — guest may have failed before serial)");
            }
            anyhow::bail!("{detail}");
        }
        tracing::info!(service_id, ready_ms, "VM service reachable");

        Ok((
            DeployWorkload::Microvm {
                alloc,
                vm_child: Box::new(vm_child),
                virtiofsd_children,
                socat_child: Box::new(socat_child),
                initramfs_path,
                port: port.clone(),
            },
            create_ms,
            start_ms,
            network_ms,
            ready_ms,
        ))
    }

    async fn deploy_container(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        port: &PortMapping,
        podman_args: &[String],
        env: &HashMap<String, String>,
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        tracing::info!(
            service_id,
            host = port.host,
            guest = port.guest,
            "allocated container port"
        );

        let bin_name = config.service.bin_name().to_string();
        validate_bin_name(&bin_name)?;
        let mem_mb = config.service.memory.as_mebibytes();
        let base_dir = default_base_dir(service_id);

        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "create".into(),
                description: "Preparing container rootfs".into(),
            })
            .await;
        let rootfs_spec = RootfsSpec {
            service_id: service_id.to_string(),
            store_path: store_path.to_path_buf(),
            bin_name: bin_name.clone(),
            base_dir: base_dir.clone(),
            bash_store: None,
            curl_store: None,
        };
        let prepared = self.containers.prepare(&rootfs_spec).await?;
        let create_ms = t.elapsed().as_millis();
        tracing::info!(service_id, create_ms, "container rootfs ready");

        let _ = tx
            .send(DeployEvent::Progress {
                phase: "start".into(),
                description: "Starting rootless Podman container".into(),
            })
            .await;
        let t_start = Instant::now();
        let start_spec = ContainerStartSpec {
            service_id: service_id.to_string(),
            rootfs: prepared.clone(),
            host_port: port.host,
            guest_port: port.guest,
            memory_mb: mem_mb,
            env: build_container_env(port.guest, env),
            extra_args: podman_args.to_vec(),
        };
        let running = self.containers.start(&start_spec).await?;
        let start_ms = t_start.elapsed().as_millis();
        let network_ms = 0u128;
        tracing::info!(service_id, start_ms, "container started");

        let metadata_path = base_dir.join("metadata.json");
        let metadata = build_container_metadata(
            service_id,
            port.host,
            port.guest,
            &store_path.to_string_lossy(),
            &running.container_id,
            &running.container_name,
            &running.rootfs_path.to_string_lossy(),
            mem_mb,
            Some(&bin_name),
            podman_args,
        );
        write_metadata(&metadata_path, &metadata)?;

        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "ready".into(),
                description: "Waiting for container service to be reachable".into(),
            })
            .await;
        tracing::info!(
            service_id,
            host_port = port.host,
            "polling container readiness"
        );
        let up = TapForwarder::wait_for_host_port(port.host, Duration::from_secs(10)).await;
        let ready_ms = t.elapsed().as_millis();
        if !up {
            anyhow::bail!("container not reachable on 127.0.0.1:{} in 10s", port.host);
        }
        tracing::info!(service_id, ready_ms, "container service reachable");

        Ok((
            DeployWorkload::Container {
                container_id: running.container_id,
                container_name: running.container_name,
                rootfs_path: running.rootfs_path,
                port: port.clone(),
            },
            create_ms,
            start_ms,
            network_ms,
            ready_ms,
        ))
    }
}

/// Resolve the prior runtime for a service before redeploy.
///
/// 1. Check on-disk metadata first (`prior_runtime_from_disk`).
/// 2. If no metadata (None), probe for a podman container named `russel-{service_id}`.
/// 3. If no container but the internal dirs exist, treat as legacy Microvm.
/// 4. Otherwise None (first deploy).
async fn resolve_prior_runtime(service_id: &str) -> Option<RuntimeKind> {
    if let Some(runtime) = prior_runtime_from_disk(service_id) {
        return Some(runtime);
    }

    // Probe podman for a running/stopped container with the russel label.
    let container_name = format!("russel-{}", service_id);
    let probe = Command::new("podman")
        .args(["container", "exists", &container_name])
        .output()
        .await;
    if let Ok(out) = &probe {
        if out.status.success() {
            tracing::info!(
                service_id,
                container = %container_name,
                "discovered existing podman container (no metadata)"
            );
            return Some(RuntimeKind::Container);
        }
    }

    // No metadata and no container: if any russel/microvms directory exists,
    // assume legacy Microvm so teardown can proceed correctly.
    let russel_dir = format!("/var/lib/russel/{}", service_id);
    let microvms_dir = format!("/var/lib/microvms/{}", service_id);
    if Path::new(&russel_dir).exists() || Path::new(&microvms_dir).exists() {
        tracing::info!(
            service_id,
            "no metadata but dirs exist — treating prior as legacy Microvm"
        );
        return Some(RuntimeKind::Microvm);
    }

    None
}

/// Kill + wait (with timeout, then force) all old children so ports are free.
async fn kill_and_wait_children(
    old_vm_proc: Option<tokio::process::Child>,
    old_aux_procs: Vec<tokio::process::Child>,
) {
    let mut children: Vec<tokio::process::Child> = old_vm_proc.into_iter().collect();
    children.extend(old_aux_procs);
    for mut child in children {
        let _ = child.kill().await;
        let wait = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        if !matches!(wait, Ok(Ok(_))) {
            // Force kill if still alive after timeout.
            tracing::warn!("child process did not exit gracefully after SIGKILL");
        }
    }
}

async fn destroy_prior_runtime(
    prior: RuntimeKind,
    service_id: &str,
    microvm_runner: &MicrovmRunner,
    container_runner: &ContainerRunner,
) -> anyhow::Result<()> {
    match prior {
        RuntimeKind::Microvm => microvm_runner.destroy(service_id).await,
        RuntimeKind::Container => {
            container_runner.destroy(service_id).await?;
            PortAllocator::release(service_id);
            Ok(())
        }
    }
}

async fn cleanup_failed_deploy(
    runtime: RuntimeKind,
    service_id: &str,
    microvm_runner: &MicrovmRunner,
    container_runner: &ContainerRunner,
) {
    let _ = match runtime {
        RuntimeKind::Microvm => microvm_runner.destroy(service_id).await,
        RuntimeKind::Container => {
            let result = container_runner.destroy(service_id).await;
            PortAllocator::release(service_id);
            result
        }
    };
}

async fn restore_backup_dirs(
    russel_dir: &str,
    microvms_dir: &str,
    russel_bak: &str,
    microvms_bak: &str,
    has_microvms_backup: bool,
) {
    let _ = tokio::fs::rename(russel_bak, russel_dir).await;
    if has_microvms_backup {
        let _ = tokio::fs::rename(microvms_bak, microvms_dir).await;
    }
}

async fn attempt_microvm_rollback(
    service_id: &str,
    russel_dir: &str,
    microvms_dir: &str,
    russel_bak: &str,
    microvms_bak: &str,
    has_microvms_backup: bool,
    runner: &MicrovmRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    // 1. Restore backup dirs
    tokio::fs::rename(russel_bak, russel_dir).await?;
    if has_microvms_backup {
        tokio::fs::rename(microvms_bak, microvms_dir).await?;
    }

    // 2. Parse metadata fail-closed (no silent defaults for required fields)
    let old_metadata_path = format!("{}/metadata.json", russel_dir);
    let content = std::fs::read_to_string(&old_metadata_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;

    let host_port = u16::try_from(
        old_meta["host_port"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing host_port"))?,
    )
    .map_err(|_| anyhow::anyhow!("host_port out of u16 range"))?;
    let guest_port = u16::try_from(
        old_meta["guest_port"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing guest_port"))?,
    )
    .map_err(|_| anyhow::anyhow!("guest_port out of u16 range"))?;

    let kernel_path_str = old_meta["kernel_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing kernel_path"))?;
    let kernel_path = PathBuf::from(kernel_path_str);
    if !kernel_path.exists() {
        anyhow::bail!("kernel_path does not exist: {}", kernel_path.display());
    }

    let mem_mb = u16::try_from(
        old_meta["mem_mb"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing mem_mb"))?,
    )
    .map_err(|_| anyhow::anyhow!("mem_mb out of u16 range"))?;

    let bin_name = old_meta["bin_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing bin_name"))?
        .to_string();

    // Resolve app/store paths. Prefer explicit fields; support legacy metadata
    // that stored the Nix store directory in `app_path` and omitted `store_path`.
    let (app_path, store_path) = resolve_rollback_app_paths(
        old_meta["app_path"].as_str(),
        old_meta["store_path"].as_str(),
        &bin_name,
    )?;
    if !Path::new(&app_path).exists() {
        anyhow::bail!("app_path does not exist: {app_path}");
    }

    // Require initramfs key; rebuild only when the recorded path is gone on disk.
    let initramfs_recorded = old_meta["initramfs"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing initramfs"))?;
    let initramfs_path = {
        let recorded = PathBuf::from(initramfs_recorded);
        if recorded.exists() {
            recorded
        } else {
            tracing::warn!(
                service_id,
                path = %recorded.display(),
                "recorded initramfs missing on disk — rebuilding agent initramfs"
            );
            runner.build_agent_initramfs().await?
        }
    };

    // 3. Always rewrite deploy.env so legacy/stale APP values cannot stick
    let alloc = subnet_for(service_id);
    let cfg_dir = format!("{}/cfg", russel_dir);
    std::fs::create_dir_all(&cfg_dir)?;
    let deploy_env = format!(
        "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
        alloc.vm_ip,
        alloc.host_ip,
        guest_port,
        shell_quote(&app_path)
    );
    std::fs::write(format!("{cfg_dir}/deploy.env"), deploy_env)?;

    // 4. Reserve port
    PortAllocator::reserve(service_id, host_port)?;

    // Helper: tear down anything acquired after port reservation.
    async fn cleanup_rollback_resources(
        service_id: &str,
        alloc: &SubnetAllocation,
        runner: &MicrovmRunner,
        vm_child: Option<tokio::process::Child>,
        aux: Option<Vec<tokio::process::Child>>,
    ) {
        drop(vm_child);
        drop(aux);
        if let Err(e) = runner.destroy(service_id).await {
            tracing::warn!(service_id, error = %e, "failed to destroy partial rollback microVM");
        }
        let _ = TapForwarder::teardown(alloc).await;
        PortAllocator::release(service_id);
    }

    // 5–6. Network + boot; clean up on any failure after reservation
    let cfg_dir_path = PathBuf::from(&cfg_dir);
    let boot_result: anyhow::Result<(
        tokio::process::Child,
        Vec<tokio::process::Child>,
        tokio::process::Child,
    )> = async {
        let socat_child = TapForwarder::setup(service_id, &alloc, host_port, guest_port).await?;
        let pool = shared_warm_pool();
        let BootOutput {
            vm_child,
            virtiofsd_children,
        } = pool
            .restore_or_boot(
                service_id,
                &kernel_path,
                &initramfs_path,
                &alloc,
                mem_mb,
                &cfg_dir_path,
            )
            .await?;
        Ok((vm_child, virtiofsd_children, socat_child))
    }
    .await;

    let (vm_child, virtiofsd_children, socat_child) = match boot_result {
        Ok(v) => v,
        Err(e) => {
            cleanup_rollback_resources(service_id, &alloc, runner, None, None).await;
            return Err(e.context("microVM rollback boot failed"));
        }
    };

    let v_pids: Vec<u32> = virtiofsd_children.iter().filter_map(|c| c.id()).collect();

    // 7. Readiness BEFORE marking deployed / writing durable success metadata
    let ready =
        TapForwarder::wait_for_vm_port(&alloc.vm_ip, guest_port, Duration::from_secs(10)).await;
    if !ready {
        // Tear down partial restore; do not report rolled_back for a dead service.
        let mut aux = vec![socat_child];
        aux.extend(virtiofsd_children);
        cleanup_rollback_resources(service_id, &alloc, runner, Some(vm_child), Some(aux)).await;
        anyhow::bail!(
            "rolled-back microVM not reachable on {}:{guest_port} within 10s",
            alloc.vm_ip
        );
    }

    // 8. Write metadata + mark deployed only after readiness
    let meta = build_microvm_metadata(
        service_id,
        host_port,
        guest_port,
        &alloc.vm_ip,
        &alloc.host_ip,
        vm_child.id(),
        &v_pids,
        socat_child.id(),
        kernel_path_str,
        &store_path,
        mem_mb,
        Some(&app_path),
        Some(&bin_name),
        Some(&initramfs_path.display().to_string()),
    );
    if let Err(e) = write_metadata(&old_metadata_path, &meta) {
        let mut aux = vec![socat_child];
        aux.extend(virtiofsd_children);
        cleanup_rollback_resources(service_id, &alloc, runner, Some(vm_child), Some(aux)).await;
        return Err(e.context("microVM rollback metadata write failed"));
    }

    let mut aux = vec![socat_child];
    aux.extend(virtiofsd_children);
    state.mark_deployed_with_aux(service_id, vm_child, aux);

    Ok(())
}

/// Resolve `app_path` / `store_path` for rollback.
///
/// Current metadata writes both. Legacy writers stored the Nix store directory
/// in `app_path` and omitted `store_path`, so APP would be `/nix/store/<hash>`
/// instead of `/nix/store/<hash>/bin/<bin>`. Reconstruct in that case.
fn resolve_rollback_app_paths(
    app_from_meta: Option<&str>,
    store_from_meta: Option<&str>,
    bin_name: &str,
) -> anyhow::Result<(String, String)> {
    let bin_suffix = format!("/bin/{bin_name}");
    match (app_from_meta, store_from_meta) {
        (Some(app), Some(store)) => {
            // Bare store dir written as app_path (or missing /bin/<bin>) → reconstruct.
            if app.ends_with(&bin_suffix) {
                Ok((app.to_string(), store.to_string()))
            } else {
                Ok((format!("{store}{bin_suffix}"), store.to_string()))
            }
        }
        // Legacy: only app_path — may be full binary path or bare store dir.
        (Some(app), None) => {
            if let Some(store) = app.strip_suffix(&bin_suffix) {
                Ok((app.to_string(), store.to_string()))
            } else {
                // Treat app_path as the store directory.
                Ok((format!("{app}{bin_suffix}"), app.to_string()))
            }
        }
        (None, Some(store)) => Ok((format!("{store}{bin_suffix}"), store.to_string())),
        (None, None) => anyhow::bail!("missing app_path and store_path in rollback metadata"),
    }
}

async fn attempt_container_rollback(
    service_id: &str,
    russel_dir: &str,
    russel_bak: &str,
    containers: &ContainerRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    tokio::fs::rename(russel_bak, russel_dir).await?;
    let old_metadata_path = format!("{}/metadata.json", russel_dir);
    let content = std::fs::read_to_string(&old_metadata_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;
    let old_host_port = old_meta["host_port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing host_port in container metadata"))?
        as u16;
    let old_guest_port = old_meta["guest_port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing guest_port in container metadata"))?
        as u16;
    let old_mem_mb = old_meta["mem_mb"].as_u64().unwrap_or(512) as u16;
    let old_bin_name = old_meta["bin_name"].as_str().unwrap_or("app");
    let old_rootfs_path = old_meta["rootfs_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing rootfs_path in container metadata"))?;
    let old_podman_args: Vec<String> = old_meta
        .get("podman_args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    PortAllocator::reserve(service_id, old_host_port)?;

    let start_spec = ContainerStartSpec {
        service_id: service_id.to_string(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from(old_rootfs_path),
            entrypoint: PathBuf::from(format!("/bin/{}", old_bin_name)),
        },
        host_port: old_host_port,
        guest_port: old_guest_port,
        memory_mb: old_mem_mb,
        env: vec![("PORT".to_string(), old_guest_port.to_string())],
        extra_args: old_podman_args.clone(),
    };
    let running = containers.start(&start_spec).await?;

    // Do not mark deployed until the restored container is reachable.
    let ready = TapForwarder::wait_for_host_port(old_host_port, Duration::from_secs(10)).await;
    if !ready {
        if let Err(e) = containers.destroy(service_id).await {
            tracing::warn!(
                service_id,
                error = %e,
                "failed to destroy unready rolled-back container"
            );
        }
        PortAllocator::release(service_id);
        anyhow::bail!(
            "rolled-back container not reachable on host port {old_host_port} within 10s"
        );
    }

    state.mark_deployed_container(service_id, &running.container_id);

    let old_store_path = old_meta["store_path"]
        .as_str()
        .unwrap_or("/nix/store/unknown");
    let new_metadata = build_container_metadata(
        service_id,
        old_host_port,
        old_guest_port,
        old_store_path,
        &running.container_id,
        &running.container_name,
        &running.rootfs_path.to_string_lossy(),
        old_mem_mb,
        Some(old_bin_name),
        &old_podman_args,
    );
    write_metadata(&old_metadata_path, &new_metadata)?;
    Ok(())
}

/// Maximum accepted size for a Russelfile (prevents huge-file DoS via config_path).
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Validate a binary/service name for shell safety: only `[A-Za-z0-9._+-]`.
/// Rejects empty strings, whitespace, shell metacharacters.
pub fn validate_bin_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("bin_name must not be empty");
    }
    if name.len() > 256 {
        anyhow::bail!("bin_name too long (max 256 characters)");
    }
    let valid = name.bytes().all(|c| {
        c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'+' || c == b'-'
    });
    if !valid {
        anyhow::bail!(
            "bin_name '{}' contains invalid characters (only A-Za-z0-9._+- allowed)",
            name
        );
    }
    Ok(())
}

/// Shell-safe single-quoted value for deploy.env: escapes embedded `'` as `'\''`.
pub fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Build the env list for a container start spec: PORT first (managed),
/// then user env vars (already validated; PORT filtered out to prevent override).
pub fn build_container_env(
    guest_port: u16,
    user_env: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        vec![("PORT".to_string(), guest_port.to_string())];
    for (key, value) in user_env {
        if key == "PORT" {
            continue; // managed by Russel, user cannot override
        }
        env.push((key.clone(), value.clone()));
    }
    env
}

/// Validate relative path components of user `config_path` (no absolute / `..`).
fn validate_relative_config_path(config_path: &str) -> anyhow::Result<&Path> {
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
    Ok(cfg)
}

/// Open and parse a Russelfile strictly under `repo_path` without a
/// validate-then-reopen TOCTOU window on the path string.
///
/// Contract (security-sensitive):
/// - rejects empty, absolute, and `..` components
/// - opens the repository directory, then each parent component via `openat`
///   with `O_DIRECTORY | O_NOFOLLOW` (intermediate symlinks rejected; no
///   path-based open after `canonicalize` that could race with renames)
/// - opens the leaf with `openat(..., O_NOFOLLOW)` relative to the final
///   directory descriptor (final symlink rejected)
/// - `fstat`s the open fd for regular-file + size cap
/// - reads at most `MAX_CONFIG_BYTES` from the same open handle
///
/// Callers must load configuration only through this helper (or an equivalent
/// descriptor-relative open) so deploy never reopens a validated path string.
fn load_russelfile_under_repo(repo_path: &Path, config_path: &str) -> anyhow::Result<Russelfile> {
    let cfg = validate_relative_config_path(config_path)?;

    let file_name = cfg
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config_path '{}' has no file name", config_path))?;

    #[cfg(unix)]
    let mut file = {
        let repo_dir = open_directory_fd(repo_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repository path {}: {e}",
                repo_path.display()
            )
        })?;

        // Walk parent components with stable directory descriptors so a rename
        // of an intermediate directory cannot swap in a symlink after a path
        // canonicalize (path-based open would follow the new link).
        let mut dir_fd = repo_dir;
        if let Some(parent) = cfg.parent() {
            for component in parent.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(name) => {
                        dir_fd =
                            openat_directory_nofollow(dir_fd.as_raw_fd(), name).map_err(|e| {
                                anyhow::anyhow!(
                                    "config_path '{}' not found under repository: {e}",
                                    config_path
                                )
                            })?;
                    }
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        // Already rejected by validate_relative_config_path.
                        anyhow::bail!("config_path must be relative to the repository root");
                    }
                }
            }
        }

        openat_file_nofollow(dir_fd.as_raw_fd(), file_name).map_err(|e| {
            // Leaf symlink → ELOOP / "Too many levels of symbolic links"
            anyhow::anyhow!("cannot open config_path '{}': {e}", config_path)
        })?
    };

    #[cfg(not(unix))]
    let mut file = {
        let repo_canon = repo_path.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repository path {}: {e}",
                repo_path.display()
            )
        })?;
        let joined = repo_canon.join(cfg);
        let parent = joined.parent().ok_or_else(|| {
            anyhow::anyhow!("config_path '{}' has no parent directory", config_path)
        })?;
        let parent_canon = parent.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "config_path '{}' not found under repository: {e}",
                config_path
            )
        })?;
        if !parent_canon.starts_with(&repo_canon) {
            anyhow::bail!("config_path escapes repository root");
        }
        OpenOptions::new()
            .read(true)
            .open(parent_canon.join(file_name))
            .map_err(|e| anyhow::anyhow!("cannot open config_path '{}': {e}", config_path))?
    };

    let meta = file
        .metadata()
        .map_err(|e| anyhow::anyhow!("cannot stat opened config_path: {e}"))?;
    if !meta.is_file() {
        anyhow::bail!("config_path must be a regular file");
    }
    if meta.len() > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "config_path exceeds maximum size of {} bytes",
            MAX_CONFIG_BYTES
        );
    }

    // Bound the read itself (not only the pre-check): a concurrent writer could
    // grow the file after fstat; `take` ensures we never buffer more than cap+1.
    let mut contents = String::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_string(&mut contents)
        .map_err(|e| anyhow::anyhow!("failed to read config_path '{}': {e}", config_path))?;
    if (contents.len() as u64) > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "config_path exceeds maximum size of {} bytes",
            MAX_CONFIG_BYTES
        );
    }

    Russelfile::load_from_str(&contents)
}

/// Open `path` as a directory (symlinks on the final component may be followed;
/// containment is enforced by subsequent `openat` + `O_NOFOLLOW` steps).
#[cfg(unix)]
fn open_directory_fd(path: &Path) -> std::io::Result<OwnedFd> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    opts.custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC);
    let file = opts.open(path)?;
    Ok(OwnedFd::from(file))
}

/// `openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`.
#[cfg(unix)]
fn openat_directory_nofollow(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
) -> std::io::Result<OwnedFd> {
    let c_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openat(parent, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` — regular file open.
#[cfg(unix)]
fn openat_file_nofollow(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
) -> std::io::Result<File> {
    let c_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
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

    const MINIMAL: &str = r#"[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;

    #[test]
    fn load_russelfile_accepts_relative_file() {
        let repo = TempRepo::new();
        write_config(repo.path(), "Russelfile.toml", MINIMAL);
        let cfg = load_russelfile_under_repo(repo.path(), "Russelfile.toml").unwrap();
        assert_eq!(cfg.service.name, "app");
        assert_eq!(cfg.service.port, 3000);
    }

    #[test]
    fn load_russelfile_accepts_nested_relative() {
        let repo = TempRepo::new();
        write_config(repo.path(), "deploy/Russelfile.toml", MINIMAL);
        let cfg = load_russelfile_under_repo(repo.path(), "deploy/Russelfile.toml").unwrap();
        assert_eq!(cfg.service.name, "app");
    }

    #[test]
    fn load_russelfile_rejects_absolute() {
        let repo = TempRepo::new();
        let err = load_russelfile_under_repo(repo.path(), "/etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("relative"), "{err}");
    }

    #[test]
    fn load_russelfile_rejects_parent_dir() {
        let repo = TempRepo::new();
        let err = load_russelfile_under_repo(repo.path(), "../outside.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn load_russelfile_rejects_missing() {
        let repo = TempRepo::new();
        let err = load_russelfile_under_repo(repo.path(), "missing.toml")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not found")
                || err.contains("cannot open")
                || err.contains("No such file"),
            "{err}"
        );
    }

    #[test]
    fn load_russelfile_rejects_directory() {
        let repo = TempRepo::new();
        std::fs::create_dir(repo.path().join("subdir")).unwrap();
        let err = load_russelfile_under_repo(repo.path(), "subdir")
            .unwrap_err()
            .to_string();
        // Directory has no file name open as file → open or "regular file" error
        assert!(
            err.contains("regular file")
                || err.contains("cannot open")
                || err.contains("Is a directory"),
            "{err}"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn load_russelfile_rejects_symlink_leaf() {
        let repo = TempRepo::new();
        let outside = std::env::temp_dir().join(format!(
            "russel-config-path-outside-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.toml");
        std::fs::write(
            &outside_file,
            b"[service]\nname=\"x\"\nsource=\".\"\nport=1\nmemory=\"1mb\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside_file, repo.path().join("escape.toml")).unwrap();
        // O_NOFOLLOW rejects the leaf symlink (does not follow out of the repo).
        let err = load_russelfile_under_repo(repo.path(), "escape.toml")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot open")
                || err.contains("symbolic link")
                || err.contains("Too many levels")
                || err.contains("os error"),
            "{err}"
        );
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn load_russelfile_rejects_symlink_intermediate_dir() {
        let repo = TempRepo::new();
        let outside = std::env::temp_dir().join(format!(
            "russel-config-path-outside-dir-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.toml"), MINIMAL.as_bytes()).unwrap();
        // Intermediate directory component is a symlink → openat O_NOFOLLOW must fail.
        std::os::unix::fs::symlink(&outside, repo.path().join("linkdir")).unwrap();
        let err = load_russelfile_under_repo(repo.path(), "linkdir/secret.toml")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not found")
                || err.contains("cannot open")
                || err.contains("symbolic link")
                || err.contains("Too many levels")
                || err.contains("os error"),
            "{err}"
        );
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn load_russelfile_rejects_oversized() {
        let repo = TempRepo::new();
        let path = repo.path().join("huge.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"[service]\n").unwrap();
            let pad = vec![b'#'; MAX_CONFIG_BYTES as usize + 1];
            f.write_all(&pad).unwrap();
        }
        let err = load_russelfile_under_repo(repo.path(), "huge.toml")
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

enum DeployWorkload {
    Microvm {
        alloc: SubnetAllocation,
        vm_child: Box<tokio::process::Child>,
        virtiofsd_children: Vec<tokio::process::Child>,
        socat_child: Box<tokio::process::Child>,
        initramfs_path: PathBuf,
        port: PortMapping,
    },
    Container {
        container_id: String,
        container_name: String,
        rootfs_path: PathBuf,
        port: PortMapping,
    },
}

impl DeployWorkload {
    fn port(&self) -> &PortMapping {
        match self {
            Self::Microvm { port, .. } | Self::Container { port, .. } => port,
        }
    }

    async fn teardown_network(&self) {
        if let Self::Microvm { alloc, .. } = self
            && let Err(teardown_error) = TapForwarder::teardown(alloc).await
        {
            tracing::warn!(error = %teardown_error, "failed to tear down TAP after Traefik registration failure");
        }
    }
}

struct DeployOutput {
    store_path: PathBuf,
    port: PortMapping,
    runtime: RuntimeKind,
    timing: DeployTiming,
    workload: DeployWorkload,
}

#[cfg(test)]
mod deploy_tests {
    use crate::container::validate_podman_args_for_runtime;
    use russel_core::config::RuntimeKind;

    #[test]
    fn podman_args_rejected_for_microvm_runtime() {
        let err =
            validate_podman_args_for_runtime(RuntimeKind::Microvm, &["-v".into(), "/a:/b".into()])
                .unwrap_err();
        assert!(err.to_string().contains("microvm"));
    }

    #[test]
    fn podman_args_allowed_for_container_runtime() {
        validate_podman_args_for_runtime(
            RuntimeKind::Container,
            &["--network".into(), "bridge".into()],
        )
        .unwrap();
    }
}
