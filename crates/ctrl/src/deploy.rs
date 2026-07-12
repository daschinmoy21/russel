use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::process::Command;

use russel_core::{
    api::{DeployRequest, DeployResponse, DeployEvent, DeployTiming, PortMapping},
    config::Russelfile,
};

use crate::{
    build::NixBuilder,
    database::DatabaseProvisioner,
    git::GitClient,
    microvm::{BootOutput, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    traefik::TraefikClient,
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
            git: GitClient::default(),
            builder: NixBuilder,
            database: DatabaseProvisioner,
            runner: MicrovmRunner::new(),
            ports: PortAllocator::default(),
            traefik: TraefikClient::default(),
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
        self.state.mark_building(&service_id);

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
                self.state.mark_deployed(&service_id, output.vm_child);
                self.state.store_aux_process(output.socat_child);
                self.state.store_aux_process(output.virtiofsd_child);
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
                    // Do NOT mark as failed - the old VM was restored and is running
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
                        message: format!("deployment failed but rolled back successfully: {}", original_error),
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
        let resolve_ms = t.elapsed().as_millis();
        tracing::info!(service_id, service_name = %config.service.name, resolve_ms, "repo resolved");



        // ── 2. Nix build app + ensure kernel + busybox + modules ──────────
        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "build".into(),
                description: "Building Nix package + ensuring kernel/busybox/modules".into(),
            })
            .await;
        let build = self.builder.build(&repo_path).await?;
        let kernel = self.runner.ensure_kernel().await?;
        let busybox = self.runner.ensure_busybox().await?;
        let kernel_modules = self.runner.ensure_kernel_modules().await?;
        let build_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            store = %build.store_path.display(),
            kernel = %kernel.display(),
            modules = %kernel_modules.display(),
            build_ms,
            "build complete (kernel + busybox + modules cached)"
        );

        // Validate service_id before any filesystem operations
        MicrovmRunner::validate_service_id(service_id)?;

        // Now that the build has succeeded, we can prepare the backup and rollback path
        let russel_dir = format!("/var/lib/russel/{}", service_id);
        let microvms_dir = format!("/var/lib/microvms/{}", service_id);
        let russel_bak = format!("{}.bak", russel_dir);
        let microvms_bak = format!("{}.bak", microvms_dir);
        let has_backup = std::path::Path::new(&russel_dir).exists();
        if has_backup {
            // Perform transactional backup: propagate errors and restore on failure
            if let Err(e) = tokio::fs::rename(&russel_dir, &russel_bak).await {
                anyhow::bail!("failed to backup russel directory: {}", e);
            }
            if let Err(e) = tokio::fs::rename(&microvms_dir, &microvms_bak).await {
                // Restore the first rename on failure
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                anyhow::bail!("failed to backup microvms directory: {}", e);
            }
        }

        // Take the old processes from state
        let (old_vm_proc, old_aux_procs) = self.state.take_processes_if_matches(service_id);

        // Teardown the old VM (it will stop systemd service, delete old TAP, release ports)
        // Since the directories are renamed to .bak, they are not deleted.
        if let Err(e) = self.runner.destroy(service_id).await {
            // Restore processes first so the lifecycle can be re-attempted
            self.state.restore_processes(service_id, old_vm_proc, old_aux_procs);
            // Restore backups on teardown failure
            if has_backup {
                let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                let _ = tokio::fs::rename(&microvms_bak, &microvms_dir).await;
            }
            anyhow::bail!("failed to teardown old VM: {}", e);
        }

        let mut port_reservation = None;
        let deploy_result = async {
            // Keep the registry reservation alive until the deployment is fully
            // committed. Any error in this block releases it via Drop.
            port_reservation = Some(PortReservation::new(service_id));
            let port = match request.port.clone() {
                Some(p) => {
                    PortAllocator::reserve(service_id, p.host)?;
                    p
                }
                None => {
                    let ports = self.ports.clone();
                    let service_id = service_id.to_string();
                    let host = tokio::task::spawn_blocking(move || ports.next(&service_id)).await??;
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

            // ── 4. Create initramfs (minimal rootfs with app + init) ───────────
            let t = Instant::now();
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "create".into(),
                    description: "Building minimal initramfs".into(),
                })
                .await;
            let bin_name = config.service.bin_name().to_string();
            let mem_mb = config.service.memory.as_mebibytes();

            let initramfs_path = self
                .runner
                .build_initramfs(
                    service_id,
                    &alloc,
                    port.guest,
                    &build.store_path,
                    &bin_name,
                    &busybox,
                    &kernel_modules,
                )
                .await?;
            let create_ms = t.elapsed().as_millis();
            tracing::info!(service_id, create_ms, "initramfs ready");

            // ── 5. TAP + socat + boot VM (serial, as user requested) ──────────
            let t = Instant::now();
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "start".into(),
                    description: "Setting up network + booting VM".into(),
                })
                .await;
            tracing::info!(service_id, "setting up TAP + socat + booting VM (serial)");

            // Step A: create TAP + setup port forwarding (socat)
            let socat_child = TapForwarder::setup(service_id, &alloc, port.host, port.guest).await?;

            // Step B: boot cloud-hypervisor with virtiofsd + minimal initramfs
            let BootOutput { vm_child, virtiofsd_child } = self
                .runner
                .boot(service_id, &kernel, &initramfs_path, &alloc, mem_mb)
                .await?;

            // Write metadata.json for precise and secure cleanup
            // This must succeed for deployment to be considered successful
            let metadata_path = format!("/var/lib/russel/{}/metadata.json", service_id);
            let metadata = serde_json::json!({
                "service_id": service_id,
                "host_port": port.host,
                "guest_port": port.guest,
                "vm_ip": alloc.vm_ip,
                "vm_pid": vm_child.id(),
                "virtiofsd_pid": virtiofsd_child.id(),
                "socat_pid": socat_child.id(),
                "kernel_path": kernel.to_string_lossy(),
                "mem_mb": mem_mb,
            });
            let content = serde_json::to_string_pretty(&metadata)
                .map_err(|e| anyhow::anyhow!("failed to serialize metadata: {}", e))?;
            std::fs::write(&metadata_path, content)
                .map_err(|e| anyhow::anyhow!("failed to write metadata to {}: {}", metadata_path, e))?;

            let start_ms = t.elapsed().as_millis();
            let network_ms = 0u128; // included in start_ms (serial)
            tracing::info!(service_id, start_ms, "VM booted + network configured");

            // ── 6. Wait for VM service to be reachable ───────────────────────────
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
            let up = TapForwarder::wait_for_vm_port(
                &alloc.vm_ip,
                port.guest,
                Duration::from_secs(10),
            )
            .await;
            let ready_ms = t.elapsed().as_millis();
            if !up {
                anyhow::bail!("VM not reachable in 10s");
            }
            tracing::info!(service_id, ready_ms, "VM service reachable");

            Ok::<_, anyhow::Error>((port, alloc, vm_child, virtiofsd_child, socat_child, initramfs_path, create_ms, start_ms, network_ms, ready_ms))
        }.await;

        let (port, alloc, vm_child, virtiofsd_child, socat_child, initramfs_path, create_ms, start_ms, network_ms, ready_ms) = match deploy_result {
            Ok(val) => {
                // Do NOT delete backups or kill old processes yet - wait until after Traefik registration
                val
            }
            Err(deploy_err) => {
                tracing::error!(service_id, error = %deploy_err, "Deployment failed — cleaning up and attempting rollback");
                // Always tear down any partially-created VM from the failed deployment
                let _ = self.runner.destroy(service_id).await;
                if has_backup {
                    let rollback_res = async {
                        tokio::fs::rename(&russel_bak, &russel_dir).await?;
                        tokio::fs::rename(&microvms_bak, &microvms_dir).await?;
                        let old_metadata_path = format!("{}/metadata.json", russel_dir);
                        let content = std::fs::read_to_string(&old_metadata_path)?;
                        let old_meta: serde_json::Value = serde_json::from_str(&content)?;
                        let old_host_port = old_meta["host_port"].as_u64().ok_or_else(|| anyhow::anyhow!("missing host_port"))? as u16;
                        let old_guest_port = old_meta["guest_port"].as_u64().ok_or_else(|| anyhow::anyhow!("missing guest_port"))? as u16;
                        let old_mem_mb = old_meta["mem_mb"].as_u64().unwrap_or(512) as u16;
                        let old_kernel_path = PathBuf::from(old_meta["kernel_path"].as_str().unwrap_or("/nix/store/kernel"));
                        let old_initramfs_path = PathBuf::from(format!("{}/initramfs.cpio", russel_dir));
                        let old_alloc = subnet_for(service_id);

                        PortAllocator::reserve(service_id, old_host_port)?;
                        let old_socat = TapForwarder::setup(service_id, &old_alloc, old_host_port, old_guest_port).await?;
                        let old_boot = self.runner.boot(service_id, &old_kernel_path, &old_initramfs_path, &old_alloc, old_mem_mb).await?;

                        let old_vm_pid = old_boot.vm_child.id();
                        let old_virtiofsd_pid = old_boot.virtiofsd_child.id();
                        let old_socat_pid = old_socat.id();

                        self.state.mark_deployed(service_id, old_boot.vm_child);
                        self.state.store_aux_process(old_socat);
                        self.state.store_aux_process(old_boot.virtiofsd_child);

                        let new_metadata = serde_json::json!({
                            "service_id": service_id,
                            "host_port": old_host_port,
                            "guest_port": old_guest_port,
                            "vm_ip": old_meta["vm_ip"],
                            "vm_pid": old_vm_pid,
                            "virtiofsd_pid": old_virtiofsd_pid,
                            "socat_pid": old_socat_pid,
                            "kernel_path": old_kernel_path.to_string_lossy(),
                            "mem_mb": old_mem_mb,
                        });
                        if let Ok(c) = serde_json::to_string_pretty(&new_metadata) {
                            let _ = std::fs::write(old_metadata_path, c);
                        }
                        Ok::<(), anyhow::Error>(())
                    }.await;

                    match rollback_res {
                        Ok(()) => {
                            tracing::info!(service_id, "Rollback to previous VM succeeded");
                            // The old VM is active again, so keep its port reservation.
                            port_reservation.as_mut().expect("port reservation exists").disarm();
                            // Return an error that indicates rollback succeeded (caller should NOT mark as failed)
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

        // Register with Traefik before committing the deployment. Release the
        // reservation if this final fallible step fails.
        if let Err(error) = self.traefik.register(service_id, port.host).await {
            if let Err(teardown_error) = TapForwarder::teardown(&alloc).await {
                tracing::warn!(service_id, error = %teardown_error, "failed to tear down TAP after Traefik registration failure");
            }
            PortAllocator::release(service_id);
            return Err(error);
        }

        // Only after successful Traefik registration, clean up old resources
        if has_backup {
            let _ = Command::new("rm").args(["-rf", &format!("{}.bak", russel_dir)]).output().await;
            let _ = Command::new("rm").args(["-rf", &format!("{}.bak", microvms_dir)]).output().await;
        }
        if let Some(mut p) = old_vm_proc {
            let _ = p.kill().await;
        }
        for mut p in old_aux_procs {
            let _ = p.kill().await;
        }

        self.state.attach_flake_path(service_id, repo_path.clone());
        port_reservation.as_mut().expect("port reservation exists").disarm();

        Ok(DeployOutput {
            store_path: build.store_path,
            initramfs_path,
            alloc,
            vm_child,
            virtiofsd_child,
            socat_child,
            port,
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

struct PortReservation {
    service_id: String,
    armed: bool,
}

impl PortReservation {
    fn new(service_id: &str) -> Self {
        Self { service_id: service_id.to_string(), armed: true }
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
    virtiofsd_child: tokio::process::Child,
    socat_child: tokio::process::Child,
    port: PortMapping,
    timing: DeployTiming,
}
