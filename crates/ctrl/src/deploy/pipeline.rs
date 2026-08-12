//! DeployPipeline: request entrypoint, dual-live orchestration, and workload types.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::process::Command;

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployResponse, DeployTiming, PortMapping},
    config::{RuntimeKind, merge_env_maps, resolve_runtime, validate_env_map},
};

use crate::{
    build::{self, BuildBackend},
    container::{ContainerRunner, validate_podman_args_for_runtime},
    deployments::{self, AppendSuccess, DesiredStateSnapshot},
    git::GitClient,
    ingress::{self, Backend, Ingress},
    metadata::{rewrite_metadata_service_id, write_metadata},
    microvm::{self, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder},
    state::AppState,
};

use super::config::load_russelfile_under_repo;
use super::rollback::{
    attempt_container_rollback, attempt_microvm_rollback, cleanup_failed_deploy,
    destroy_prior_runtime, kill_and_wait_children, resolve_prior_runtime, restore_backup_dirs,
};

/// Short random generation id (8 lowercase hex chars).
pub(crate) fn new_generation_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Fold pid into the low 32 bits we keep: rapid same-ns deploys from
    // different processes stay distinct; output is always 8 lowercase hex digits.
    let mixed = (nanos as u32) ^ std::process::id();
    format!("{mixed:08x}")
}

/// Promote a candidate generation directory tree to the stable service id.
///
/// Called only after the old generation has been destroyed, so `service_id`
/// paths are free. Rewrites `service_id` inside metadata.json.
pub(crate) async fn promote_generation(runtime_key: &str, service_id: &str) -> anyhow::Result<()> {
    if runtime_key == service_id {
        return Ok(());
    }
    let pairs = [
        (
            format!("/var/lib/russel/{runtime_key}"),
            format!("/var/lib/russel/{service_id}"),
        ),
        (
            format!("/var/lib/microvms/{runtime_key}"),
            format!("/var/lib/microvms/{service_id}"),
        ),
    ];
    for (from, to) in pairs {
        if !Path::new(&from).exists() {
            continue;
        }
        if Path::new(&to).exists() {
            anyhow::bail!("promote target already exists: {to}");
        }
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| anyhow::anyhow!("rename {from} -> {to}: {e}"))?;
    }
    let meta_path = format!("/var/lib/russel/{service_id}/metadata.json");
    if Path::new(&meta_path).exists() {
        rewrite_metadata_service_id(&meta_path, service_id)?;
    }
    tracing::info!(
        runtime_key,
        service_id,
        "promoted candidate generation to stable service id"
    );
    Ok(())
}

/// Persist repo/config so update + health restart can rebuild desired state.
pub(crate) fn record_source_in_metadata(
    service_id: &str,
    repo_url: &str,
    config_path: &str,
) -> anyhow::Result<()> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read metadata for source record: {e}"))?;
    let mut value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("parse metadata for source record: {e}"))?;
    let object = if let Some(object) = value.as_object_mut() {
        object
    } else {
        value = serde_json::json!({});
        value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("failed to create metadata object"))?
    };
    object.insert("repo_url".into(), serde_json::json!(repo_url));
    object.insert("config_path".into(), serde_json::json!(config_path));
    write_metadata(&path, &value)
}

/// Typed outcome of `deploy_inner` — success, rollback, or hard failure.
pub(crate) enum DeployInnerResult {
    // Box large success payload (clippy large_enum_variant).
    Success(Box<DeployOutput>),
    RolledBack { runtime: RuntimeKind, error: String },
}

pub struct DeployPipeline {
    pub(crate) state: AppState,
    pub(crate) git: GitClient,
    pub(crate) builder: Arc<dyn BuildBackend>,
    pub(crate) runner: MicrovmRunner,
    pub(crate) containers: ContainerRunner,
    pub(crate) ports: PortAllocator,
    pub(crate) ingress: Arc<dyn Ingress>,
}

impl std::fmt::Debug for DeployPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeployPipeline")
            .field("state", &self.state)
            .field("git", &self.git)
            .field("builder", &"Arc<dyn BuildBackend>")
            .field("runner", &self.runner)
            .field("containers", &self.containers)
            .field("ports", &self.ports)
            .field("ingress", &"Arc<dyn Ingress>")
            .finish()
    }
}

impl DeployPipeline {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            git: GitClient,
            builder: build::default_builder(),
            // DeployPipeline is constructed per request.  Keep the expensive
            // kernel/busybox/module resolution cache alive across requests so
            // the benchmark's later VMs measure VM work rather than repeated
            // `nix build --no-link` evaluations.
            runner: microvm::shared_runner(),
            containers: ContainerRunner::new(),
            ports: PortAllocator,
            ingress: ingress::default_ingress(),
        }
    }

    /// Replace the build backend (useful for tests).
    #[allow(dead_code)]
    pub fn with_builder(mut self, b: Arc<dyn BuildBackend>) -> Self {
        self.builder = b;
        self
    }

    pub async fn deploy(
        &self,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> DeployResponse {
        let started = Instant::now();
        // #300: require an explicit service id — never share the old "api" default.
        let service_id = match request
            .vm_id
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some(id) => id.to_string(),
            None => {
                tracing::error!(repo = %request.repo_url, "deploy rejected: vm_id required");
                return DeployResponse {
                    service_id: String::new(),
                    vm_id: String::new(),
                    status: "failed".to_string(),
                    store_path: None,
                    microvm_config_path: None,
                    runner_path: None,
                    port: None,
                    elapsed_ms: started.elapsed().as_millis(),
                    timing: None,
                    vm_ip: None,
                    runtime: request.runtime,
                    message: "vm_id is required; shared default \"api\" was removed".to_string(),
                    route_host: None,
                    backend_port: None,
                };
            }
        };
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
                route_host: None,
                backend_port: None,
            };
        }

        // Defense in depth: reject host/guest port 0 even if API skipped validate.
        if let Some(ref p) = request.port
            && let Err(e) = p.validate()
        {
            tracing::error!(service_id = %service_id, error = %e, "deploy rejected: invalid port");
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
                message: e,
                route_host: None,
                backend_port: None,
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
                route_host: None,
                backend_port: None,
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
                        self.state.mark_deployed_with_aux(
                            &service_id,
                            *vm_child,
                            aux,
                            Some(host_port),
                            Some(guest_port),
                        );
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
                        self.state.mark_deployed_container(
                            &service_id,
                            &container_id,
                            Some(host_port),
                            Some(guest_port),
                        );
                        (
                            format!(
                                "container {container_name} running. localhost:{host_port} -> guest:{guest_port}"
                            ),
                            None,
                            Some(rootfs_path.display().to_string()),
                        )
                    }
                };
                let route_host = self.ingress.primary_host(&service_id);
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
                    route_host,
                    backend_port: Some(host_port),
                }
            }
            Ok(DeployInnerResult::RolledBack {
                runtime: restored_runtime,
                error: original_error,
            }) => {
                let elapsed = started.elapsed().as_millis();
                // Do not treat auto-rollback as completing an explicit operator rollback.
                deployments::clear_pending_rollback(&service_id);
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
                    route_host: None,
                    backend_port: None,
                }
            }
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
                deployments::clear_pending_rollback(&service_id);
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
                    route_host: None,
                    backend_port: None,
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
        let (repo_path, checkout_lease) = self.git.clone_or_use_local(&request.repo_url).await?;
        let _checkout_lease = self.git.hold_checkout(&repo_path);
        drop(checkout_lease);
        let config = load_russelfile_under_repo(&repo_path, &request.config_path)?;
        let runtime = resolve_runtime(config.service.runtime, request.runtime)?;
        validate_podman_args_for_runtime(runtime, &request.podman_args)?;

        // Merge env: file < request (request wins on key conflict).
        // Save pre-resolution env for desired_state; resolve secret:// refs for deploy.
        let merged_env_pre_resolve = merge_env_maps(&config.service.env, &request.env);
        validate_env_map(&merged_env_pre_resolve)?;
        let merged_env = crate::secrets::resolve_env_secrets(&merged_env_pre_resolve)?;
        validate_env_map(&merged_env)?;

        // Build desired_state for rollback + health restart + update (F-04/08/09).
        let desired_state = {
            let mut ds = serde_json::Map::new();
            // env PRE secret resolution
            let env_obj: serde_json::Map<String, serde_json::Value> = merged_env_pre_resolve
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            ds.insert("env".into(), serde_json::Value::Object(env_obj));
            if !request.podman_args.is_empty() {
                ds.insert(
                    "podman_args".into(),
                    serde_json::Value::Array(
                        request
                            .podman_args
                            .iter()
                            .map(|a| serde_json::Value::String(a.clone()))
                            .collect(),
                    ),
                );
            }
            ds.insert(
                "repo_url".into(),
                serde_json::Value::String(request.repo_url.clone()),
            );
            ds.insert(
                "config_path".into(),
                serde_json::Value::String(request.config_path.clone()),
            );
            ds.insert(
                "runtime".into(),
                serde_json::Value::String(runtime.to_string()),
            );
            // Fixed port only when user passed -p
            if let Some(ref p) = request.port {
                let mut port_obj = serde_json::Map::new();
                port_obj.insert("host".into(), serde_json::json!(p.host));
                port_obj.insert("guest".into(), serde_json::json!(p.guest));
                ds.insert("port".into(), serde_json::Value::Object(port_obj));
            }
            Some(serde_json::Value::Object(ds))
        };

        let resolve_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            service_name = %config.service.name,
            runtime = %runtime,
            resolve_ms,
            "repo resolved"
        );

        // ── 2. Build (+ kernel stack only for microVM) ───────────────────
        let t = Instant::now();
        let build_description = if runtime == RuntimeKind::Microvm {
            "Building package + ensuring kernel/busybox/modules"
        } else {
            "Building package"
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
            let ki = kernel
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("kernel not resolved for microvm runtime"))?;
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
        let dual_live = prior_runtime.is_some();

        // Generation identity: when replacing a live service, boot the candidate
        // under `{service_id}_g{gen}` so the active generation keeps its TAP/port
        // until ingress.swap and cutover complete (zero-downtime path).
        let generation_id = new_generation_id();
        let runtime_key = if dual_live {
            let key = format!("{service_id}_g{generation_id}");
            MicrovmRunner::validate_service_id(&key)?;
            key
        } else {
            service_id.to_string()
        };

        let russel_dir = format!("/var/lib/russel/{}", service_id);
        let microvms_dir = format!("/var/lib/microvms/{}", service_id);
        let russel_bak = format!("{}.bak", russel_dir);
        let microvms_bak = format!("{}.bak", microvms_dir);
        let has_russel_dir = Path::new(&russel_dir).exists();
        let has_microvms_dir = Path::new(&microvms_dir).exists();
        let has_backup = has_russel_dir && !dual_live;

        // Cold path only: destroy-in-place before boot (no live prior).
        // Dual-live keeps the active generation untouched until cutover.
        if !dual_live {
            // Order (F-23): rename dirs to .bak FIRST so any failure after this
            // point is rollback-covered. Then disarm the supervisor + kill children.
            // 1. Rename dirs to .bak (creates rollback safety net)
            // 2. take_processes — disarm supervisor
            // 3. kill+wait old children so ports are freed
            // 4. destroy_prior_runtime — cleans TAP/ports (rollback will re-create)
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

            let (old_vm_proc, old_aux_procs) = self
                .state
                .take_processes(service_id)
                .unwrap_or((None, Vec::new()));

            kill_and_wait_children(old_vm_proc, old_aux_procs).await;

            if let Some(prior_kind) = prior_runtime
                && let Err(e) =
                    destroy_prior_runtime(prior_kind, service_id, &self.runner, &self.containers)
                        .await
            {
                if has_backup {
                    let _ = tokio::fs::rename(&russel_bak, &russel_dir).await;
                    if has_microvms_dir {
                        let _ = tokio::fs::rename(&microvms_bak, &microvms_dir).await;
                    }
                }
                anyhow::bail!("failed to teardown prior {}: {}", prior_kind, e);
            }
        } else {
            tracing::info!(
                service_id,
                generation_id = %generation_id,
                runtime_key = %runtime_key,
                "dual-live redeploy: keeping active generation until candidate is ready"
            );
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "candidate".into(),
                    description: format!(
                        "Booting generation {generation_id} alongside active service"
                    ),
                })
                .await;
        }

        // H4: remember the fixed host port the operator requested so we can
        // try to re-claim it after dual-live cutover destroys the old gen.
        // Keep 0 out of recovery even if an invalid mapping slipped through.
        let fixed_host = request.port.as_ref().map(|p| p.host).filter(|&h| h != 0);

        let mut port_reservation = None;
        let deploy_result = async {
            // Determine port first, then arm the reservation (F-28: avoid
            // releasing the old service's port on early allocation failure).
            let port = if dual_live {
                // Always allocate a fresh backend port for the candidate so the
                // active generation keeps its listener. Fixed -p is Traefik-facing
                // after cutover; backend port may differ across generations.
                if request.port.is_some() {
                    tracing::info!(
                        service_id,
                        "dual-live redeploy: ignoring fixed -p for candidate backend; \
                         Traefik host stays stable via Ingress::swap"
                    );
                }
                let ports = self.ports.clone();
                let key = runtime_key.clone();
                let host = tokio::task::spawn_blocking(move || ports.next(&key)).await??;
                PortMapping {
                    host,
                    guest: config.service.port,
                }
            } else {
                match request.port.clone() {
                    Some(p) => {
                        PortAllocator::reserve(&runtime_key, p.host)?;
                        p
                    }
                    None => {
                        let ports = self.ports.clone();
                        let key = runtime_key.clone();
                        let host = tokio::task::spawn_blocking(move || ports.next(&key)).await??;
                        PortMapping {
                            host,
                            guest: config.service.port,
                        }
                    }
                }
            };
            // Arm reservation only after port is secured.
            port_reservation = Some(PortReservation::new(&runtime_key));

            match runtime {
                RuntimeKind::Microvm => {
                    self.deploy_microvm(
                        &runtime_key,
                        &config,
                        &build.store_path,
                        &port,
                        kernel
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("kernel not resolved"))?,
                        &merged_env,
                        &tx,
                        Some(generation_id.as_str()),
                        desired_state.as_ref(),
                    )
                    .await
                }
                RuntimeKind::Container => {
                    self.deploy_container(
                        &runtime_key,
                        &config,
                        &build.store_path,
                        &port,
                        &request.podman_args,
                        &merged_env,
                        &tx,
                        Some(generation_id.as_str()),
                        desired_state.as_ref(),
                    )
                    .await
                }
            }
        }
        .await;

        let (workload, create_ms, start_ms, network_ms, ready_ms) = match deploy_result {
            Ok(val) => val,
            Err(deploy_err) => {
                tracing::error!(service_id, runtime_key = %runtime_key, error = %deploy_err, "Deployment failed — cleaning up candidate (prior runtime was {prior_runtime:?})");
                // Only destroy the candidate; dual-live leaves the active generation alone.
                cleanup_failed_deploy(runtime, &runtime_key, &self.runner, &self.containers).await;
                if dual_live {
                    // Active generation never stopped — report hard failure without rollback.
                    return Err(deploy_err
                        .context("candidate generation failed; active generation left untouched"));
                }
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
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "port reservation dropped before deploy completed"
                                    )
                                })?
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
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "port reservation dropped before deploy completed"
                                    )
                                })?
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

        let backend = Backend::localhost(workload.port().host);
        let ingress_result = if dual_live {
            // Zero-downtime cutover: rewrite Traefik backend for the stable service id.
            tracing::info!(
                service_id,
                backend = %backend.url(),
                generation_id = %generation_id,
                "Ingress::swap — pointing stable route at candidate generation"
            );
            self.ingress.swap(service_id, &backend).await
        } else {
            self.ingress.register(service_id, &backend, &[]).await
        };

        if let Err(error) = ingress_result {
            // Full candidate teardown on ingress failure (#116). Active gen stays if dual-live.
            workload.teardown_network().await;
            match &workload {
                DeployWorkload::Microvm { .. } => {
                    if let Err(e) = self.runner.destroy(&runtime_key).await {
                        tracing::warn!(runtime_key = %runtime_key, error = %e, "failed to destroy microVM after ingress failure");
                    }
                }
                DeployWorkload::Container { .. } => {
                    if let Err(e) = self.containers.destroy(&runtime_key).await {
                        tracing::warn!(runtime_key = %runtime_key, error = %e, "failed to destroy container after ingress failure");
                    }
                }
            }
            PortAllocator::release(&runtime_key);
            return Err(error);
        }

        if dual_live {
            // Drain old generation only after successful swap.
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "cutover".into(),
                    description: "Draining previous generation after ingress swap".into(),
                })
                .await;

            let (old_vm_proc, old_aux_procs) = self
                .state
                .take_processes(service_id)
                .unwrap_or((None, Vec::new()));
            kill_and_wait_children(old_vm_proc, old_aux_procs).await;

            if let Some(prior_kind) = prior_runtime
                && let Err(e) =
                    destroy_prior_runtime(prior_kind, service_id, &self.runner, &self.containers)
                        .await
            {
                tracing::error!(
                    service_id,
                    error = %e,
                    "CRITICAL: candidate is live and swapped but old generation destroy failed"
                );
                // Continue promote — traffic is already on the candidate.
            }

            // Promote candidate dirs to the stable service_id path.
            if let Err(e) = promote_generation(&runtime_key, service_id).await {
                tracing::error!(
                    service_id,
                    runtime_key = %runtime_key,
                    error = %e,
                    "CRITICAL: failed to promote generation dirs; candidate still running under runtime key"
                );
                // Keep state under runtime_key so stop/destroy can find it.
                self.state
                    .attach_flake_path(&runtime_key, repo_path.clone());
            } else {
                // Re-key port + in-memory state to the stable service id.
                PortAllocator::release(&runtime_key);
                if let Err(e) = PortAllocator::claim_existing(service_id, workload.port().host) {
                    tracing::warn!(service_id, error = %e, "failed to claim port under service_id after promote");
                }
                self.state.rekey_service(&runtime_key, service_id);
                self.state.attach_flake_path(service_id, repo_path.clone());

                // H4: After old gen is destroyed, reclaim operator fixed `-p` for
                // microVMs only (spawn extra socat — never TapForwarder::setup,
                // which deletes TAP). Containers keep the candidate publish port
                // (rebind would require podman recreate); Traefik is SoT.
                if let Some(fixed) = fixed_host
                    && fixed != workload.port().host
                    && matches!(runtime, RuntimeKind::Microvm)
                {
                    let ephemeral = workload.port().host;
                    // service_id currently holds ephemeral via claim_existing above.
                    match PortAllocator::reserve(service_id, fixed) {
                        Ok(()) => {
                            tracing::info!(
                                service_id,
                                fixed,
                                ephemeral,
                                "reclaimed fixed host port after dual-live cutover"
                            );
                            if let DeployWorkload::Microvm {
                                ref alloc,
                                ref port,
                                ..
                            } = workload
                            {
                                match TapForwarder::spawn_socat(
                                    service_id,
                                    fixed,
                                    &alloc.vm_ip,
                                    port.guest,
                                )
                                .await
                                {
                                    Ok(socat) => {
                                        let mut inner = self.state.lock_inner();
                                        if let Some(s) = inner.services.get_mut(service_id) {
                                            s.aux_processes.push(socat);
                                            s.host_port = Some(fixed);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            service_id,
                                            error = %e,
                                            "dual-live fixed-port socat spawn failed; \
                                             traffic remains on ephemeral {ephemeral}"
                                        );
                                        // Restore ephemeral registration for status/list.
                                        let _ =
                                            PortAllocator::claim_existing(service_id, ephemeral);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                service_id,
                                fixed,
                                error = %e,
                                "dual-live fixed port not free — using candidate ephemeral"
                            );
                        }
                    }
                }
            }
        } else {
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
        }

        // Record source so `russel update` / health restart can redeploy.
        if let Err(e) =
            record_source_in_metadata(service_id, &request.repo_url, &request.config_path)
        {
            tracing::warn!(service_id, error = %e, "failed to record source in metadata");
        }

        // Append deployment history journal (operators / dashboard rollback surface).
        let port = workload.port().clone();
        let desired_snap = DesiredStateSnapshot::from_desired_json(
            desired_state.as_ref(),
            Some(port.host),
            Some(port.guest),
        );
        if let Err(e) = deployments::append_success(
            service_id,
            AppendSuccess {
                generation_id: Some(generation_id.clone()),
                runtime: Some(runtime),
                store_path: Some(build.store_path.display().to_string()),
                repo_url: Some(request.repo_url.clone()),
                config_path: Some(request.config_path.clone()),
                host_port: Some(port.host),
                guest_port: Some(port.guest),
                message: Some("deploy complete".into()),
                desired_state: Some(desired_snap),
            },
        ) {
            tracing::warn!(service_id, error = %e, "failed to append deployment history");
        }

        port_reservation
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("port reservation dropped before deploy completed"))?
            .disarm();

        Ok(DeployInnerResult::Success(Box::new(DeployOutput {
            store_path: build.store_path,
            port,
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
}

pub(crate) struct PortReservation {
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

pub(crate) enum DeployWorkload {
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

pub(crate) struct DeployOutput {
    store_path: PathBuf,
    port: PortMapping,
    runtime: RuntimeKind,
    timing: DeployTiming,
    workload: DeployWorkload,
}
