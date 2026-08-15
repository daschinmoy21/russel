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
    container::{ContainerStartSpec, RootfsSpec, default_base_dir},
    metadata::{
        build_container_metadata_with_gen, build_microvm_metadata_with_gen, write_metadata,
    },
    microvm::{BootOutput, KernelInfo},
    network::{SubnetAllocation, TapForwarder, subnet_for},
    warm_pool::shared_warm_pool,
};

use super::env::{build_container_env, validate_bin_name};
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
    ) -> anyhow::Result<(DeployWorkload, u128, u128, u128, u128)> {
        let alloc: SubnetAllocation = subnet_for(service_id)?;
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

        let t = Instant::now();
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "create".into(),
                description: "Writing deploy config".into(),
            })
            .await;

        let cfg_dir = format!("/var/lib/russel/{}/cfg", service_id);
        super::write_deploy_env(
            &cfg_dir,
            &alloc.vm_ip,
            &alloc.host_ip,
            port.guest,
            &app_path,
            env,
        )?;

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
                config.service.cpus,
                &cfg_dir_path,
            )
            .await?;

        let metadata_path = format!("/var/lib/russel/{}/metadata.json", service_id);
        let v_pids: Vec<u32> = virtiofsd_children.iter().filter_map(|c| c.id()).collect();
        let mut meta = build_microvm_metadata_with_gen(
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
            config.service.cpus,
            Some(&app_path),
            Some(&bin_name),
            Some(&initramfs_path.display().to_string()),
            generation_id,
            Some(&alloc.tap_id),
        );

        // Merge desired_state for rollback + health restart (F-04).
        if let Some(ds) = desired_state
            && let Some(obj) = meta.as_object_mut()
        {
            obj.insert("desired_state".into(), ds.clone());
        }
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

        // H5: Also verify the host-side published port (socat bind may have
        // failed even when the guest is listening).
        let host_up = TapForwarder::wait_for_host_port(port.host, Duration::from_secs(2)).await;
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
