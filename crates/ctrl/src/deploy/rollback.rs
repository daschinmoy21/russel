//! Prior-runtime teardown and dual-live rollback helpers.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use russel_core::config::RuntimeKind;

use crate::{
    container::{
        ContainerRunner, ContainerStartSpec, PreparedRootfs, validate_podman_passthrough_args,
    },
    metadata::{
        build_container_metadata, build_microvm_metadata, prior_runtime_from_disk, write_metadata,
    },
    microvm::{BootOutput, MicrovmRunner},
    network::{PortAllocator, SubnetAllocation, TapForwarder, subnet_for},
    state::AppState,
    warm_pool::shared_warm_pool,
};

use super::env::shell_quote;

/// Resolve the prior runtime for a service before redeploy.
///
/// 1. Check on-disk metadata first (`prior_runtime_from_disk`).
/// 2. If no metadata (None), probe for a podman container named `russel-{service_id}`.
/// 3. If no container but the internal dirs exist, treat as legacy Microvm.
/// 4. Otherwise None (first deploy).
pub(crate) async fn resolve_prior_runtime(service_id: &str) -> Option<RuntimeKind> {
    if let Some(runtime) = prior_runtime_from_disk(service_id) {
        return Some(runtime);
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
        return Some(RuntimeKind::Container);
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

/// Kill + wait (with timeout) all old children so ports are free.
/// tokio `kill()` sends SIGKILL directly on Unix; there is no graceful phase.
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

pub(crate) async fn cleanup_failed_deploy(
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

pub(crate) async fn restore_backup_dirs(
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

pub(crate) async fn attempt_microvm_rollback(
    service_id: &str,
    russel_dir: &str,
    microvms_dir: &str,
    russel_bak: &str,
    microvms_bak: &str,
    has_microvms_backup: bool,
    runner: &MicrovmRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    // 0. Validate backup metadata BEFORE renaming (F-16: avoid unrecoverable
    //    half-restore when backup metadata is corrupt/missing).
    let old_metadata_bak_path = format!("{}/metadata.json", russel_bak);
    let content = std::fs::read_to_string(&old_metadata_bak_path)?;
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
    // Extract desired_state user env for deploy.env restoration (F-04).
    let user_env: HashMap<String, String> = old_meta
        .get("desired_state")
        .and_then(|ds| ds.get("env"))
        .and_then(|env_obj| {
            if let serde_json::Value::Object(map) = env_obj {
                let mut out = HashMap::new();
                for (k, v) in map {
                    if let Some(val) = v.as_str() {
                        out.insert(k.clone(), val.to_string());
                    }
                }
                Some(out)
            } else {
                None
            }
        })
        .unwrap_or_default();
    // Resolve secret:// refs in restored env
    let user_env = crate::secrets::resolve_env_secrets(&user_env)?;

    // All validations passed — now rename safely.
    // 1. Restore backup dirs
    tokio::fs::rename(russel_bak, russel_dir).await?;
    if has_microvms_backup {
        tokio::fs::rename(microvms_bak, microvms_dir).await?;
    }

    // 3. Always rewrite deploy.env so legacy/stale APP values cannot stick.
    //    Include user env from desired_state (F-04: env restored on rollback).
    // Fail closed: never persist preferred_subnet (unregistered) identities that
    // may collide with another service after probe exhaustion.
    let alloc = subnet_for(service_id)?;
    let cfg_dir = format!("{}/cfg", russel_dir);
    std::fs::create_dir_all(&cfg_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut deploy_env = format!(
        "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
        alloc.vm_ip,
        alloc.host_ip,
        guest_port,
        shell_quote(&app_path)
    );
    // Append user env vars from desired_state, shell-quoted (secrets resolved above).
    for (key, value) in &user_env {
        deploy_env.push_str(&format!("{}={}\n", key, shell_quote(value)));
    }
    let deploy_env_path = PathBuf::from(format!("{cfg_dir}/deploy.env"));
    crate::secrets::secure_write(&deploy_env_path, deploy_env.as_bytes(), "deploy.env")?;

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

    let rollback_cpus: u8 = old_meta["cpus"]
        .as_u64()
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(1)
        .clamp(1, 32);

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
                rollback_cpus,
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
    let mut meta = build_microvm_metadata(
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
        rollback_cpus,
        Some(&app_path),
        Some(&bin_name),
        Some(&initramfs_path.display().to_string()),
    );

    // Preserve desired_state from old metadata for future rollbacks (F-04).
    if let Some(ds) = old_meta.get("desired_state")
        && let Some(obj) = meta.as_object_mut()
    {
        obj.insert("desired_state".into(), ds.clone());
    }
    let metadata_path = format!("{}/metadata.json", russel_dir);
    if let Err(e) = write_metadata(&metadata_path, &meta) {
        let mut aux = vec![socat_child];
        aux.extend(virtiofsd_children);
        cleanup_rollback_resources(service_id, &alloc, runner, Some(vm_child), Some(aux)).await;
        return Err(e.context("microVM rollback metadata write failed"));
    }

    let mut aux = vec![socat_child];
    aux.extend(virtiofsd_children);
    state.mark_deployed_with_aux(service_id, vm_child, aux, Some(host_port), Some(guest_port));

    Ok(())
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

pub(crate) async fn attempt_container_rollback(
    service_id: &str,
    russel_dir: &str,
    russel_bak: &str,
    containers: &ContainerRunner,
    state: &AppState,
) -> anyhow::Result<()> {
    // 0. Validate backup metadata BEFORE renaming (F-16).
    let old_metadata_bak_path = format!("{}/metadata.json", russel_bak);
    let content = std::fs::read_to_string(&old_metadata_bak_path)?;
    let old_meta: serde_json::Value = serde_json::from_str(&content)?;

    let old_host_port = u16::try_from(
        old_meta["host_port"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing host_port in container metadata"))?,
    )
    .map_err(|_| anyhow::anyhow!("host_port out of u16 range in container metadata"))?;
    let old_guest_port = u16::try_from(
        old_meta["guest_port"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing guest_port in container metadata"))?,
    )
    .map_err(|_| anyhow::anyhow!("guest_port out of u16 range in container metadata"))?;
    let old_mem_mb = u16::try_from(old_meta["mem_mb"].as_u64().unwrap_or(512))
        .map_err(|_| anyhow::anyhow!("mem_mb out of u16 range in container metadata"))?;
    let old_bin_name = old_meta["bin_name"].as_str().unwrap_or("app");
    let old_rootfs_path = old_meta["rootfs_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing rootfs_path in container metadata"))?;

    // F-04: restore podman_args from desired_state when present, falling back
    // to the legacy top-level "podman_args" field for older metadata.
    let mut old_podman_args: Vec<String> = old_meta
        .get("desired_state")
        .and_then(|ds| ds.get("podman_args"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| {
            // Legacy: podman_args at top level
            old_meta
                .get("podman_args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        });
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

    // F-04: restore user env from desired_state (resolved secrets).
    let user_env: HashMap<String, String> = old_meta
        .get("desired_state")
        .and_then(|ds| ds.get("env"))
        .and_then(|env_obj| {
            if let serde_json::Value::Object(map) = env_obj {
                let mut out = HashMap::new();
                for (k, v) in map {
                    if let Some(val) = v.as_str() {
                        out.insert(k.clone(), val.to_string());
                    }
                }
                Some(out)
            } else {
                None
            }
        })
        .unwrap_or_default();
    // Resolve secret:// refs
    let user_env = crate::secrets::resolve_env_secrets(&user_env)?;

    // All validations passed — rename safely.
    tokio::fs::rename(russel_bak, russel_dir).await?;

    PortAllocator::reserve(service_id, old_host_port)?;

    // Build container env: PORT first, then user env (PORT filtered out).
    let mut env: Vec<(String, String)> = vec![("PORT".to_string(), old_guest_port.to_string())];
    for (key, value) in &user_env {
        if key != "PORT" {
            env.push((key.clone(), value.clone()));
        }
    }

    let start_spec = ContainerStartSpec {
        service_id: service_id.to_string(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from(old_rootfs_path),
            entrypoint: PathBuf::from(format!("/bin/{}", old_bin_name)),
        },
        host_port: old_host_port,
        guest_port: old_guest_port,
        memory_mb: old_mem_mb,
        env,
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
        &old_podman_args,
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
