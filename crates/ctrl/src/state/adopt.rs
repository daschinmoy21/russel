//! Startup reconcile / adoption of workloads observed from disk.

use std::time::Instant;

use russel_core::api::{ServiceStatus, VmState};
use russel_core::config::RuntimeKind;

use super::app::AppState;
use super::helpers::{parse_rfc3339_to_instant, push_capped};

impl AppState {
    /// Adopt a running microVM observed from disk metadata.
    ///
    /// No `Child` handles are created — the VM is observed-only.
    /// Stop/destroy already use disk metadata + `MicrovmRunner`.
    /// If the service already has live Child handles, does nothing.
    ///
    /// When `vm_pid` is Some, spawns a lightweight PID liveness supervisor
    /// that marks the service failed if the process disappears.
    pub fn adopt_running_microvm(
        &self,
        service_id: &str,
        host_port: u16,
        guest_port: u16,
        vm_pid: Option<u32>,
        deployed_at: Option<&str>,
    ) {
        let generation = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            if s.vm_process.is_some() {
                tracing::info!(
                    service_id = %service_id,
                    "skipping microvm adoption — already has live Child handle"
                );
                return;
            }
            s.status = ServiceStatus::Deployed;
            s.vm_state = VmState::Running;
            s.runtime = Some(RuntimeKind::Microvm);
            s.vm_pid = vm_pid;
            s.host_port = Some(host_port);
            s.guest_port = Some(guest_port);
            s.started_at = deployed_at
                .and_then(parse_rfc3339_to_instant)
                .unwrap_or_else(Instant::now);
            s.container_id = None;
            s.vm_process = None; // observed-only
            s.aux_processes.clear();
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.process_generation = s.process_generation.wrapping_add(1);
            push_capped(
                &mut s.logs,
                &format!(
                    "adopted running microvm from disk (host_port={host_port}, guest_port={guest_port})\n"
                ),
            );
            s.process_generation
        };
        // Spawn PID supervisor when we have a known PID.
        if vm_pid.is_some() {
            self.spawn_pid_supervisor(service_id.to_string(), generation);
        }
    }

    /// Adopt a running container observed from disk metadata.
    ///
    /// Spawns a container liveness supervisor (same as `mark_deployed_container`).
    /// If the service already has live Child handles, does nothing.
    pub fn adopt_running_container(
        &self,
        service_id: &str,
        container_id: &str,
        host_port: u16,
        guest_port: u16,
        deployed_at: Option<&str>,
    ) {
        let generation = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            if s.vm_process.is_some() {
                tracing::info!(
                    service_id = %service_id,
                    "skipping container adoption — already has live Child handle"
                );
                return;
            }
            s.status = ServiceStatus::Deployed;
            s.vm_state = VmState::Running;
            s.started_at = deployed_at
                .and_then(parse_rfc3339_to_instant)
                .unwrap_or_else(Instant::now);
            s.vm_pid = None;
            s.vm_process = None;
            s.container_id = Some(container_id.to_string());
            s.runtime = Some(RuntimeKind::Container);
            s.host_port = Some(host_port);
            s.guest_port = Some(guest_port);
            s.aux_processes.clear();
            push_capped(
                &mut s.logs,
                &format!(
                    "adopted running container from disk (id={container_id}, host_port={host_port}, guest_port={guest_port})\n"
                ),
            );
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.process_generation = s.process_generation.wrapping_add(1);
            s.process_generation
        };
        self.spawn_container_supervisor(
            service_id.to_string(),
            container_id.to_string(),
            generation,
        );
    }

    /// Mark a service stopped from on-disk metadata (no live processes).
    /// Used by startup reconcile so status APIs still return host_port/runtime.
    pub fn mark_stopped_from_disk(
        &self,
        service_id: &str,
        runtime: RuntimeKind,
        host_port: Option<u16>,
        guest_port: Option<u16>,
    ) {
        {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            // Do not clobber an in-memory live deployment. A live Child handle
            // is always authoritative; a container is only authoritative while
            // its vm_state is "running".
            if s.vm_process.is_some()
                || (s.container_id.is_some() && s.vm_state == VmState::Running)
            {
                return;
            }
            s.status = ServiceStatus::Stopped;
            s.vm_state = VmState::None;
            s.runtime = Some(runtime);
            s.host_port = host_port;
            s.guest_port = guest_port;
            s.vm_pid = None;
            s.vm_process = None;
            s.aux_processes.clear();
        }
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_stopped_from_disk");
        }
    }
}
