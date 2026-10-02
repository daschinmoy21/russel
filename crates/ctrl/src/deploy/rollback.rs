//! Prior-runtime teardown and dual-live rollback helpers.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use russel_core::config::RuntimeKind;
use russel_core::volumes::{ExtraPortSpec, ResolvedVolume, extra_port_key};

use crate::{
    container::{
        ContainerRunner, ContainerStartSpec, PreparedRootfs, validate_podman_passthrough_args,
    },
    metadata::{
        build_container_metadata, build_microvm_metadata, prior_runtime_from_disk, write_metadata,
    },
    microvm::{BootOutput, MicrovmRunner, ready},
    network::{
        MICROVM_READY_TIMEOUT, MicrovmNet, MicrovmNetMode, PortAllocator, SubnetAllocation,
        TapForwarder, subnet_for,
    },
    state::AppState,
    warm_pool::shared_warm_pool,
};

/// Extract `desired_state.env` from rollback metadata into a plain map
/// (string values only). Shared by the microVM and container rollback paths.
fn desired_state_env(old_meta: &serde_json::Value) -> HashMap<String, String> {
    old_meta
        .pointer("/desired_state/env")
        .and_then(|env| env.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// `desired_state.<key>` from rollback metadata, or the default when it is
/// absent or malformed.
fn desired<T: serde::de::DeserializeOwned + Default>(meta: &serde_json::Value, key: &str) -> T {
    meta.get("desired_state")
        .and_then(|ds| ds.get(key))
        .and_then(|v| T::deserialize(v).ok())
        .unwrap_or_default()
}

/// A required port or size field of recorded metadata.
fn required_u16(meta: &serde_json::Value, key: &str) -> anyhow::Result<u16> {
    let n = meta[key]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("metadata is missing {key}"))?;
    u16::try_from(n).map_err(|_| anyhow::anyhow!("metadata {key} out of u16 range"))
}

/// Resolve the prior runtime for a service before redeploy.
///
/// 1. Check on-disk metadata first (`prior_runtime_from_disk`).
/// 2. If no metadata (None), probe for a podman container named `russel-{service_id}`.
/// 3. If no container but the internal dirs exist, treat as legacy Microvm.
/// 4. Otherwise None (first deploy).
///
/// An unreadable metadata file is an error. Falling through to the legacy
/// microVM path would destroy a container.
pub(crate) async fn resolve_prior_runtime(service_id: &str) -> anyhow::Result<Option<RuntimeKind>> {
    match prior_runtime_from_disk(service_id) {
        Ok(Some(runtime)) => return Ok(Some(runtime)),
        Ok(None) => {}
        Err(e) => {
            anyhow::bail!(
                "metadata.json for {service_id} cannot be read ({e}); refusing to assume microvm"
            );
        }
    }

    // Probe podman for a running/stopped container with the russel label.
    let container_name = format!("russel-{}", service_id);
    let probe = crate::container::podman_command()
        .await
        .args(["container", "exists", &container_name])
        .output()
        .await;
    if let Ok(out) = &probe
        && out.status.success()
    {
        tracing::info!(
            service_id,
            container = %container_name,
            "discovered existing podman container (no metadata)"
        );
        return Ok(Some(RuntimeKind::Container));
    }

    // No metadata and no container: if any russel/microvms directory exists,
    // assume legacy Microvm so teardown can proceed correctly.
    // A destroy that kept volumes leaves only `volumes/` and no metadata.
    // That is not a VM; microVM destroy would remove_dir_all the data.
    let russel_dir = crate::paths::service_dir(service_id).display().to_string();
    let microvms_dir = crate::paths::microvm_dir(service_id).display().to_string();
    let russel_path = Path::new(&russel_dir);
    let microvms_path = Path::new(&microvms_dir);
    if russel_path.exists() || microvms_path.exists() {
        // `RUSSEL_DATA_DIR=/var/lib/microvms` makes the marker dir the
        // service dir itself; it existing then says nothing about a VM.
        let separate_marker = microvms_path.exists() && microvms_path != russel_path;
        if !separate_marker && crate::container::dir_is_kept_volumes_only(russel_path) {
            tracing::info!(
                service_id,
                "service dir has only kept volumes; not treating it as a microvm"
            );
            return Ok(None);
        }
        tracing::info!(
            service_id,
            "no metadata but dirs exist — treating prior as legacy Microvm"
        );
        return Ok(Some(RuntimeKind::Microvm));
    }

    Ok(None)
}

/// Retire a microVM generation's processes (#562). Write [`STOP_FILE`] into
/// its config dir: the guest init sends the app SIGTERM, and the guest
/// powers off once the app has exited. Wait up to `grace` for the VM to go
/// down, then SIGKILL whatever is left.
///
/// `service_dir` is where the generation's files are now (the `.bak` name
/// on the cold path). A VM this ctrl did not start (it restarted since) is
/// waited on through its recorded pid. A VM booted from an older initramfs
/// does not watch for the file and is killed when `grace` runs out.
pub(crate) async fn retire_microvm(
    service_dir: &Path,
    mut vm: Option<tokio::process::Child>,
    aux: Vec<tokio::process::Child>,
    grace: Duration,
) {
    let pid = vm.as_ref().and_then(tokio::process::Child::id).or_else(|| {
        crate::metadata::load_service_disk_record_from(&service_dir.join("metadata.json"))
            .and_then(|record| record.vm_pid)
    });
    if let Some(pid) = pid {
        let stop = service_dir.join("cfg").join(STOP_FILE);
        match std::fs::write(&stop, b"") {
            Ok(()) => {
                let exited = match vm.as_mut() {
                    Some(child) => tokio::time::timeout(grace, child.wait()).await.is_ok(),
                    None => crate::microvm::wait_for_process_exit(pid, grace).await,
                };
                if exited {
                    tracing::info!(pid, "retired microVM stopped after its app exited");
                } else {
                    tracing::warn!(
                        pid,
                        grace_secs = grace.as_secs(),
                        "retired microVM still running after the grace period; killing it"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(path = %stop.display(), error = %e, "cannot ask the retired microVM to stop; killing it");
            }
        }
    }
    kill_and_wait_children(vm, aux).await;
}

/// Name of the file [`retire_microvm`] writes into a generation's config
/// dir, which the guest sees as `/config/stop`.
pub(crate) const STOP_FILE: &str = "stop";

/// Kill + wait (with timeout) all old children so ports are free.
/// tokio `kill()` sends SIGKILL directly on Unix; there is no graceful phase.
/// [`retire_microvm`] gives the app its grace period first.
pub(crate) async fn kill_and_wait_children(
    old_vm_proc: Option<tokio::process::Child>,
    old_aux_procs: Vec<tokio::process::Child>,
) {
    let mut children: Vec<tokio::process::Child> = old_vm_proc.into_iter().collect();
    children.extend(old_aux_procs);
    for mut child in children {
        let _ = child.kill().await;
        let wait = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        if let Err(_elapsed) = wait {
            tracing::warn!("child process did not exit within 5s after SIGKILL");
        }
    }
}

pub(crate) async fn destroy_prior_runtime(
    prior: RuntimeKind,
    service_id: &str,
    microvm_runner: &MicrovmRunner,
) -> anyhow::Result<()> {
    match prior {
        RuntimeKind::Microvm => microvm_runner.destroy(service_id).await,
        RuntimeKind::Container => {
            // Redeploy must not apply destroy's keep policy. The new generation
            // bind-mounts the same managed directories.
            crate::container::destroy_preserving_volumes(service_id).await?;
            PortAllocator::release_service(service_id);
            Ok(())
        }
    }
}

pub(crate) async fn cleanup_failed_deploy(
    runtime: RuntimeKind,
    service_id: &str,
    microvm_runner: &MicrovmRunner,
) {
    let _ = match runtime {
        RuntimeKind::Microvm => microvm_runner.destroy(service_id).await,
        RuntimeKind::Container => {
            let result = crate::container::destroy_preserving_volumes(service_id).await;
            PortAllocator::release_service(service_id);
            result
        }
    };
}

/// A service's live dirs and the `.bak` names a cold deploy parks them at.
pub(super) struct ServiceDirs {
    pub(super) russel: String,
    pub(super) microvms: String,
    pub(super) russel_bak: String,
    pub(super) microvms_bak: String,
    pub(super) has_microvms: bool,
}

impl ServiceDirs {
    pub(super) fn of(service_id: &str) -> Self {
        let russel = crate::paths::service_dir(service_id).display().to_string();
        let microvms = crate::paths::microvm_dir(service_id).display().to_string();
        Self {
            russel_bak: format!("{russel}.bak"),
            microvms_bak: format!("{microvms}.bak"),
            has_microvms: Path::new(&microvms).exists(),
            russel,
            microvms,
        }
    }

    /// Put the `.bak` dirs back after a failed cold deploy, best effort.
    pub(super) async fn restore(&self) {
        if let Err(e) = crate::container::restore_backed_up_service_dir(
            Path::new(&self.russel),
            Path::new(&self.russel_bak),
        )
        .await
        {
            tracing::warn!(
                error = %e,
                russel_dir = %self.russel,
                russel_bak = %self.russel_bak,
                "failed to restore service dir from backup"
            );
        }
        if self.has_microvms {
            let _ = tokio::fs::rename(&self.microvms_bak, &self.microvms).await;
        }
    }

    /// Best-effort cleanup of the pre-deploy backup dirs. Use remove_dir_all
    /// rather than shelling out to `rm -rf` (F-46); only a keep-id rootfs
    /// falls back to `podman unshare rm` (#464). A missing dir (already
    /// cleaned) is not an error.
    pub(super) async fn remove_backups(&self) {
        if let Err(e) = crate::container::remove_tree(Path::new(&self.russel_bak)).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.russel_bak,
                error = %e,
                "failed to remove russel backup dir after deploy"
            );
        }
        if self.has_microvms
            && let Err(e) = std::fs::remove_dir_all(&self.microvms_bak)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.microvms_bak,
                error = %e,
                "failed to remove microvms backup dir after deploy"
            );
        }
    }
}

pub(crate) async fn attempt_microvm_rollback(
    service_id: &str,
    dirs: &ServiceDirs,
    runner: &MicrovmRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    // 0. Validate backup metadata BEFORE renaming (F-16: avoid unrecoverable
    //    half-restore when backup metadata is corrupt/missing).
    let old_metadata_bak_path = format!("{}/metadata.json", dirs.russel_bak);
    let content = std::fs::read_to_string(&old_metadata_bak_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;
    let recorded = RecordedMicrovm::from_metadata(service_id, old_meta, runner).await?;

    // All validations passed — now rename safely.
    // 1. Restore backup dirs, putting stashed volumes back on the live path.
    crate::container::restore_backed_up_service_dir(
        Path::new(&dirs.russel),
        Path::new(&dirs.russel_bak),
    )
    .await?;
    if dirs.has_microvms {
        tokio::fs::rename(&dirs.microvms_bak, &dirs.microvms).await?;
    }

    recorded
        .launch(
            service_id,
            &dirs.russel,
            runner,
            state,
            FailedLaunch::Destroy,
        )
        .await
}

/// What a failed [`RecordedMicrovm::launch`] leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailedLaunch {
    /// Remove the service dir and release its ports (rollback).
    Destroy,
    /// Stop the processes but keep the dir, recorded metadata, and ports, so
    /// a later relaunch can retry (restart-on-exit).
    Keep,
}

/// A microVM generation recorded in `metadata.json`, validated and ready to
/// boot again: by rollback from its backup, or in place by restart-on-exit.
pub(crate) struct RecordedMicrovm {
    meta: serde_json::Value,
    host_port: u16,
    guest_port: u16,
    kernel_path: PathBuf,
    mem_mb: u16,
    bin_name: String,
    app_path: String,
    store_path: String,
    initramfs_path: PathBuf,
    user_env: HashMap<String, String>,
    run_as: super::RunAs,
}

impl RecordedMicrovm {
    /// Validate everything a boot needs before anything on disk changes.
    pub(crate) async fn from_metadata(
        service_id: &str,
        meta: serde_json::Value,
        runner: &MicrovmRunner,
    ) -> anyhow::Result<Self> {
        let host_port = required_u16(&meta, "host_port")?;
        let guest_port = required_u16(&meta, "guest_port")?;
        let kernel_path = PathBuf::from(
            meta["kernel_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing kernel_path"))?,
        );
        if !kernel_path.exists() {
            anyhow::bail!("kernel_path does not exist: {}", kernel_path.display());
        }

        let mem_mb = required_u16(&meta, "mem_mb")?;
        let bin_name = meta["bin_name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing bin_name"))?
            .to_string();

        // Resolve app/store paths. Prefer explicit fields; support legacy metadata
        // that stored the Nix store directory in `app_path` and omitted `store_path`.
        let (app_path, store_path) = resolve_rollback_app_paths(
            meta["app_path"].as_str(),
            meta["store_path"].as_str(),
            &bin_name,
        )?;
        if !Path::new(&app_path).exists() {
            anyhow::bail!("app_path does not exist: {app_path}");
        }

        // Require initramfs key; rebuild only when the recorded path is gone on disk.
        let initramfs_recorded = meta["initramfs"]
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
        // Restore the desired_state user env for deploy.env (F-04), with
        // secret:// refs resolved again.
        let mut user_env = crate::secrets::resolve_env_secrets(&desired_state_env(&meta))?;
        let run_as = super::RunAs::from_desired_state(meta.get("desired_state"));
        run_as.apply_env_defaults(&mut user_env);

        Ok(Self {
            meta,
            host_port,
            guest_port,
            kernel_path,
            mem_mb,
            bin_name,
            app_path,
            store_path,
            initramfs_path,
            user_env,
            run_as,
        })
    }

    /// Boot this generation from `russel_dir`: rewrite deploy.env, reserve
    /// ports, set up networking, boot, and wait for readiness. Only then write
    /// metadata and hand the processes to `state`.
    pub(crate) async fn launch(
        self,
        service_id: &str,
        russel_dir: &str,
        runner: &MicrovmRunner,
        state: &AppState,
        on_failure: FailedLaunch,
    ) -> anyhow::Result<()> {
        let Self {
            meta: old_meta,
            host_port,
            guest_port,
            kernel_path,
            mem_mb,
            bin_name,
            app_path,
            store_path,
            initramfs_path,
            user_env,
            run_as,
        } = self;

        // 3. Always rewrite deploy.env so legacy/stale APP values cannot stick.
        //    Include user env from desired_state (F-04: env restored on rollback).
        // Fail closed: never persist preferred_subnet (unregistered) identities that
        // may collide with another service after probe exhaustion.
        let alloc = subnet_for(service_id)?;
        let cfg_dir = format!("{}/cfg", russel_dir);
        let recorded_args: Vec<String> = desired(&old_meta, "args");
        let recorded_volumes: Vec<ResolvedVolume> = desired(&old_meta, "volumes");
        let recorded_extra_ports: Vec<(u16, u16)> =
            desired::<Vec<ExtraPortSpec>>(&old_meta, "extra_ports")
                .iter()
                .map(|p| (p.host, p.guest))
                .collect();
        super::write_deploy_env(
            &cfg_dir,
            &alloc.vm_ip,
            &alloc.host_ip,
            guest_port,
            &app_path,
            &user_env,
            &recorded_args,
            &recorded_volumes,
            run_as,
        )?;
        super::chown_managed_volumes_for_app(&recorded_volumes, run_as)?;

        // 4. Reserve port
        PortAllocator::reserve(service_id, host_port)?;
        for (i, (host, _)) in recorded_extra_ports.iter().enumerate() {
            if let Err(e) = PortAllocator::reserve(&extra_port_key(service_id, i), *host) {
                if on_failure == FailedLaunch::Destroy {
                    PortAllocator::release_service(service_id);
                }
                return Err(e);
            }
        }

        // Helper: tear down anything acquired after port reservation.
        async fn cleanup_launch_resources(
            service_id: &str,
            alloc: &SubnetAllocation,
            net_mode: MicrovmNetMode,
            runner: &MicrovmRunner,
            on_failure: FailedLaunch,
            vm_child: Option<tokio::process::Child>,
            aux: Vec<tokio::process::Child>,
        ) {
            match on_failure {
                FailedLaunch::Destroy => {
                    drop((vm_child, aux));
                    if let Err(e) = runner.destroy(service_id).await {
                        tracing::warn!(service_id, error = %e, "failed to destroy partially booted microVM");
                    }
                }
                FailedLaunch::Keep => {
                    for mut child in vm_child.into_iter().chain(aux) {
                        let _ = child.kill().await;
                    }
                    if let Err(e) = runner.stop(service_id).await {
                        tracing::warn!(service_id, error = %e, "failed to stop partially booted microVM");
                    }
                }
            }
            let _ = MicrovmNet::teardown(net_mode, alloc).await;
            if on_failure == FailedLaunch::Destroy {
                PortAllocator::release_service(service_id);
            }
        }

        let cpus: u8 = old_meta["cpus"]
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .unwrap_or(1)
            .clamp(1, 32);

        // 5–6. Network + boot; clean up on any failure after reservation
        let net_mode = MicrovmNetMode::for_host()?;
        let cfg_dir_path = PathBuf::from(&cfg_dir);
        let boot_result: anyhow::Result<(
            tokio::process::Child,
            Vec<tokio::process::Child>,
            MicrovmNet,
        )> = async {
            let net = MicrovmNet::setup(
                net_mode,
                service_id,
                &alloc,
                host_port,
                guest_port,
                &recorded_extra_ports,
            )
            .await?;
            let volume_fs =
                crate::microvm::volume_fs_mounts(Path::new(russel_dir), &recorded_volumes);
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
                    &net.attach,
                    &volume_fs,
                    mem_mb,
                    cpus,
                    &cfg_dir_path,
                )
                .await?;
            Ok((vm_child, virtiofsd_children, net))
        }
        .await;

        let (mut vm_child, virtiofsd_children, net) = match boot_result {
            Ok(v) => v,
            Err(e) => {
                cleanup_launch_resources(
                    service_id,
                    &alloc,
                    net_mode,
                    runner,
                    on_failure,
                    None,
                    Vec::new(),
                )
                .await;
                return Err(e.context("recorded microVM boot failed"));
            }
        };

        let v_pids: Vec<u32> = virtiofsd_children.iter().filter_map(|c| c.id()).collect();

        // 7. Readiness BEFORE marking deployed / writing durable success metadata
        // A VM that exits fails at once (#493). No settle window: a relaunch
        // or a cold-path restore replaces nothing that is still running, and
        // the supervisor sees a later crash.
        let outcome = ready::wait_until_ready(
            MicrovmNet::wait_ready(
                net_mode,
                service_id,
                &alloc,
                host_port,
                guest_port,
                MICROVM_READY_TIMEOUT,
            ),
            &mut vm_child,
            Duration::ZERO,
        )
        .await;
        if !matches!(outcome, ready::ReadyOutcome::Ready) {
            // Tear down the partial boot; do not report success for a dead service.
            // Read the console first: cleanup may remove the service dir.
            let error = match outcome {
                ready::ReadyOutcome::Exited(status) => ready::exited_error(
                    status,
                    &ready::console_tail(
                        &crate::paths::service_dir(service_id).join("console.log"),
                        ready::CONSOLE_TAIL_LINES,
                    ),
                ),
                _ => format!(
                    "recorded microVM not reachable on {}:{guest_port} within {}s",
                    alloc.vm_ip,
                    MICROVM_READY_TIMEOUT.as_secs()
                ),
            };
            cleanup_launch_resources(
                service_id,
                &alloc,
                net_mode,
                runner,
                on_failure,
                Some(vm_child),
                net.into_aux(virtiofsd_children),
            )
            .await;
            anyhow::bail!("{error}");
        }

        // 8. Write metadata + mark deployed only after readiness
        let mut meta = build_microvm_metadata(
            service_id,
            host_port,
            guest_port,
            &alloc.vm_ip,
            &alloc.host_ip,
            vm_child.id(),
            &v_pids,
            net.socat_pid(),
            &kernel_path.display().to_string(),
            &store_path,
            mem_mb,
            cpus,
            Some(&app_path),
            Some(&bin_name),
            Some(&initramfs_path.display().to_string()),
            None,
            None,
        );
        crate::metadata::record_effective_resources(
            &mut meta,
            crate::microvm::effective_memory_mb(mem_mb),
            Some(cpus),
        );
        net.record(&mut meta);
        let aux = net.into_aux(virtiofsd_children);

        // Keep the recorded generation's identity and desired_state (F-04) so
        // later rollbacks, updates, and restarts see the same deploy.
        if let Some(obj) = meta.as_object_mut() {
            for key in ["desired_state", "generation_id", "repo_url", "config_path"] {
                if let Some(value) = old_meta.get(key) {
                    obj.insert(key.into(), value.clone());
                }
            }
        }
        let metadata_path = format!("{}/metadata.json", russel_dir);
        if let Err(e) = write_metadata(&metadata_path, &meta) {
            cleanup_launch_resources(
                service_id,
                &alloc,
                net_mode,
                runner,
                on_failure,
                Some(vm_child),
                aux,
            )
            .await;
            return Err(e.context("recorded microVM metadata write failed"));
        }

        state.mark_deployed_with_aux(service_id, vm_child, aux, Some(host_port), Some(guest_port));

        Ok(())
    }
}

/// Resolve `app_path` / `store_path` for rollback.
///
/// Current metadata writes both. Legacy writers stored the Nix store directory
/// in `app_path` and omitted `store_path`, so APP would be `/nix/store/<hash>`
/// instead of `/nix/store/<hash>/bin/<bin>`. Reconstruct in that case.
pub(crate) fn resolve_rollback_app_paths(
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

/// Where a recorded path is while the service dir sits at its `.bak` name:
/// paths inside `live_dir` move with it, others stay put.
fn path_while_backed_up(path: &str, live_dir: &str, bak_dir: &str) -> PathBuf {
    match Path::new(path).strip_prefix(live_dir) {
        Ok(rel) => Path::new(bak_dir).join(rel),
        Err(_) => PathBuf::from(path),
    }
}

pub(crate) async fn attempt_container_rollback(
    service_id: &str,
    dirs: &ServiceDirs,
    containers: &ContainerRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    let (russel_dir, russel_bak) = (dirs.russel.as_str(), dirs.russel_bak.as_str());
    // 0. Validate backup metadata BEFORE renaming (F-16).
    let old_metadata_bak_path = format!("{}/metadata.json", russel_bak);
    let content = std::fs::read_to_string(&old_metadata_bak_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;

    let old_host_port = required_u16(&old_meta, "host_port")?;
    let old_guest_port = required_u16(&old_meta, "guest_port")?;
    let old_mem_mb = u16::try_from(old_meta["mem_mb"].as_u64().unwrap_or(512))
        .map_err(|_| anyhow::anyhow!("mem_mb out of u16 range in container metadata"))?;
    let old_bin_name = old_meta["bin_name"].as_str().unwrap_or("app");
    let old_rootfs_path = old_meta["rootfs_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing rootfs_path in container metadata"))?;
    // Fail before renaming anything if the previous generation was collected.
    // A rootfs inside the service dir is still under `.bak` at this point.
    if !path_while_backed_up(old_rootfs_path, russel_dir, russel_bak).exists() {
        anyhow::bail!(
            "previous rootfs_path no longer exists (garbage-collected?): {old_rootfs_path}"
        );
    }

    // F-04: restore podman_args from desired_state when present, falling back
    // to the legacy top-level "podman_args" field for older metadata.
    let strings = |v: Option<&serde_json::Value>| -> Option<Vec<String>> {
        Some(
            v?.as_array()?
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect(),
        )
    };
    let mut old_podman_args = strings(old_meta.pointer("/desired_state/podman_args"))
        .or_else(|| strings(old_meta.get("podman_args")))
        .unwrap_or_default();
    // Re-validate persisted podman_args; on failure drop them + warn.
    if !old_podman_args.is_empty()
        && let Err(e) = validate_podman_passthrough_args(&old_podman_args)
    {
        tracing::warn!(
            service_id,
            error = %e,
            "persisted podman_args failed re-validation — dropping for rollback"
        );
        old_podman_args.clear();
    }

    // F-04: restore user env from desired_state and re-resolve secret:// refs.
    let declared_env = desired_state_env(&old_meta);
    let mut user_env = crate::secrets::resolve_env_secrets(&declared_env)?;
    let run_as = super::RunAs::from_desired_state(old_meta.get("desired_state"));
    run_as.apply_env_defaults(&mut user_env);

    // All validations passed — rename safely, preserving managed volumes.
    crate::container::restore_backed_up_service_dir(Path::new(russel_dir), Path::new(russel_bak))
        .await?;

    let old_extra_ports: Vec<ExtraPortSpec> = desired(&old_meta, "extra_ports");

    PortAllocator::reserve(service_id, old_host_port)?;
    for (i, extra) in old_extra_ports.iter().enumerate() {
        if let Err(e) = PortAllocator::reserve(&extra_port_key(service_id, i), extra.host) {
            PortAllocator::release_service(service_id);
            return Err(e);
        }
    }

    let (env, secret_env) = super::env::split_secret_env(
        super::env::build_container_env(old_guest_port, &user_env),
        &declared_env,
    );

    let start_spec = ContainerStartSpec {
        service_id: service_id.to_string(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from(old_rootfs_path),
            entrypoint: PathBuf::from(format!("/bin/{}", old_bin_name)),
        },
        host_port: old_host_port,
        guest_port: old_guest_port,
        memory_mb: old_mem_mb,
        cpus: old_meta["cpus"].as_u64().and_then(|n| u8::try_from(n).ok()),
        env,
        secret_env,
        podman_args: old_podman_args,
        volumes: desired(&old_meta, "volumes"),
        extra_ports: old_extra_ports.iter().map(|p| (p.host, p.guest)).collect(),
        service_args: desired(&old_meta, "args"),
        userns_keep_id: run_as == super::RunAs::App,
        restart: desired(&old_meta, "restart"),
    };
    let running = containers.start(&start_spec).await?;

    // Do not mark deployed until the restored container is reachable.
    let ready = TapForwarder::wait_for_host_port(old_host_port, Duration::from_secs(10)).await;
    if !ready {
        if let Err(e) = crate::container::destroy_preserving_volumes(service_id).await {
            tracing::warn!(
                service_id,
                error = %e,
                "failed to destroy unready rolled-back container"
            );
        }
        PortAllocator::release_service(service_id);
        anyhow::bail!(
            "rolled-back container not reachable on host port {old_host_port} within 10s"
        );
    }

    state.mark_deployed_container(
        service_id,
        &running.container_id,
        Some(old_host_port),
        Some(old_guest_port),
    );

    let old_store_path = old_meta["store_path"]
        .as_str()
        .unwrap_or("/nix/store/unknown");
    let mut new_metadata = build_container_metadata(
        service_id,
        old_host_port,
        old_guest_port,
        old_store_path,
        &running.container_id,
        &running.container_name,
        &running.rootfs_path.to_string_lossy(),
        old_mem_mb,
        Some(old_bin_name),
        &start_spec.podman_args,
        None,
    );

    if let Some(cpus) = start_spec.cpus {
        new_metadata["cpus"] = serde_json::json!(cpus);
    }
    crate::metadata::record_effective_resources(
        &mut new_metadata,
        old_mem_mb,
        running.effective_cpus,
    );
    // Preserve desired_state from old metadata for future rollbacks (F-04).
    if let Some(ds) = old_meta.get("desired_state")
        && let Some(obj) = new_metadata.as_object_mut()
    {
        obj.insert("desired_state".into(), ds.clone());
    }
    write_metadata(format!("{}/metadata.json", russel_dir), &new_metadata)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rootfs_inside_the_service_dir_is_looked_up_under_bak() {
        // The cold path renames the service dir to .bak before booting the new
        // generation; the old rootfs is there when rollback checks it.
        assert_eq!(
            path_while_backed_up("/d/api/rootfs", "/d/api", "/d/api.bak"),
            PathBuf::from("/d/api.bak/rootfs")
        );
        assert_eq!(
            path_while_backed_up("/nix/store/x-rootfs", "/d/api", "/d/api.bak"),
            PathBuf::from("/nix/store/x-rootfs")
        );
        // A sibling that only shares the prefix string is not inside it.
        assert_eq!(
            path_while_backed_up("/d/api2/rootfs", "/d/api", "/d/api.bak"),
            PathBuf::from("/d/api2/rootfs")
        );
    }

    #[test]
    fn resolve_app_paths_both_present_with_bin_suffix() {
        let (app, store) =
            resolve_rollback_app_paths(Some("/nix/store/x/bin/app"), Some("/nix/store/x"), "app")
                .unwrap();
        assert_eq!(app, "/nix/store/x/bin/app");
        assert_eq!(store, "/nix/store/x");
    }

    #[test]
    fn resolve_app_paths_reconstructs_from_store_when_app_is_bare() {
        // Legacy writer stored the bare store dir in app_path.
        let (app, store) =
            resolve_rollback_app_paths(Some("/nix/store/x"), Some("/nix/store/x"), "app").unwrap();
        assert_eq!(app, "/nix/store/x/bin/app");
        assert_eq!(store, "/nix/store/x");
    }

    #[test]
    fn resolve_app_paths_legacy_app_only_with_bin() {
        let (app, store) =
            resolve_rollback_app_paths(Some("/nix/store/x/bin/app"), None, "app").unwrap();
        assert_eq!(app, "/nix/store/x/bin/app");
        assert_eq!(store, "/nix/store/x");
    }

    #[test]
    fn resolve_app_paths_legacy_app_only_bare_store() {
        let (app, store) = resolve_rollback_app_paths(Some("/nix/store/x"), None, "app").unwrap();
        assert_eq!(app, "/nix/store/x/bin/app");
        assert_eq!(store, "/nix/store/x");
    }

    #[test]
    fn resolve_app_paths_store_only() {
        let (app, store) = resolve_rollback_app_paths(None, Some("/nix/store/x"), "app").unwrap();
        assert_eq!(app, "/nix/store/x/bin/app");
        assert_eq!(store, "/nix/store/x");
    }

    #[test]
    fn resolve_app_paths_neither_is_error() {
        assert!(resolve_rollback_app_paths(None, None, "app").is_err());
    }

    #[test]
    fn desired_state_env_extracts_string_values_only() {
        let meta = serde_json::json!({
            "desired_state": {
                "env": {
                    "FOO": "bar",
                    "NUMBER": 42,
                    "NULL": null,
                    "PORT": "3000"
                }
            }
        });
        let env = desired_state_env(&meta);
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("PORT"), Some(&"3000".to_string()));
        assert!(!env.contains_key("NUMBER"));
        assert!(!env.contains_key("NULL"));
    }

    #[test]
    fn desired_state_env_empty_when_absent_or_malformed() {
        assert!(desired_state_env(&serde_json::json!({})).is_empty());
        assert!(desired_state_env(&serde_json::json!({"desired_state": {}})).is_empty());
        // env present but not an object → empty.
        assert!(desired_state_env(&serde_json::json!({"desired_state": {"env": "x"}})).is_empty());
    }

    struct Fixture {
        _tmp: TempDir,
        dirs: ServiceDirs,
    }

    fn fixture_with_metadata(metadata: Option<&str>) -> Fixture {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().display();
        let dirs = ServiceDirs {
            russel: format!("{root}/russel"),
            microvms: format!("{root}/microvms"),
            russel_bak: format!("{root}/russel.bak"),
            microvms_bak: format!("{root}/microvms.bak"),
            has_microvms: false,
        };
        std::fs::create_dir_all(&dirs.russel_bak).unwrap();
        if let Some(content) = metadata {
            std::fs::write(format!("{}/metadata.json", dirs.russel_bak), content).unwrap();
        }
        Fixture { _tmp: tmp, dirs }
    }

    /// Error from a microVM rollback whose backup holds `metadata`.
    async fn microvm_rollback_error(metadata: Option<&str>) -> String {
        let fx = fixture_with_metadata(metadata);
        attempt_microvm_rollback("svc", &fx.dirs, &MicrovmRunner::new(), &AppState::default())
            .await
            .unwrap_err()
            .to_string()
    }

    async fn container_rollback_error(metadata: Option<&str>) -> String {
        let fx = fixture_with_metadata(metadata);
        attempt_container_rollback(
            "svc",
            &fx.dirs,
            &ContainerRunner::new(),
            &AppState::default(),
        )
        .await
        .unwrap_err()
        .to_string()
    }

    #[tokio::test]
    async fn microvm_rollback_missing_metadata_fails_closed() {
        let err = microvm_rollback_error(None).await;
        assert!(
            err.contains("metadata.json") || err.contains("No such file"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn microvm_rollback_malformed_metadata_fails_closed() {
        let err = microvm_rollback_error(Some("not-json")).await;
        // serde_json parse failure surfaces directly (no field was missing).
        assert!(
            !err.contains("host_port") && !err.contains("kernel_path"),
            "expected a JSON parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn microvm_rollback_missing_fields_fail_closed() {
        for (metadata, field) in [
            ("{}", "host_port"),
            (r#"{"host_port": 8080}"#, "guest_port"),
            (r#"{"host_port": 8080, "guest_port": 3000}"#, "kernel_path"),
        ] {
            let err = microvm_rollback_error(Some(metadata)).await;
            assert!(err.contains(field), "{metadata}: unexpected error: {err}");
        }
    }

    #[tokio::test]
    async fn container_rollback_missing_metadata_fails_closed() {
        let err = container_rollback_error(None).await;
        assert!(
            err.contains("metadata.json") || err.contains("No such file"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn container_rollback_missing_rootfs_path_fails_closed() {
        let err =
            container_rollback_error(Some(r#"{"host_port": 8080, "guest_port": 3000}"#)).await;
        assert!(err.contains("rootfs_path"), "unexpected error: {err}");
    }
}
