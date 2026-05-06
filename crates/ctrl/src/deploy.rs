use std::{path::PathBuf, time::{Duration, Instant}};

use russel_core::{
    api::{DeployRequest, DeployResponse, DeployEvent, DeployTiming, PortMapping},
    config::Russelfile,
};

use crate::{
    build::NixBuilder,
    database::DatabaseProvisioner,
    git::GitClient,
    microvm::MicrovmRunner,
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    traefik::TraefikClient,
};

#[derive(Debug)]
pub struct DeployPipeline {
    state: AppState,
    git: GitClient,
    builder: NixBuilder,
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
            runner: MicrovmRunner,
            ports: PortAllocator::default(),
            traefik: TraefikClient::default(),
        }
    }

    pub async fn deploy(&self, request: DeployRequest, tx: tokio::sync::mpsc::Sender<DeployEvent>) -> DeployResponse {
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
                tracing::info!(service_id = %service_id, elapsed_ms = elapsed,
                    host_port, guest_port, "deploy succeeded");
                self.state.mark_deployed(&service_id, output.child);
                self.state.store_aux_process(output.socat_child);
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "deployed".to_string(),
                    store_path: Some(output.store_path.display().to_string()),
                    microvm_config_path: Some(
                        output.deploy_dir.join("flake.nix").display().to_string()
                    ),
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
                tracing::error!(service_id = %service_id, elapsed_ms = elapsed,
                    error = %error, "deploy failed");
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
        // ── 1. Resolve repo ──────────────────────────────────────────────────
        let t = Instant::now();
        let _ = tx.send(DeployEvent::Progress { phase: "resolve".into(), description: "Resolving source & Russelfile".into() }).await;
        let repo_path = self.git.clone_or_use_local(&request.repo_url).await?;
        let config_path = repo_path.join(PathBuf::from(&request.config_path));
        let config = Russelfile::load(&config_path)?;
        let resolve_ms = t.elapsed().as_millis();
        tracing::info!(service_id, service_name = %config.service.name, resolve_ms, "repo resolved");

        if let Some(database) = &config.database {
            self.database.ensure(database).await?;
        }
        if !repo_path.join("flake.nix").exists() {
            anyhow::bail!("no flake.nix in '{}' — required for packages.default", repo_path.display());
        }

        // ── 2. Nix build (fail-fast) ─────────────────────────────────────────
        let t = Instant::now();
        let _ = tx.send(DeployEvent::Progress { phase: "build".into(), description: "Building Nix package".into() }).await;
        let build = self.builder.build(&repo_path).await?;
        let build_ms = t.elapsed().as_millis();
        tracing::info!(service_id, store = %build.store_path.display(), build_ms, "nix build complete");

        // ── 3. Allocate port + subnet ────────────────────────────────────────
        let port = request.port.unwrap_or_else(|| PortMapping {
            host: self.ports.next(),
            guest: config.service.port,
        });
        let alloc: SubnetAllocation = subnet_for(service_id);
        tracing::info!(service_id, host = port.host, guest = port.guest, vm_ip = %alloc.vm_ip, "allocated");

        let t = Instant::now();
        let _ = tx.send(DeployEvent::Progress { phase: "create".into(), description: "Generating deploy.nix + registering microVM".into() }).await;
        tracing::info!(service_id, "generating deploy.nix and registering microvm");
        self.runner.create(service_id, &repo_path, &config, &alloc, port.guest, &build.store_path).await?;
        let create_ms = t.elapsed().as_millis();
        tracing::info!(service_id, create_ms, "microvm created and registered");

        // ── 5. Start VM + setup tap concurrently ─────────────────────────────
        //    `systemctl start` launches cloud-hypervisor which creates the tap
        //    interface. We can start waiting for the tap immediately in parallel
        //    rather than sequentially.
        let t = Instant::now();
        let _ = tx.send(DeployEvent::Progress { phase: "start".into(), description: "Starting VM + configuring network (parallel)".into() }).await;
        tracing::info!(service_id, "starting VM and setting up network concurrently");

        let alloc_clone = alloc.clone();
        let h_port = port.host;
        let g_port = port.guest;

        // Spawn start + network in parallel.
        let (start_result, net_result) = tokio::join!(
            self.runner.start(service_id),
            TapForwarder::setup(&alloc_clone, h_port, g_port)
        );

        let started = start_result?;
        let socat_child = net_result?;
        let start_ms = t.elapsed().as_millis();
        // network_ms is included in start_ms since they ran concurrently
        let network_ms = 0u128; // reported as 0 because it overlapped with start

        tracing::info!(service_id, start_ms, "VM started + network configured");

        // ── 6. Wait for VM service to be reachable ───────────────────────────
        let t = Instant::now();
        let _ = tx.send(DeployEvent::Progress { phase: "ready".into(), description: "Waiting for VM service to be reachable".into() }).await;
        tracing::info!(service_id, vm_ip = %alloc.vm_ip, guest_port = port.guest, "polling VM readiness");
        let up = TapForwarder::wait_for_vm_port(&alloc.vm_ip, port.guest, Duration::from_secs(30)).await;
        let ready_ms = t.elapsed().as_millis();
        if up {
            tracing::info!(service_id, ready_ms, "VM service reachable");
        } else {
            tracing::warn!(service_id, "VM not reachable in 30s — continuing");
        }

        self.traefik.register(service_id, port.host).await?;
        self.state.attach_flake_path(service_id, repo_path.clone());

        let deploy_dir = PathBuf::from(format!("/var/lib/russel/{}", service_id));
        Ok(DeployOutput {
            store_path: build.store_path,
            deploy_dir,
            alloc,
            child: started.child,
            socat_child,
            port,
            timing: DeployTiming { resolve_ms, build_ms, create_ms, start_ms, network_ms, ready_ms },
        })
    }
}

#[derive(Debug)]
struct DeployOutput {
    store_path: PathBuf,
    deploy_dir: PathBuf,
    alloc: SubnetAllocation,
    child: tokio::process::Child,
    socat_child: tokio::process::Child,
    port: PortMapping,
    timing: DeployTiming,
}