//! Runtime-specific cold deploy paths (microVM and container).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use russel_core::{
    api::{DeployEvent, PortMapping},
    config::Russelfile,
};

use crate::{
    container::{
        CONTAINER_READY_TIMEOUT, ContainerStartSpec, LOG_TAIL_LINES, ReadyOutcome, RootfsSpec,
        container_log_path, default_base_dir, inspect_state, log_tail, not_ready_error,
        wait_until_ready, watch_container,
    },
    metadata::{
        build_container_metadata_with_gen, build_microvm_metadata_with_gen, write_metadata,
    },
    microvm::{BootOutput, KernelInfo, ready},
    network::{
        MICROVM_READY_TIMEOUT, MicrovmNet, MicrovmNetMode, SubnetAllocation, TapForwarder,
        subnet_for,
    },
    warm_pool::shared_warm_pool,
};

use super::env::{build_container_env, split_secret_env, validate_bin_name};
use super::pipeline::{DeployPipeline, DeployWorkload};

impl DeployPipeline {
    pub(crate) async fn deploy_microvm(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        port: &PortMapping,
        kernel_info: &KernelInfo,
        env: &HashMap<String, String>,
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
        generation_id: Option<&str>,
        desired_state: Option<&serde_json::Value>,
        volumes: &[russel_core::volumes::ResolvedVolume],
        using_package: bool,
        // How long the guest must stay up after the app answers: WATCH_WINDOW
        // on a cold replace, which already stopped the live generation; zero
        // otherwise (a first deploy, or dual-live, which watches at cutover).
        settle: Duration,
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        crate::microvm::check_volume_guest_paths(volumes)?;
        let run_as = super::RunAs::from_user(config.service.user.as_deref());
        let alloc: SubnetAllocation = subnet_for(service_id)?;
        tracing::info!(
            service_id,
            host = port.host,
            guest = port.guest,
            vm_ip = %alloc.vm_ip,
            "allocated"
        );

        let bin_name = config.service.bin_name_for_build(using_package).to_string();
        validate_bin_name(&bin_name)?;
        let mem_mb = config.service.memory.as_mebibytes();
        let app_path = format!("{}/bin/{bin_name}", store_path.display());

        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "create".into(),
                description: "Writing deploy config".into(),
            })
            .await;

        let cfg_dir = crate::paths::service_dir(service_id)
            .join("cfg")
            .display()
            .to_string();
        super::write_deploy_env(
            &cfg_dir,
            &alloc.vm_ip,
            &alloc.host_ip,
            port.guest,
            &app_path,
            env,
            &config.service.args,
            volumes,
            run_as,
        )?;
        crate::container::prepare_managed_volume_dirs(volumes).await?;
        super::chown_managed_volumes_for_app(volumes, run_as)?;

        // Agent initramfs is cached after first use — no app baked in.
        let initramfs_path = self.runner.build_agent_initramfs().await?;

        let create_ms = t.elapsed().as_millis();
        tracing::info!(
            service_id,
            create_ms,
            "deploy.env written, agent initramfs ready"
        );

        let _ = tx
            .send(DeployEvent::Progress {
                phase: "start".into(),
                description: "Setting up network + booting/restoring VM".into(),
            })
            .await;
        let net_mode = MicrovmNetMode::for_host()?;
        tracing::info!(
            service_id,
            net = net_mode.as_str(),
            "setting up network + booting VM"
        );

        let t_net = Instant::now();
        let extra_ports: Vec<(u16, u16)> = config.ports.iter().map(|p| (p.host, p.guest)).collect();
        let net = MicrovmNet::setup(
            net_mode,
            service_id,
            &alloc,
            port.host,
            port.guest,
            &extra_ports,
        )
        .await?;
        let volume_fs =
            crate::microvm::volume_fs_mounts(&crate::paths::service_dir(service_id), volumes);
        let network_ms = t_net.elapsed().as_millis();

        let t_start = Instant::now();
        let cfg_dir_path = PathBuf::from(&cfg_dir);
        let pool = shared_warm_pool();
        let BootOutput {
            mut vm_child,
            virtiofsd_children,
        } = pool
            .restore_or_boot(
                service_id,
                &kernel_info.path,
                &initramfs_path,
                &alloc,
                &net.attach,
                &volume_fs,
                mem_mb,
                config.service.cpus,
                &cfg_dir_path,
            )
            .await?;

        let metadata_path = crate::metadata::metadata_path(service_id)
            .display()
            .to_string();
        let v_pids: Vec<u32> = virtiofsd_children.iter().filter_map(|c| c.id()).collect();
        let mut meta = build_microvm_metadata_with_gen(
            service_id,
            port.host,
            port.guest,
            &alloc.vm_ip,
            &alloc.host_ip,
            vm_child.id(),
            &v_pids,
            net.socat_pid(),
            &kernel_info.path.display().to_string(),
            &store_path.display().to_string(),
            mem_mb,
            config.service.cpus,
            Some(&app_path),
            Some(&bin_name),
            Some(&initramfs_path.display().to_string()),
            generation_id,
            net.tap_id(&alloc),
        );
        net.record(&mut meta);

        // Merge desired_state for rollback + health restart (F-04).
        if let Some(ds) = desired_state
            && let Some(obj) = meta.as_object_mut()
        {
            obj.insert("desired_state".into(), ds.clone());
        }
        write_metadata(&metadata_path, &meta)?;

        // Marker dir for legacy discovery. The service dir already lists the
        // VM, so a root-owned /var/lib/microvms under an unprivileged ctrl
        // must not fail the deploy (#413).
        let microvms_marker = crate::paths::microvm_dir(service_id);
        if let Err(e) = std::fs::create_dir_all(&microvms_marker) {
            tracing::warn!(
                service_id,
                dir = %microvms_marker.display(),
                error = %e,
                "could not create microVM marker dir; set RUSSEL_MICROVMS_DIR or RUSSEL_DATA_DIR"
            );
        }

        let start_ms = t_start.elapsed().as_millis();
        tracing::info!(
            service_id,
            network_ms,
            start_ms,
            "network + VM booted/restored"
        );

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
        let outcome = ready::wait_until_ready(
            MicrovmNet::wait_ready(
                net.mode,
                service_id,
                &alloc,
                port.host,
                port.guest,
                MICROVM_READY_TIMEOUT,
            ),
            &mut vm_child,
            settle,
        )
        .await;
        let ready_ms = t.elapsed().as_millis();
        let console_path = crate::paths::service_dir(service_id).join("console.log");
        if let ready::ReadyOutcome::Exited(status) = outcome {
            anyhow::bail!(
                "{}",
                ready::exited_error(
                    status,
                    &ready::console_tail(&console_path, ready::CONSOLE_TAIL_LINES)
                )
            );
        }
        if matches!(outcome, ready::ReadyOutcome::TimedOut) {
            let console_log = console_path.display().to_string();
            let cfg_env = format!("{cfg_dir}/deploy.env");
            let mut detail = format!("VM not reachable in {}s", MICROVM_READY_TIMEOUT.as_secs());
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

        // H5: Also verify the host-side published port (socat bind may have
        // failed even when the guest is listening).
        // passt readiness already went through the host port.
        let host_up = net.mode == MicrovmNetMode::Passt
            || TapForwarder::wait_for_host_port(port.host, Duration::from_secs(2)).await;
        if !host_up {
            anyhow::bail!(
                "microVM guest reachable ({}:{}) but host port {} is not bound — \
                 socat / publish bind failure",
                alloc.vm_ip,
                port.guest,
                port.host
            );
        }

        Ok((
            DeployWorkload::Microvm {
                alloc,
                vm_child: Box::new(vm_child),
                virtiofsd_children,
                net_mode: net.mode,
                socat_child: Box::new(net.forwarder),
                extra_forwarders: net.extra_forwarders,
                initramfs_path,
                port: port.clone(),
            },
            create_ms,
            start_ms,
            network_ms,
            ready_ms,
        ))
    }

    pub(crate) async fn deploy_container(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        port: &PortMapping,
        podman_args: &[String],
        env: &HashMap<String, String>,
        tx: &tokio::sync::mpsc::Sender<DeployEvent>,
        generation_id: Option<&str>,
        desired_state: Option<&serde_json::Value>,
        volumes: &[russel_core::volumes::ResolvedVolume],
        using_package: bool,
        // Same as deploy_microvm: WATCH_WINDOW on a cold replace, else zero.
        settle: Duration,
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        tracing::info!(
            service_id,
            host = port.host,
            guest = port.guest,
            "allocated container port"
        );

        let bin_name = config.service.bin_name_for_build(using_package).to_string();
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
            debug: config.service.debug,
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
        let (plain_env, secret_env) =
            split_secret_env(build_container_env(port.guest, env), &config.service.env);
        let start_spec = ContainerStartSpec {
            service_id: service_id.to_string(),
            rootfs: prepared.clone(),
            host_port: port.host,
            guest_port: port.guest,
            memory_mb: mem_mb,
            cpus: Some(config.service.cpus),
            env: plain_env,
            secret_env,
            podman_args: podman_args.to_vec(),
            volumes: volumes.to_vec(),
            extra_ports: config.ports.iter().map(|p| (p.host, p.guest)).collect(),
            service_args: config.service.args.clone(),
            userns_keep_id: super::RunAs::from_user(config.service.user.as_deref())
                == super::RunAs::App,
            restart: config.service.restart.clone(),
        };
        let running = self.containers.start(&start_spec).await?;
        let start_ms = t_start.elapsed().as_millis();
        let network_ms = 0u128;
        tracing::info!(service_id, start_ms, "container started");

        let metadata_path = base_dir.join("metadata.json");
        let mut metadata = build_container_metadata_with_gen(
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
            generation_id,
        );

        metadata["cpus"] = serde_json::json!(config.service.cpus);

        // Merge desired_state for rollback + health restart (F-04).
        if let Some(ds) = desired_state
            && let Some(obj) = metadata.as_object_mut()
        {
            obj.insert("desired_state".into(), ds.clone());
        }
        write_metadata(&metadata_path, &metadata)?;

        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "ready".into(),
                description: "Waiting for the app to accept connections".into(),
            })
            .await;
        tracing::info!(
            service_id,
            host_port = port.host,
            "polling container readiness"
        );
        // The forwarder accepts on the host port whether or not the app is up,
        // so probe for the app and watch the container for an early exit (#462).
        let addr = TapForwarder::host_port_addr(port.host);
        let outcome = wait_until_ready(
            || crate::network::app_accepts(&addr),
            || inspect_state(&running.container_name),
            CONTAINER_READY_TIMEOUT,
        )
        .await;
        // A cold replace already stopped the live generation: a crash right
        // after answering must fail here, where the .bak restore covers it (#493).
        let outcome = match outcome {
            ReadyOutcome::Ready if !settle.is_zero() => {
                watch_container(&running.container_name, settle)
                    .await
                    .map_or(ReadyOutcome::Ready, ReadyOutcome::Died)
            }
            other => other,
        };
        let ready_ms = t.elapsed().as_millis();
        if outcome != ReadyOutcome::Ready {
            let tail = log_tail(&container_log_path(service_id), LOG_TAIL_LINES);
            tracing::warn!(
                service_id,
                ?outcome,
                ready_ms,
                "container did not become ready"
            );
            anyhow::bail!(
                "{}",
                not_ready_error(
                    &outcome,
                    port.host,
                    port.guest,
                    CONTAINER_READY_TIMEOUT,
                    &tail
                )
            );
        }
        tracing::info!(service_id, ready_ms, "container app accepting connections");

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
