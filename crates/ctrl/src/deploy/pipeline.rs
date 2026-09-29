//! DeployPipeline: request entrypoint and workload types. The deploy phases
//! themselves are in `phases.rs`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Instant,
};

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployResponse, DeployTiming, PortMapping},
    config::{GuestKind, RuntimeKind, Russelfile},
    volumes::{ExtraPortSpec, ResolvedVolume},
};

use crate::{
    build::{self, BuildBackend},
    container::{ContainerRunner, attach_managed_volumes, detach_managed_volumes},
    deployments::{self, DesiredStateSnapshot},
    git::{GitClient, redact_repo_url},
    ingress::{self, Ingress},
    metadata::{rewrite_metadata_service_id, write_metadata},
    microvm::{self, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation},
    state::AppState,
};

use super::config::load_russelfile_under_repo;

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
    // Managed volumes live in the stable service dir, which destroy-of-prior
    // leaves in place. Park them, replace the dir with the generation tree,
    // then put the data back. Otherwise promote fails with "target already exists"
    // whenever `keep` left `volumes/` behind.
    let russel_from = crate::paths::service_dir(runtime_key);
    let russel_to = crate::paths::service_dir(service_id);
    if russel_from.exists() {
        if russel_to.exists() {
            detach_managed_volumes(&russel_to).await?;
            crate::container::remove_tree(&russel_to)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("remove promote target {}: {e}", russel_to.display())
                })?;
        }
        tokio::fs::rename(&russel_from, &russel_to)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "rename {} -> {}: {e}",
                    russel_from.display(),
                    russel_to.display()
                )
            })?;
        attach_managed_volumes(&russel_to).await?;
    }
    let micro_from = crate::paths::microvm_dir(runtime_key).display().to_string();
    let micro_to = crate::paths::microvm_dir(service_id).display().to_string();
    if Path::new(&micro_from).exists() {
        if Path::new(&micro_to).exists() {
            anyhow::bail!("promote target already exists: {micro_to}");
        }
        tokio::fs::rename(&micro_from, &micro_to)
            .await
            .map_err(|e| anyhow::anyhow!("rename {micro_from} -> {micro_to}: {e}"))?;
    }
    let meta_path = crate::metadata::metadata_path(service_id)
        .display()
        .to_string();
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

/// Move the deployment history (`deployments.json`, `rollback.pending`) from
/// one service dir to another. Best effort: a missing file is fine.
pub(crate) async fn carry_history(from_id: &str, to_id: &str) {
    let from = crate::paths::service_dir(from_id);
    let to = crate::paths::service_dir(to_id);
    let journal = deployments::deployments_path(from_id);
    let names = [
        journal.file_name().map(|n| n.to_os_string()),
        Some("rollback.pending".into()),
    ];
    for name in names.into_iter().flatten() {
        let src = from.join(&name);
        if !src.exists() {
            continue;
        }
        if let Err(e) = tokio::fs::create_dir_all(&to).await {
            tracing::warn!(dir = %to.display(), error = %e, "cannot keep deployment history");
            return;
        }
        if let Err(e) = tokio::fs::rename(&src, to.join(&name)).await {
            tracing::warn!(file = %src.display(), error = %e, "cannot keep deployment history");
        }
    }
}

/// Persist repo/config so update + health restart can rebuild desired state.
pub(crate) fn record_source_in_metadata(
    service_id: &str,
    repo_url: &str,
    config_path: &str,
) -> anyhow::Result<()> {
    let path = crate::metadata::metadata_path(service_id)
        .display()
        .to_string();
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
    object.insert(
        "repo_url".into(),
        serde_json::json!(redact_repo_url(repo_url)),
    );
    object.insert("config_path".into(), serde_json::json!(config_path));
    write_metadata(&path, &value)
}

/// JSON object written into metadata `desired_state` (next to `"runtime"`).
///
/// Env is the pre-secret-resolution map so rollback and health restart
/// re-resolve `secret://` refs. `guest` is always present (default busybox).
/// Optional Russelfile extras stored in `desired_state` for rollback/update.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DesiredExtras<'a> {
    pub volumes: &'a [ResolvedVolume],
    pub extra_ports: &'a [ExtraPortSpec],
    pub package: Option<&'a str>,
    pub args: &'a [String],
    pub run_as: Option<super::RunAs>,
    pub restart: Option<&'a str>,
    /// Commit the source built, and whether its tree differed (#448).
    pub rev: Option<&'a crate::git::SourceRev>,
}

pub(crate) fn build_desired_state(
    repo_url: &str,
    config_path: &str,
    runtime: RuntimeKind,
    guest: GuestKind,
    env: &HashMap<String, String>,
    podman_args: &[String],
    port: Option<&PortMapping>,
    ingress_host: Option<&str>,
    extras: DesiredExtras<'_>,
) -> serde_json::Value {
    let mut ds = serde_json::Map::new();
    let env_obj: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    ds.insert("env".into(), serde_json::Value::Object(env_obj));
    if !podman_args.is_empty() {
        ds.insert(
            "podman_args".into(),
            serde_json::Value::Array(
                podman_args
                    .iter()
                    .map(|a| serde_json::Value::String(a.clone()))
                    .collect(),
            ),
        );
    }
    ds.insert(
        "repo_url".into(),
        serde_json::Value::String(redact_repo_url(repo_url)),
    );
    ds.insert(
        "config_path".into(),
        serde_json::Value::String(config_path.to_string()),
    );
    ds.insert(
        "runtime".into(),
        serde_json::Value::String(runtime.to_string()),
    );
    ds.insert("guest".into(), serde_json::Value::String(guest.to_string()));
    if let Some(p) = port {
        let mut port_obj = serde_json::Map::new();
        port_obj.insert("host".into(), serde_json::json!(p.host));
        port_obj.insert("guest".into(), serde_json::json!(p.guest));
        ds.insert("port".into(), serde_json::Value::Object(port_obj));
    }
    if let Some(host) = ingress_host {
        ds.insert(
            "ingress_host".into(),
            serde_json::Value::String(host.into()),
        );
    }
    if !extras.volumes.is_empty()
        && let Ok(v) = serde_json::to_value(extras.volumes)
    {
        ds.insert("volumes".into(), v);
    }
    if !extras.extra_ports.is_empty()
        && let Ok(v) = serde_json::to_value(extras.extra_ports)
    {
        ds.insert("extra_ports".into(), v);
    }
    if let Some(package) = extras.package {
        ds.insert("package".into(), serde_json::Value::String(package.into()));
    }
    if !extras.args.is_empty()
        && let Ok(v) = serde_json::to_value(extras.args)
    {
        ds.insert("args".into(), v);
    }
    if let Some(run_as) = extras.run_as {
        ds.insert(
            "run_as".into(),
            serde_json::Value::String(run_as.as_str().into()),
        );
    }
    if let Some(restart) = extras.restart {
        ds.insert("restart".into(), serde_json::Value::String(restart.into()));
    }
    if let Some(rev) = extras.rev {
        ds.insert("rev".into(), serde_json::Value::String(rev.rev.clone()));
        if rev.dirty {
            ds.insert("dirty".into(), serde_json::Value::Bool(true));
        }
    }
    serde_json::Value::Object(ds)
}

/// Return the configured listen port from an address environment variable.
/// The controller and agent use `host:port` strings; taking the final colon
/// also handles bracketed IPv6 addresses without resolving hostnames here.
fn configured_listen_port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|addr| addr.rsplit_once(':').map(|(_, port)| port.to_string()))
        .and_then(|port| port.parse().ok())
        .unwrap_or(default)
}

pub(super) fn validate_live_ingress_port(port: Option<u16>) -> anyhow::Result<()> {
    let Some(port) = port else {
        return Ok(());
    };
    // Privileged pins are rejected here as well as in `PortAllocator::reserve`:
    // dual-live redeploys allocate an ephemeral candidate backend and never
    // reserve the pin, but the pin still returns via `fixed_host` reclaim.
    reject_live_listen_collision(port, "ingress.port")
}

/// Host ports published on the machine, including `[[ports]]` rows.
///
/// Load-time checks only know the default 7878/7946. The process may be bound
/// elsewhere via `RUSSEL_CTRL_ADDR` / `RUSSEL_AGENT_ADDR`.
pub(super) fn reject_live_listen_collision(port: u16, what: &str) -> anyhow::Result<()> {
    if port < 1024 {
        anyhow::bail!("{what} {port} is privileged (< 1024); Traefik owns 80/443");
    }
    if port == configured_listen_port("RUSSEL_CTRL_ADDR", 7878) {
        anyhow::bail!("{what} {port} collides with the control-plane listen port");
    }
    if port == configured_listen_port("RUSSEL_AGENT_ADDR", 7946) {
        anyhow::bail!("{what} {port} collides with the agent listen port");
    }
    Ok(())
}

/// Typed outcome of `deploy_inner` — success, rollback, or hard failure.
pub(crate) enum DeployInnerResult {
    // Box large success payload (clippy large_enum_variant).
    Success(Box<DeployOutput>),
    RolledBack { runtime: RuntimeKind, error: String },
}

/// Source checkout and Russelfile for one deploy, loaded before the service
/// id is known. The lease keeps the checkout alive until the deploy ends.
pub(super) struct ResolvedSource {
    pub(super) repo_path: PathBuf,
    pub(super) _checkout: crate::git::CheckoutLease,
    pub(super) config: Russelfile,
    /// Commit the source builds; `None` outside git (#448).
    pub(super) rev: Option<crate::git::SourceRev>,
}

/// The service id is the Russelfile `service.name` (#446). A requested id
/// is only a check: it never renames the service.
fn check_requested_id(requested: Option<&str>, name: &str) -> anyhow::Result<()> {
    match requested {
        Some(requested) if requested != name => anyhow::bail!(
            "Russelfile service.name is {name:?} but the request targets {requested:?}; \
             the service id is service.name (to rename, deploy under the new name, \
             then destroy {requested:?})"
        ),
        _ => Ok(()),
    }
}

/// Failed response for a deploy rejected before `deploy_inner` ran.
fn rejected_response(
    service_id: String,
    started: Instant,
    runtime: Option<RuntimeKind>,
    error: anyhow::Error,
) -> DeployResponse {
    DeployResponse {
        vm_id: service_id.clone(),
        service_id,
        status: "failed".to_string(),
        store_path: None,
        microvm_config_path: None,
        port: None,
        elapsed_ms: started.elapsed().as_millis(),
        timing: None,
        vm_ip: None,
        runtime,
        message: format!("{error:#}"),
        route_host: None,
        backend_port: None,
        rev: None,
    }
}

pub struct DeployPipeline {
    pub(crate) state: AppState,
    pub(crate) git: GitClient,
    pub(crate) builder: Arc<dyn BuildBackend>,
    pub(crate) runner: MicrovmRunner,
    pub(crate) containers: ContainerRunner,
    pub(crate) ports: PortAllocator,
    pub(crate) ingress: Arc<dyn Ingress>,
    claimed_id: Arc<OnceLock<String>>,
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
            .field("claimed_id", &self.claimed_id.get())
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
            claimed_id: Arc::default(),
        }
    }

    /// Slot that holds the service id once this pipeline has claimed it
    /// (`mark_building`). The id comes from the Russelfile, so a caller that
    /// must clean up after a panicked deploy reads it from here.
    pub fn claimed_service_id(&self) -> Arc<OnceLock<String>> {
        Arc::clone(&self.claimed_id)
    }

    pub async fn deploy(
        &self,
        request: DeployRequest,
        tx: tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> DeployResponse {
        let started = Instant::now();
        let requested_id = request
            .vm_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        tracing::info!(
            requested_id = ?requested_id,
            repo = %redact_repo_url(&request.repo_url),
            "deploy started"
        );

        // The Russelfile is read before any state is touched: its
        // `service.name` is the service id (#446).
        let source = match self
            .resolve_source(&request, requested_id.as_deref(), &tx)
            .await
        {
            Ok(source) => source,
            Err(e) => {
                tracing::error!(error = %e, "deploy rejected: could not resolve source");
                return rejected_response(requested_id.unwrap_or_default(), started, None, e);
            }
        };
        let service_id = source.config.service.name.clone();
        let vm_id = service_id.clone();
        let runtime = Some(source.config.service.runtime);

        // Defense in depth: load already applies the service-id rule to service.name.
        if let Err(e) = russel_core::ids::validate_service_id(&service_id) {
            tracing::error!(service_id = %service_id, error = %e, "deploy rejected: invalid service_id");
            return rejected_response(service_id, started, runtime, e);
        }

        if !request.force
            && let Some(response) = self.unchanged_response(&service_id, &request, &source, started)
        {
            tracing::info!(service_id = %service_id, "deploy unchanged: already running this source");
            return response;
        }

        if let Err(e) = self.state.mark_building(&service_id) {
            tracing::error!(service_id = %service_id, error = %e, "deploy rejected: service busy");
            return rejected_response(service_id, started, runtime, e);
        }
        let _ = self.claimed_id.set(service_id.clone());

        let request_runtime = runtime;
        let result = self
            .deploy_inner(&service_id, request, source, started, tx)
            .await;

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
                let fixed_port_socat = output.fixed_port_socat;
                let fixed_host_port = output.fixed_host_port;
                let recorded_host = fixed_host_port.unwrap_or(host_port);
                let (message, vm_ip, microvm_config_path) = match output.workload {
                    DeployWorkload::Microvm {
                        alloc,
                        initramfs_path,
                        vm_child,
                        virtiofsd_children,
                        socat_child,
                        extra_forwarders,
                        ..
                    } => {
                        let mut aux = vec![*socat_child];
                        aux.extend(extra_forwarders);
                        aux.extend(virtiofsd_children);
                        if let Some(fixed) = fixed_port_socat {
                            aux.push(fixed);
                        }
                        self.state.mark_deployed_with_aux(
                            &service_id,
                            *vm_child,
                            aux,
                            Some(recorded_host),
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
                let route_host = output
                    .route_host
                    .or_else(|| self.ingress.primary_host(&service_id));
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "deployed".to_string(),
                    store_path: Some(output.store_path.display().to_string()),
                    microvm_config_path,
                    port: Some(output.port),
                    elapsed_ms: elapsed,
                    timing: Some(output.timing),
                    vm_ip,
                    runtime: Some(output.runtime),
                    message,
                    route_host,
                    backend_port: Some(host_port),
                    rev: output.rev,
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
                    rev: None,
                }
            }
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
                deployments::clear_pending_rollback(&service_id);
                tracing::error!(
                    service_id = %service_id,
                    elapsed_ms = elapsed,
                    error = %format!("{error:#}"),
                    "deploy failed"
                );
                self.state.mark_failed(&service_id, format!("{error:#}"));
                DeployResponse {
                    service_id,
                    vm_id,
                    status: "failed".to_string(),
                    store_path: None,
                    microvm_config_path: None,
                    port: None,
                    elapsed_ms: elapsed,
                    timing: None,
                    vm_ip: None,
                    runtime: request_runtime,
                    message: format!("{error:#}"),
                    route_host: None,
                    backend_port: None,
                    rev: None,
                }
            }
        }
    }

    /// `Some` when the service already runs exactly this source: same repo and
    /// config path at the same clean commit (#448). Clean means the Russelfile
    /// is tracked and unmodified, so the commit pins its contents too. A dirty
    /// tree or a source outside git always deploys.
    fn unchanged_response(
        &self,
        service_id: &str,
        request: &DeployRequest,
        source: &ResolvedSource,
        started: Instant,
    ) -> Option<DeployResponse> {
        let rev = source.rev.as_ref().filter(|r| !r.dirty)?;
        let status = self.state.status(service_id)?;
        if status.status != russel_core::api::ServiceStatus::Deployed.as_str()
            || status.vm_state != russel_core::api::VmState::Running.as_str()
        {
            return None;
        }
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::metadata::metadata_path(service_id)).ok()?,
        )
        .ok()?;
        let recorded =
            DesiredStateSnapshot::from_desired_json(meta.get("desired_state"), None, None);
        let same = recorded.rev.as_deref() == Some(rev.rev.as_str())
            && !recorded.dirty
            && recorded.repo_url.as_deref() == Some(redact_repo_url(&request.repo_url).as_str())
            && recorded.config_path.as_deref() == Some(request.config_path.as_str());
        if !same {
            return None;
        }
        let short = rev.rev.get(..12).unwrap_or(&rev.rev);
        Some(DeployResponse {
            vm_id: service_id.to_string(),
            service_id: service_id.to_string(),
            status: "unchanged".to_string(),
            store_path: meta
                .get("store_path")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            microvm_config_path: None,
            port: status
                .host_port
                .zip(status.guest_port)
                .map(|(host, guest)| PortMapping { host, guest }),
            elapsed_ms: started.elapsed().as_millis(),
            timing: None,
            vm_ip: meta
                .get("vm_ip")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            runtime: status.runtime,
            message: format!(
                "already running {short} with this Russelfile; nothing to apply (--force redeploys)"
            ),
            route_host: status.route_host,
            backend_port: status.host_port,
            rev: Some(rev.rev.clone()),
        })
    }

    /// Fetch the source and load its Russelfile. A `requested_id` (API
    /// `vm_id`, or the id `update` targets) must equal `service.name`.
    async fn resolve_source(
        &self,
        request: &DeployRequest,
        requested_id: Option<&str>,
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
    ) -> anyhow::Result<ResolvedSource> {
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "resolve".into(),
                description: "Resolving source & Russelfile".into(),
            })
            .await;
        let (repo_path, checkout) = match request.rev.as_deref() {
            // A pinned checkout's lease covers the clone root, which a local
            // subdir deploy's `repo_path` is below.
            Some(rev) => self.git.checkout_rev(&request.repo_url, rev).await?,
            None => {
                let (repo_path, checkout_lease) =
                    self.git.clone_or_use_local(&request.repo_url).await?;
                let checkout = self.git.hold_checkout(&repo_path);
                drop(checkout_lease);
                (repo_path, checkout)
            }
        };
        let config = load_russelfile_under_repo(&repo_path, &request.config_path)?;
        check_requested_id(requested_id, &config.service.name)?;
        let rev = crate::git::source_rev(&repo_path, &request.config_path).await;
        if let Some(pinned) = request.rev.as_deref()
            && rev.as_ref().map(|r| r.rev.as_str()) != Some(pinned)
        {
            anyhow::bail!("checkout of {pinned} resolved to {rev:?}");
        }
        Ok(ResolvedSource {
            repo_path,
            _checkout: checkout,
            config,
            rev,
        })
    }
}

pub(crate) enum DeployWorkload {
    Microvm {
        alloc: SubnetAllocation,
        vm_child: Box<tokio::process::Child>,
        virtiofsd_children: Vec<tokio::process::Child>,
        /// Port forwarder: socat (tap) or passt.
        socat_child: Box<tokio::process::Child>,
        /// tap: one socat per `[[ports]]` row.
        extra_forwarders: Vec<tokio::process::Child>,
        net_mode: crate::network::MicrovmNetMode,
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
    pub(super) fn port(&self) -> &PortMapping {
        match self {
            Self::Microvm { port, .. } | Self::Container { port, .. } => port,
        }
    }

    pub(super) async fn teardown_network(&self) {
        if let Self::Microvm {
            alloc, net_mode, ..
        } = self
            && let Err(teardown_error) =
                crate::network::MicrovmNet::teardown(*net_mode, alloc).await
        {
            tracing::warn!(error = %teardown_error, "failed to tear down TAP after Traefik registration failure");
        }
    }
}

pub(crate) struct DeployOutput {
    pub(super) store_path: PathBuf,
    pub(super) port: PortMapping,
    pub(super) runtime: RuntimeKind,
    pub(super) timing: DeployTiming,
    pub(super) route_host: Option<String>,
    pub(super) workload: DeployWorkload,
    /// Extra socat for the operator's pinned host port after dual-live cutover.
    ///
    /// Held here, not in `AppState`, until `mark_deployed_with_aux`. That call
    /// replaces `aux_processes`, which dropped and killed a child pushed earlier.
    pub(super) fixed_port_socat: Option<tokio::process::Child>,
    pub(super) fixed_host_port: Option<u16>,
    /// Commit this generation was built from.
    pub(super) rev: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod ingress_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        ctrl: Option<std::ffi::OsString>,
        agent: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn pin_defaults() -> Self {
            Self::pin("127.0.0.1:7878", "127.0.0.1:7946")
        }

        fn pin_ctrl(ctrl: &str) -> Self {
            Self::pin(ctrl, "127.0.0.1:7946")
        }

        fn pin(ctrl_addr: &str, agent_addr: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ctrl = std::env::var_os("RUSSEL_CTRL_ADDR");
            let agent = std::env::var_os("RUSSEL_AGENT_ADDR");
            // SAFETY: `_lock` is held until drop, so tests in this module
            // serialize mutation of these two vars and restore both on drop.
            unsafe {
                std::env::set_var("RUSSEL_CTRL_ADDR", ctrl_addr);
                std::env::set_var("RUSSEL_AGENT_ADDR", agent_addr);
            }
            Self {
                ctrl,
                agent,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.ctrl.take() {
                    Some(v) => std::env::set_var("RUSSEL_CTRL_ADDR", v),
                    None => std::env::remove_var("RUSSEL_CTRL_ADDR"),
                }
                match self.agent.take() {
                    Some(v) => std::env::set_var("RUSSEL_AGENT_ADDR", v),
                    None => std::env::remove_var("RUSSEL_AGENT_ADDR"),
                }
            }
        }
    }

    #[test]
    fn live_ingress_port_uses_canonical_default_collision_messages() {
        let _guard = EnvGuard::pin_defaults();
        assert_eq!(
            validate_live_ingress_port(Some(7878))
                .unwrap_err()
                .to_string(),
            "ingress.port 7878 collides with the control-plane listen port"
        );
        assert_eq!(
            validate_live_ingress_port(Some(7946))
                .unwrap_err()
                .to_string(),
            "ingress.port 7946 collides with the agent listen port"
        );
        assert_eq!(
            validate_live_ingress_port(Some(80))
                .unwrap_err()
                .to_string(),
            "ingress.port 80 is privileged (< 1024); Traefik owns 80/443"
        );
        validate_live_ingress_port(Some(4000)).unwrap();
        validate_live_ingress_port(None).unwrap();
    }

    #[test]
    fn requested_id_is_a_check_against_service_name() {
        check_requested_id(None, "api").unwrap();
        check_requested_id(Some("api"), "api").unwrap();
        let err = check_requested_id(Some("examples-basic-http"), "api")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("service.name is \"api\"")
                && err.contains("\"examples-basic-http\"")
                && err.contains("to rename"),
            "{err}"
        );
    }

    #[test]
    fn extra_port_rejects_live_ctrl_listen() {
        let _guard = EnvGuard::pin_ctrl("127.0.0.1:9000");
        assert!(
            reject_live_listen_collision(9000, "[[ports]] host")
                .unwrap_err()
                .to_string()
                .contains("9000")
        );
        reject_live_listen_collision(7878, "[[ports]] host").unwrap();
    }
}
