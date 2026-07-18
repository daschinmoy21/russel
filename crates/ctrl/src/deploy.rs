use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::process::Command;

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployResponse, DeployTiming, PortMapping},
    config::{RuntimeKind, Russelfile, resolve_runtime},
};

use crate::{
    build::NixBuilder,
    container::{
        ContainerRunner, ContainerStartSpec, PreparedRootfs, RootfsSpec,
        default_base_dir, validate_podman_args_for_runtime,
    },
    database::DatabaseProvisioner,
    git::GitClient,
    metadata::{build_container_metadata, write_metadata},
    microvm::{self, BootOutput, KernelInfo, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    traefik::TraefikClient,
    warm_pool::shared_warm_pool,
};

pub use crate::metadata::prior_runtime_from_disk;

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
                        rootfs_path,
                        ..
                    } => {
                        self.state
                            .mark_deployed_container(&service_id, &container_id);
                        (
                            format!(
                                "container running. localhost:{host_port} -> guest:{guest_port}"
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
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
                let error_msg = error.to_string();

                // Successful rollback encodes prior runtime so the response
                // reports what is actually running (not the failed request).
                // Format: ROLLBACK_SUCCESS:<runtime>:<original error>
                if let Some(rest) = error_msg.strip_prefix("ROLLBACK_SUCCESS:") {
                    let (restored_runtime, original_error) = match rest.split_once(':') {
                        Some((rt, msg)) => (
                            rt.parse::<RuntimeKind>().ok().or(request_runtime),
                            msg.trim(),
                        ),
                        None => (request_runtime, rest.trim()),
                    };
                    tracing::info!(
                        service_id = %service_id,
                        elapsed_ms = elapsed,
                        runtime = ?restored_runtime,
                        "deploy failed but successfully rolled back"
                    );
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
                        runtime: restored_runtime,
                        message: format!(
                            "deployment failed but rolled back successfully: {original_error}"
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
        let config_path = resolve_config_path(&repo_path, &request.config_path)?;
        let config = Russelfile::load(&config_path)?;
        let runtime = resolve_runtime(config.service.runtime, request.runtime)?;
        validate_podman_args_for_runtime(runtime, &request.podman_args)?;
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

        let prior_runtime = prior_runtime_from_disk(service_id);

        let russel_dir = format!("/var/lib/russel/{}", service_id);
        let microvms_dir = format!("/var/lib/microvms/{}", service_id);
        let russel_bak = format!("{}.bak", russel_dir);
        let microvms_bak = format!("{}.bak", microvms_dir);
        let has_russel_backup = Path::new(&russel_dir).exists();
        let has_microvms_backup = Path::new(&microvms_dir).exists();
        let has_backup = has_russel_backup;
        if has_russel_backup {
            if let Err(e) = tokio::fs::rename(&russel_dir, &russel_bak).await {
                anyhow::bail!("failed to backup russel directory: {}", e);
            }
            if has_microvms_backup
                && let Err(e) = tokio::fs::rename(&microvms_dir, &microvms_bak).await
            {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                anyhow::bail!("failed to backup microvms directory: {}", e);
            }
        }

        let (old_vm_proc, old_aux_procs) = self
            .state
            .take_processes(service_id)
            .unwrap_or((None, Vec::new()));

        if let Err(e) =
            destroy_prior_runtime(prior_runtime, service_id, &self.runner, &self.containers).await
        {
            self.state
                .restore_processes(service_id, old_vm_proc, old_aux_procs);
            if has_backup {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                if has_microvms_backup {
                    let _ = tokio::fs::rename(&microvms_bak, &microvms_dir).await;
                }
            }
            anyhow::bail!("failed to teardown prior {}: {}", prior_runtime, e);
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
                tracing::error!(service_id, error = %deploy_err, "Deployment failed — cleaning up and attempting rollback (prior runtime was {prior_runtime})");
                cleanup_failed_deploy(runtime, service_id, &self.runner, &self.containers).await;
                if has_backup && prior_runtime == RuntimeKind::Microvm {
                    let rollback_res = attempt_microvm_rollback(
                        service_id,
                        &russel_dir,
                        &microvms_dir,
                        &russel_bak,
                        &microvms_bak,
                        has_microvms_backup,
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
                            return Err(anyhow::anyhow!(
                                "ROLLBACK_SUCCESS:{prior_runtime}:{deploy_err}"
                            ));
                        }
                        Err(rollback_err) => {
                            tracing::error!(service_id, error = %rollback_err, "CRITICAL: Rollback failed. Old VM could not be restored.");
                        }
                    }
                } else if has_backup && prior_runtime == RuntimeKind::Container {
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
                            return Err(anyhow::anyhow!(
                                "ROLLBACK_SUCCESS:{prior_runtime}:{deploy_err}"
                            ));
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
                        has_microvms_backup,
                    )
                    .await;
                }
                return Err(deploy_err);
            }
        };

        if let Err(error) = self.traefik.register(service_id, workload.port().host).await {
            workload.teardown_network().await;
            PortAllocator::release(service_id);
            return Err(error);
        }

        if has_backup {
            let _ = Command::new("rm")
                .args(["-rf", &format!("{}.bak", russel_dir)])
                .output()
                .await;
            if has_microvms_backup {
                let _ = Command::new("rm")
                    .args(["-rf", &format!("{}.bak", microvms_dir)])
                    .output()
                    .await;
            }
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
        })
    }

    async fn deploy_microvm(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        port: &PortMapping,
        kernel_info: &KernelInfo,
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
        std::fs::create_dir_all(&cfg_dir).map_err(|e| {
            anyhow::anyhow!("failed to create config dir {}: {}", cfg_dir, e)
        })?;
        let deploy_env = format!(
            "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
            alloc.vm_ip, alloc.host_ip, port.guest, app_path
        );
        std::fs::write(format!("{cfg_dir}/deploy.env"), deploy_env)?;

        // Agent initramfs is cached after first use — no app baked in.
        let initramfs_path = self.runner.build_agent_initramfs().await?;

        let create_ms = t.elapsed().as_millis();
        tracing::info!(service_id, create_ms, "deploy.env written, agent initramfs ready");

        // ── TAP + socat + boot/restore VM ──────────────────────────────
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "start".into(),
                description: "Setting up network + booting/restoring VM".into(),
            })
            .await;
        tracing::info!(service_id, "setting up TAP + socat + booting VM");

        let t_net = Instant::now();
        let socat_child =
            TapForwarder::setup(service_id, &alloc, port.host, port.guest).await?;
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

        // ── Write metadata (JSON directly for virtiofsd_pids array) ────
        let metadata_path = format!("/var/lib/russel/{}/metadata.json", service_id);
        let virtiofsd_pids: Vec<serde_json::Value> = virtiofsd_children
            .iter()
            .filter_map(|c| c.id().map(|id| serde_json::Value::Number(id.into())))
            .collect();
        let metadata = serde_json::json!({
            "service_id": service_id,
            "runtime": "microvm",
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
            "initramfs": initramfs_path.to_string_lossy(),
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
            TapForwarder::wait_for_vm_port(&alloc.vm_ip, port.guest, Duration::from_secs(10))
                .await;
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
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        tracing::info!(
            service_id,
            host = port.host,
            guest = port.guest,
            "allocated container port"
        );

        let bin_name = config.service.bin_name().to_string();
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
            env: vec![("PORT".to_string(), port.guest.to_string())],
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
        let up =
            TapForwarder::wait_for_host_port(port.host, Duration::from_secs(10)).await;
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
    tokio::fs::rename(russel_bak, russel_dir).await?;
    if has_microvms_backup {
        tokio::fs::rename(microvms_bak, microvms_dir).await?;
    }
    let old_metadata_path = format!("{}/metadata.json", russel_dir);
    let content = std::fs::read_to_string(&old_metadata_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;
    let old_host_port = old_meta["host_port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing host_port"))? as u16;
    let old_guest_port = old_meta["guest_port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing guest_port"))? as u16;
    let old_mem_mb = old_meta["mem_mb"].as_u64().unwrap_or(512) as u16;
    let old_kernel_path = PathBuf::from(
        old_meta["kernel_path"]
            .as_str()
            .unwrap_or("/nix/store/kernel"),
    );
    let old_initramfs_path = PathBuf::from(format!("{}/initramfs.cpio", russel_dir));
    let old_alloc = subnet_for(service_id);

    PortAllocator::reserve(service_id, old_host_port)?;
    let old_socat =
        TapForwarder::setup(service_id, &old_alloc, old_host_port, old_guest_port).await?;
    let old_boot = runner
        .boot(
            service_id,
            &old_kernel_path,
            &old_initramfs_path,
            &old_alloc,
            old_mem_mb,
        )
        .await?;

    let old_vm_pid = old_boot.vm_child.id();
    let old_virtiofsd_pid = old_boot.virtiofsd_children.first().and_then(|c| c.id());
    let old_socat_pid = old_socat.id();

    let mut aux = vec![old_socat];
    aux.extend(old_boot.virtiofsd_children);
    state.mark_deployed_with_aux(service_id, old_boot.vm_child, aux);

    let vm_ip = old_meta["vm_ip"].as_str().unwrap_or("10.0.0.2");
    let store_path = old_meta["store_path"]
        .as_str()
        .unwrap_or("/nix/store/unknown");
    // Write metadata as JSON directly (same format as deploy_microvm).
    let virtiofsd_pids: Vec<serde_json::Value> = [old_virtiofsd_pid]
        .iter()
        .filter_map(|&pid| pid.map(|p| serde_json::Value::Number(p.into())))
        .collect();
    let new_metadata = serde_json::json!({
        "service_id": service_id,
        "runtime": "microvm",
        "host_port": old_host_port,
        "guest_port": old_guest_port,
        "vm_ip": vm_ip,
        "host_ip": old_alloc.host_ip,
        "vm_pid": old_vm_pid,
        "virtiofsd_pids": virtiofsd_pids,
        "socat_pid": old_socat_pid,
        "kernel_path": old_kernel_path.to_string_lossy(),
        "mem_mb": old_mem_mb,
        "app_path": store_path,
        "initramfs": old_initramfs_path.to_string_lossy(),
    });
    let content = serde_json::to_string_pretty(&new_metadata)
        .map_err(|e| anyhow::anyhow!("failed to serialize metadata: {}", e))?;
    std::fs::write(&old_metadata_path, content)?;
    Ok(())
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
        .ok_or_else(|| anyhow::anyhow!("missing host_port in container metadata"))? as u16;
    let old_guest_port = old_meta["guest_port"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing guest_port in container metadata"))? as u16;
    let old_mem_mb = old_meta["mem_mb"].as_u64().unwrap_or(512) as u16;
    let old_bin_name = old_meta["bin_name"].as_str().unwrap_or("app");
    let old_rootfs_path = old_meta["rootfs_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing rootfs_path in container metadata"))?;
    let old_podman_args: Vec<String> = old_meta
        .get("podman_args")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|a| a.as_str().map(str::to_string)).collect())
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
    let ready =
        TapForwarder::wait_for_host_port(old_host_port, Duration::from_secs(10)).await;
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
        let err = validate_podman_args_for_runtime(
            RuntimeKind::Microvm,
            &["-v".into(), "/a:/b".into()],
        )
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
