use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

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

    async fn deploy_inner(
        &self,
        service_id: &str,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<DeployOutput> {
        // Ensure clean state before spawning the new instance
        let _ = self.runner.destroy(service_id).await;
        let (vm_proc, aux_procs) = self.state.clear_processes_if_matches(service_id, "building", "pending");
        if let Some(mut p) = vm_proc {
            let _ = p.kill().await;
        }
        for mut p in aux_procs {
            let _ = p.kill().await;
        }

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

        // ── 3. Allocate port + subnet ──────────────────────────────────────
        let port = request.port.unwrap_or_else(|| PortMapping {
            host: self.ports.next(),
            guest: config.service.port,
        });
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
        let socat_child = TapForwarder::setup(&alloc, port.host, port.guest).await?;

        // Step B: boot cloud-hypervisor with virtiofsd + minimal initramfs
        let BootOutput { vm_child, virtiofsd_child } = self
            .runner
            .boot(service_id, &kernel, &initramfs_path, &alloc, mem_mb)
            .await?;

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
        if up {
            tracing::info!(service_id, ready_ms, "VM service reachable");
        } else {
            tracing::warn!(service_id, "VM not reachable in 10s — continuing");
        }

        self.traefik.register(service_id, port.host).await?;
        self.state.attach_flake_path(service_id, repo_path.clone());

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
