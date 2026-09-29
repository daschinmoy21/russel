//! Service lifecycle transitions, process ownership, and supervisors.

use std::time::{Duration, Instant};

use russel_core::api::{LogsResponse, ServiceStatus, StatusResponse, VmState};
use russel_core::config::RuntimeKind;
use tokio::process::Child;

use super::app::{AppState, LifecycleClaim, ServiceState, SupervisePoll, Supervision};
use super::helpers::{pid_is_alive, push_capped, read_tail_of_file};

impl AppState {
    /// Mark a service as building. Rejects if the service already exists in a
    /// conflicting lifecycle state (building/stopping/destroying). Creates a
    /// new entry if the service does not yet exist.
    pub fn mark_building(&self, service_id: &str) -> anyhow::Result<()> {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get(service_id)
            && (s.status == ServiceStatus::Building
                || s.status == ServiceStatus::Stopping
                || s.status == ServiceStatus::Destroying)
        {
            anyhow::bail!(
                "service {} is already in lifecycle state '{}'",
                service_id,
                s.status
            );
        }
        let s = inner.services.entry(service_id.to_string()).or_default();
        // Capture prior state for failure recovery during redeployment.
        s.prebuild_status = Some(s.status);
        s.prebuild_vm_state = Some(s.vm_state);
        s.status = ServiceStatus::Building;
        s.restarts = 0;
        s.restart_streak = 0;
        // Reset stale vm_state: "failed"/"none" → "pending", preserve "running"
        // for an existing VM process, set new entries to "pending".
        match s.vm_state {
            VmState::Failed | VmState::None => s.vm_state = VmState::Pending,
            VmState::Running if s.vm_process.is_none() => s.vm_state = VmState::Pending,
            _ => {}
        }
        Ok(())
    }

    /// Atomically register the VM and its auxiliary children.
    /// Replaces separate mark_deployed + store_aux_process calls with one lock acquisition.
    ///
    /// Spawns a background supervisor that marks the service failed if any
    /// tracked child exits unexpectedly (issue #32).
    ///
    /// When `host_port` / `guest_port` are provided they are stored on the
    /// in-memory service state so status and health probes do not depend solely
    /// on disk metadata.
    pub fn mark_deployed_with_aux(
        &self,
        service_id: &str,
        vm_child: Child,
        aux_children: Vec<Child>,
        host_port: Option<u16>,
        guest_port: Option<u16>,
    ) {
        let generation = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            s.status = ServiceStatus::Deployed;
            s.vm_state = VmState::Running;
            s.started_at = Instant::now();
            s.vm_pid = vm_child.id();
            s.vm_process = Some(vm_child);
            s.container_id = None;
            s.runtime = Some(RuntimeKind::Microvm);
            s.aux_processes = aux_children;
            if let Some(p) = host_port.filter(|p| *p > 0) {
                s.host_port = Some(p);
            }
            if let Some(p) = guest_port.filter(|p| *p > 0) {
                s.guest_port = Some(p);
            }
            // Deploy succeeded — clear prebuild snapshot.
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.process_generation = s.process_generation.wrapping_add(1);
            s.process_generation
        };
        self.spawn_process_supervisor(service_id.to_string(), generation);

        // Best-effort catalog update.
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_deployed_with_aux");
        }
    }

    /// Start the liveness supervisor for a workload that stays in place
    /// across a generation bump (failed redeploy, aborted stop).
    fn spawn_supervisor(&self, service_id: &str, generation: u64, supervision: Supervision) {
        match supervision {
            Supervision::Process => {
                self.spawn_process_supervisor(service_id.to_string(), generation);
            }
            Supervision::Container(container_id) => {
                // The container outlived a failed redeploy or an aborted stop;
                // its restart count so far is not a new crash.
                self.spawn_container_supervisor(
                    service_id.to_string(),
                    container_id,
                    generation,
                    None,
                );
            }
            Supervision::Pid => self.spawn_pid_supervisor(service_id.to_string(), generation),
        }
    }

    fn spawn_process_supervisor(&self, service_id: String, generation: u64) {
        // Skip supervisor when no tokio runtime is active (e.g. sync unit
        // tests); the test process is the only observer.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so try_wait is not raced with spawn.
            interval.tick().await;
            loop {
                interval.tick().await;
                match state.poll_supervised_children(&service_id, generation) {
                    SupervisePoll::Running => {}
                    SupervisePoll::Stopped => break,
                    SupervisePoll::Exited(msg) => {
                        tracing::warn!(
                            service_id = %service_id,
                            generation,
                            reason = %msg,
                            "supervised process exited unexpectedly"
                        );
                        state
                            .handle_unexpected_process_exit(&service_id, generation, msg)
                            .await;
                        crate::restart::on_unexpected_exit(&state, &service_id);
                        break;
                    }
                }
            }
        });
    }

    /// Lightweight PID liveness supervisor for adopted microVMs.
    ///
    /// Polls `/proc/{pid}` every 5s. When the PID disappears, marks the
    /// service failed if the generation still matches.
    pub(super) fn spawn_pid_supervisor(&self, service_id: String, generation: u64) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            // Initial settle delay.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                // Check generation ownership.
                let pid = {
                    let inner = state.lock_inner();
                    let Some(s) = inner.services.get(&service_id) else {
                        return;
                    };
                    if s.process_generation != generation {
                        return;
                    }
                    if s.status != ServiceStatus::Deployed || s.vm_state != VmState::Running {
                        return;
                    }
                    s.vm_pid
                };
                let Some(pid) = pid else {
                    return;
                };
                if !pid_is_alive(pid) {
                    tracing::warn!(
                        service_id = %service_id,
                        pid,
                        generation,
                        "adopted microVM PID disappeared"
                    );
                    if state.mark_failed_if_generation(
                        &service_id,
                        generation,
                        format!("adopted microVM PID {pid} no longer alive"),
                    ) {
                        crate::restart::on_unexpected_exit(&state, &service_id);
                    }
                    return;
                }
            }
        });
    }

    fn poll_supervised_children(&self, service_id: &str, generation: u64) -> SupervisePoll {
        let mut inner = self.lock_inner();
        let Some(s) = inner.services.get_mut(service_id) else {
            return SupervisePoll::Stopped;
        };
        if s.process_generation != generation {
            return SupervisePoll::Stopped;
        }
        if s.status != ServiceStatus::Deployed || s.vm_state != VmState::Running {
            return SupervisePoll::Stopped;
        }

        if let Some(ref mut child) = s.vm_process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return SupervisePoll::Exited(format!("cloud-hypervisor exited ({status})"));
                }
                Ok(None) => {}
                Err(e) => {
                    return SupervisePoll::Exited(format!("cloud-hypervisor wait error: {e}"));
                }
            }
        } else {
            // No VM handle while marked running under this generation — stop.
            return SupervisePoll::Stopped;
        }

        for (idx, child) in s.aux_processes.iter_mut().enumerate() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return SupervisePoll::Exited(format!(
                        "auxiliary process #{idx} exited ({status})"
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    return SupervisePoll::Exited(format!(
                        "auxiliary process #{idx} wait error: {e}"
                    ));
                }
            }
        }

        SupervisePoll::Running
    }

    async fn handle_unexpected_process_exit(
        &self,
        service_id: &str,
        generation: u64,
        reason: String,
    ) {
        let (vm, aux) = {
            let mut inner = self.lock_inner();
            let Some(s) = inner.services.get_mut(service_id) else {
                return;
            };
            if s.process_generation != generation {
                return;
            }
            s.process_generation = s.process_generation.wrapping_add(1);
            let vm = s.vm_process.take();
            let aux = std::mem::take(&mut s.aux_processes);
            s.vm_pid = None;
            s.status = ServiceStatus::Failed;
            s.vm_state = VmState::Failed;
            push_capped(&mut s.logs, &format!("PROCESS EXIT: {reason}\n"));
            (vm, aux)
        };

        if let Some(mut child) = vm {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        for mut child in aux {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// Mark a container deployment as running (no VM child processes).
    /// Spawns the container exit watcher (see `container_watch`).
    ///
    /// When `host_port` / `guest_port` are provided they are stored on the
    /// in-memory service state so status and health probes do not depend solely
    /// on disk metadata.
    pub fn mark_deployed_container(
        &self,
        service_id: &str,
        container_id: &str,
        host_port: Option<u16>,
        guest_port: Option<u16>,
    ) {
        let generation = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            s.status = ServiceStatus::Deployed;
            s.vm_state = VmState::Running;
            s.started_at = Instant::now();
            s.vm_pid = None;
            s.vm_process = None;
            s.container_id = Some(container_id.to_string());
            s.runtime = Some(RuntimeKind::Container);
            s.aux_processes.clear();
            if let Some(p) = host_port.filter(|p| *p > 0) {
                s.host_port = Some(p);
            }
            if let Some(p) = guest_port.filter(|p| *p > 0) {
                s.guest_port = Some(p);
            }
            push_capped(
                &mut s.logs,
                &format!("container running (id: {container_id})\n"),
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
            // Freshly started by this deploy: any restart is a crash.
            Some(0),
        );

        // Best-effort catalog update.
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_deployed_container");
        }
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let restored = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            Self::apply_failure_transition(s, &error)
        };

        if let Some((generation, supervision)) = restored {
            self.spawn_supervisor(service_id, generation, supervision);
        }

        // Best-effort catalog update.
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_failed");
        }
    }

    /// Atomically mark a service failed only if the current generation matches
    /// `expected_generation` and the service is still deployed/running.
    ///
    /// Returns `true` if the service was marked failed, `false` if the
    /// generation, status, or service changed (caller should exit quietly).
    ///
    /// Used by the container liveness supervisor to avoid marking a newer
    /// deployment generation as failed after a slow `podman inspect` await.
    pub fn mark_failed_if_generation(
        &self,
        service_id: &str,
        expected_generation: u64,
        error: String,
    ) -> bool {
        let restored = {
            let mut inner = self.lock_inner();
            let Some(s) = inner.services.get_mut(service_id) else {
                return false;
            };
            if s.process_generation != expected_generation {
                return false;
            }
            if s.status != ServiceStatus::Deployed || s.vm_state != VmState::Running {
                return false;
            }
            Self::apply_failure_transition(s, &error)
        };

        if let Some((generation, supervision)) = restored {
            self.spawn_supervisor(service_id, generation, supervision);
        }

        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_failed_if_generation");
        }

        true
    }

    /// Apply the failure state transition to a ServiceState in-place.
    ///
    /// If a prebuild snapshot exists (previous deployment still running),
    /// restores that state and returns the new generation plus the supervisor
    /// that fits the restored workload. Otherwise sets standard
    /// `failed`/`failed` state.
    pub(super) fn apply_failure_transition(
        s: &mut ServiceState,
        error: &str,
    ) -> Option<(u64, Supervision)> {
        if s.prebuild_vm_state == Some(VmState::Running) {
            let prev_status = s.prebuild_status.take().unwrap_or_default();
            let prev_vm_state = s.prebuild_vm_state.take().unwrap_or_default();
            s.status = prev_status;
            s.vm_state = prev_vm_state;
            // Bump generation to restart supervision on the restored deployment.
            s.process_generation = s.process_generation.wrapping_add(1);
            let g = s.process_generation;
            push_capped(
                &mut s.logs,
                &format!("BUILD FAILED (previous deployment preserved): {}\n", error),
            );
            Supervision::for_service(s).map(|supervision| (g, supervision))
        } else {
            // No prior VM to restore — standard failure.
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.status = ServiceStatus::Failed;
            s.vm_state = VmState::Failed;
            s.vm_pid = None;
            push_capped(&mut s.logs, error);
            push_capped(&mut s.logs, "\n");
            None
        }
    }

    /// Ensure a service entry exists with minimal state.
    /// Used to rehydrate disk-discovered VMs after restart so lifecycle
    /// endpoints (status, stop, destroy) can find them.
    pub fn ensure_service(&self, service_id: &str) {
        // Fast path: already present. Hold the mutex only for the contains_key
        // check — never across the metadata disk read below.
        {
            let inner = self.lock_inner();
            if inner.services.contains_key(service_id) {
                return;
            }
        }

        // Filesystem I/O outside the global mutex (same pattern as `status` /
        // `logs` / `build_catalog`). A concurrent insert between this read and
        // the re-acquire is benign: `entry` then no-ops and this thread's disk
        // read is simply discarded.
        let disk_meta = crate::metadata::load_metadata_from_disk(service_id);

        let mut inner = self.lock_inner();
        inner
            .services
            .entry(service_id.to_string())
            .or_insert_with(|| {
                let mut s = ServiceState {
                    status: ServiceStatus::Stopped,
                    ..Default::default()
                };
                if let Some(meta) = disk_meta {
                    s.runtime = meta.runtime;
                    if let Some(id) = meta.container_id {
                        s.container_id = Some(id);
                    }
                }
                s
            });
    }

    pub fn runtime_for_service(&self, service_id: &str) -> Option<RuntimeKind> {
        let inner = self.lock_inner();
        inner.services.get(service_id).and_then(|s| s.runtime)
    }

    pub fn attach_flake_path(&self, service_id: &str, flake_path: std::path::PathBuf) {
        let mut inner = self.lock_inner();
        let s = inner.services.entry(service_id.to_string()).or_default();
        push_capped(
            &mut s.logs,
            &format!(
                "using flake at {} (service_id: {})\n",
                flake_path.display(),
                service_id
            ),
        );
        s.flake_path = Some(flake_path);
    }

    /// Take processes for a service. Returns None if the service doesn't exist.
    /// Clears the prebuild snapshot since the old VM is committed for replacement.
    pub fn take_processes(&self, service_id: &str) -> Option<(Option<Child>, Vec<Child>)> {
        let mut inner = self.lock_inner();
        let s = inner.services.get_mut(service_id)?;
        // Invalidate the supervisor for the previous generation.
        s.process_generation = s.process_generation.wrapping_add(1);
        let vm = s.vm_process.take();
        let aux = std::mem::take(&mut s.aux_processes);
        s.vm_pid = None;
        // Processes claimed — old VM is committed for replacement.
        s.prebuild_status = None;
        s.prebuild_vm_state = None;
        Some((vm, aux))
    }

    /// Atomically begin a lifecycle operation: mark status and invalidate the
    /// process supervisor. Process handles stay in state until the success path
    /// calls [`take_processes_for_reap`]; on failure call
    /// [`abort_lifecycle_operation`] to restore prior status and re-supervise.
    ///
    /// Distinguishes NotFound (no such service) from Busy (conflicting op).
    /// Same-op re-entry (stop-while-stopping, destroy-while-destroying) is Busy
    /// so concurrent claims cannot race two cleanups. Destroy may still
    /// supersede a stuck stop. Each claim returns a `claim_generation` that
    /// abort must present so a superseded claim cannot restore.
    pub fn begin_lifecycle_operation(
        &self,
        service_id: &str,
        status: ServiceStatus,
        vm_state: VmState,
    ) -> LifecycleClaim {
        let mut inner = self.lock_inner();
        let Some(s) = inner.services.get_mut(service_id) else {
            return LifecycleClaim::NotFound;
        };
        let busy = match s.status {
            ServiceStatus::Building | ServiceStatus::Destroying => true,
            ServiceStatus::Stopping => status != ServiceStatus::Destroying,
            _ => false,
        };
        if busy {
            return LifecycleClaim::Busy;
        }
        let prior_status = s.status;
        let prior_vm_state = s.vm_state;
        s.status = status;
        s.vm_state = vm_state;
        // Invalidate supervisor so intentional stop/destroy is not reported as crash.
        // Children remain owned by this ServiceState until take_processes_for_reap.
        // claim_generation identity is this bumped process_generation.
        s.process_generation = s.process_generation.wrapping_add(1);
        s.prebuild_status = None;
        s.prebuild_vm_state = None;
        LifecycleClaim::Claimed {
            prior_status,
            prior_vm_state,
            claim_generation: s.process_generation,
        }
    }

    /// Take process handles for reaping after a successful stop/destroy.
    ///
    /// Does not bump `process_generation` — [`begin_lifecycle_operation`] already
    /// invalidated the supervisor. Returns `None` if the service is gone.
    pub fn take_processes_for_reap(&self, service_id: &str) -> Option<(Option<Child>, Vec<Child>)> {
        let mut inner = self.lock_inner();
        let s = inner.services.get_mut(service_id)?;
        let vm = s.vm_process.take();
        let aux = std::mem::take(&mut s.aux_processes);
        s.vm_pid = None;
        Some((vm, aux))
    }

    /// Abort a failed lifecycle op: restore prior status/vm_state, re-bump
    /// generation, and re-spawn a process supervisor if children remain.
    ///
    /// Restore runs only when `claim_generation` still matches the service's
    /// current `process_generation` **and** `expected_status` still matches.
    /// A newer same-status re-entry bumps generation so an older abort cannot
    /// restore `deployed` over the active claim; a completed stop/destroy
    /// advances status so a late abort cannot resurrect a phantom running
    /// snapshot without process handles.
    pub fn abort_lifecycle_operation(
        &self,
        service_id: &str,
        claim_generation: u64,
        expected_status: ServiceStatus,
        prior_status: ServiceStatus,
        prior_vm_state: VmState,
    ) {
        let needs_supervisor = {
            let mut inner = self.lock_inner();
            let Some(s) = inner.services.get_mut(service_id) else {
                return;
            };
            if s.process_generation != claim_generation {
                tracing::debug!(
                    service_id = %service_id,
                    claim_generation,
                    current_generation = s.process_generation,
                    "skipping abort restore; claim superseded by newer lifecycle op"
                );
                return;
            }
            if s.status != expected_status {
                tracing::debug!(
                    service_id = %service_id,
                    current = %s.status,
                    expected = %expected_status,
                    "skipping abort restore; lifecycle status already advanced"
                );
                return;
            }
            s.status = prior_status;
            s.vm_state = prior_vm_state;
            s.process_generation = s.process_generation.wrapping_add(1);
            let generation = s.process_generation;
            Supervision::for_service(s).map(|supervision| (generation, supervision))
        };
        if let Some((generation, supervision)) = needs_supervisor {
            self.spawn_supervisor(service_id, generation, supervision);
        }
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after abort_lifecycle_operation");
        }
    }

    pub fn set_status(&self, service_id: &str, status: ServiceStatus, vm_state: VmState) {
        {
            let mut inner = self.lock_inner();
            if let Some(s) = inner.services.get_mut(service_id) {
                s.status = status;
                s.vm_state = vm_state;
            }
        }
        // Catalog write after releasing the state lock (Mutex is not reentrant).
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after set_status");
        }
    }

    pub fn remove_service(&self, service_id: &str) {
        {
            let mut inner = self.lock_inner();
            inner.services.remove(service_id);
        }
        // Catalog write after releasing the state lock (Mutex is not reentrant).
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after remove_service");
        }
    }

    /// Move in-memory process ownership from a generation runtime key to the
    /// stable service id after a zero-downtime promote.
    ///
    /// The candidate's supervisor watched `from` and the old generation's
    /// watched `to`; both must end, and the promoted workload needs its own
    /// (#548). The new generation is above both so neither old one matches.
    pub fn rekey_service(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let respawn = {
            let mut inner = self.lock_inner();
            let Some(mut s) = inner.services.remove(from) else {
                return;
            };
            // Drop residual entry for `to` (old generation already drained).
            let old_generation = inner
                .services
                .remove(to)
                .map_or(0, |old| old.process_generation);
            s.process_generation = s.process_generation.max(old_generation).wrapping_add(1);
            push_capped(&mut s.logs, &format!("rekeyed generation {from} -> {to}\n"));
            let respawn = Supervision::for_service(&s).map(|sup| (s.process_generation, sup));
            inner.services.insert(to.to_string(), s);
            respawn
        };
        match respawn {
            // Started by this deploy: any restart from here on is a crash.
            Some((generation, Supervision::Container(container_id))) => {
                self.spawn_container_supervisor(to.to_string(), container_id, generation, Some(0))
            }
            Some((generation, supervision)) => self.spawn_supervisor(to, generation, supervision),
            None => {}
        }
    }

    /// Release ownership of all tracked child processes without killing them.
    ///
    /// Used on control-plane shutdown so `kill_on_drop` Child destructors do
    /// not tear down live Cloud Hypervisor / virtiofsd / socat workloads.
    /// Processes are intentionally leaked from the Rust side; the OS continues
    /// to run them until an operator destroys the service or reaps them.
    ///
    /// Returns the number of services that had process handles detached.
    pub fn detach_all_processes(&self) -> usize {
        let mut inner = self.lock_inner();
        let mut detached = 0usize;
        for (service_id, s) in inner.services.iter_mut() {
            let mut had = false;
            if let Some(child) = s.vm_process.take() {
                std::mem::forget(child);
                had = true;
            }
            for child in std::mem::take(&mut s.aux_processes) {
                std::mem::forget(child);
                had = true;
            }
            if had {
                // Handles are gone but the VM may still be running on disk.
                // Keep the service id so a future reconciler can re-adopt it;
                // status reflects that this controller no longer owns the PIDs.
                s.vm_pid = None;
                if s.status == ServiceStatus::Deployed || s.vm_state == VmState::Running {
                    s.status = ServiceStatus::Detached;
                    s.vm_state = VmState::Orphaned;
                }
                tracing::info!(
                    service_id = %service_id,
                    "detached workload processes for control-plane shutdown"
                );
                detached += 1;
            }
        }
        detached
    }

    pub fn status(&self, service_id: &str) -> Option<StatusResponse> {
        // Snapshot needed fields under the lock, then release before the disk
        // read (same pattern as `logs`). `started_at.elapsed()` is wall-clock
        // and cheap, so it is computed inside the critical section alongside
        // the `vm_state` check it depends on.
        let (status, vm_state, runtime, host_port, guest_port, uptime_seconds, restarts) = {
            let inner = self.lock_inner();
            let s = inner.services.get(service_id)?;
            // Only count wall-clock uptime while the workload is actually running.
            // Stopped/failed services must not report a growing timer from started_at.
            let uptime_seconds = if s.vm_state == VmState::Running {
                s.started_at.elapsed().as_secs()
            } else {
                0
            };
            (
                s.status.as_str().to_string(),
                s.vm_state.as_str().to_string(),
                s.runtime,
                s.host_port,
                s.guest_port,
                uptime_seconds,
                (s.restarts > 0).then_some(s.restarts),
            )
        };

        let disk = crate::metadata::load_metadata_from_disk(service_id);
        let route_host = disk
            .as_ref()
            .and_then(|m| m.ingress_host.clone())
            .or_else(|| {
                crate::traefik::TraefikFileIngress::from_env().host_from_dynamic_config(service_id)
            });
        Some(StatusResponse {
            service_id: service_id.to_string(),
            status,
            vm_state,
            uptime_seconds,
            runtime: runtime.or_else(|| disk.as_ref().and_then(|m| m.runtime)),
            host_port: host_port.or_else(|| disk.as_ref().and_then(|m| m.host_port)),
            guest_port: guest_port.or_else(|| disk.as_ref().and_then(|m| m.guest_port)),
            route_host,
            restarts,
        })
    }

    pub fn logs(&self, service_id: &str) -> Option<LogsResponse> {
        // Snapshot needed fields under the lock, then release before file I/O.
        let (logs_snapshot, runtime) = {
            let inner = self.lock_inner();
            let s = inner.services.get(service_id)?;
            (s.logs.clone(), s.runtime)
        };

        let runtime = match runtime {
            Some(rt) => Some(rt),
            None => match crate::metadata::prior_runtime_from_disk(service_id) {
                Ok(Some(rt)) => Some(rt),
                // Missing or unparsable metadata keeps the historical default.
                Ok(None) => Some(RuntimeKind::Microvm),
                // Unreadable is not "no file". Do not tail console.log for a
                // service that may be a container. The read already logged.
                Err(_) => None,
            },
        };

        let mut output = logs_snapshot;
        let max_file_read: u64 = 256 * 1024; // 256 KiB cap per file

        let Some(runtime) = runtime else {
            return Some(LogsResponse { output });
        };

        match runtime {
            RuntimeKind::Microvm => {
                let console_path = crate::paths::service_dir(service_id)
                    .join("console.log")
                    .display()
                    .to_string();
                if let Some(console) = read_tail_of_file(&console_path, max_file_read)
                    && !console.is_empty()
                {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("--- console.log ---\n");
                    output.push_str(&console);
                }
            }
            RuntimeKind::Container => {
                let log_path = crate::container::container_log_path(service_id);
                if let Some(container_log) = read_tail_of_file(&log_path, max_file_read)
                    && !container_log.is_empty()
                {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("--- container.log ---\n");
                    output.push_str(&container_log);
                }
            }
        }
        Some(LogsResponse { output })
    }

    /// Build the durable service catalog JSON (no disk write).
    ///
    /// Snapshot under the mutex, then do disk I/O for per-service metadata
    /// outside the critical section so state transitions are not serialized
    /// behind filesystem reads.
    pub fn build_catalog(&self) -> serde_json::Value {
        let snapshot: Vec<(String, String, Option<RuntimeKind>, Option<u16>)> = {
            let inner = self.lock_inner();
            inner
                .services
                .iter()
                .map(|(id, s)| {
                    (
                        id.clone(),
                        s.status.as_str().to_string(),
                        s.runtime,
                        s.host_port,
                    )
                })
                .collect()
        };

        let mut services_map = serde_json::Map::new();
        for (id, status, runtime, host_port) in snapshot {
            let disk = crate::metadata::load_metadata_from_disk(&id);
            let host_port = host_port.or_else(|| disk.as_ref().and_then(|m| m.host_port));
            let runtime = runtime.or_else(|| disk.as_ref().and_then(|m| m.runtime));

            let mut entry = serde_json::json!({
                "status": status,
            });

            if let Some(r) = runtime {
                entry["runtime"] = serde_json::json!(r.to_string());
            }
            if let Some(port) = host_port {
                entry["host_port"] = serde_json::json!(port);
            }

            services_map.insert(id, entry);
        }

        serde_json::json!({
            "schema_version": 1,
            "updated_at": crate::metadata::deployed_at_now(),
            "services": services_map,
        })
    }

    /// Write the durable service catalog to `/var/lib/russel/ctrl-catalog.json`.
    ///
    /// Atomic write: temp file + rename. The catalog is informational;
    /// `metadata.json` remains the source of truth for runtime details.
    pub fn write_catalog(&self) -> anyhow::Result<()> {
        crate::metadata::write_ctrl_catalog(&self.build_catalog())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use russel_core::api::{ServiceStatus, VmState};
    use russel_core::config::RuntimeKind;

    use super::super::app::{AppState, ServiceState, Supervision};
    use crate::deployments::DesiredStateSnapshot;

    /// A deployed service mid-redeploy: `mark_building` snapshot taken.
    fn redeploying(runtime: RuntimeKind) -> ServiceState {
        ServiceState {
            status: ServiceStatus::Building,
            vm_state: VmState::Running,
            runtime: Some(runtime),
            prebuild_status: Some(ServiceStatus::Deployed),
            prebuild_vm_state: Some(VmState::Running),
            ..Default::default()
        }
    }

    #[test]
    fn failed_container_redeploy_keeps_container_supervision() {
        let mut s = redeploying(RuntimeKind::Container);
        s.container_id = Some("abc123".into());
        let (generation, supervision) =
            AppState::apply_failure_transition(&mut s, "build failed").unwrap();
        assert_eq!(supervision, Supervision::Container("abc123".into()));
        assert_eq!(generation, s.process_generation);
        assert_eq!(s.status, ServiceStatus::Deployed);
        assert_eq!(s.vm_state, VmState::Running);
    }

    #[test]
    fn failed_adopted_microvm_redeploy_keeps_pid_supervision() {
        let mut s = redeploying(RuntimeKind::Microvm);
        s.vm_pid = Some(4242);
        let (_, supervision) = AppState::apply_failure_transition(&mut s, "build failed").unwrap();
        assert_eq!(supervision, Supervision::Pid);
    }

    #[test]
    fn failed_first_deploy_needs_no_supervisor() {
        let mut s = ServiceState {
            status: ServiceStatus::Building,
            vm_state: VmState::Pending,
            prebuild_status: Some(ServiceStatus::Idle),
            prebuild_vm_state: Some(VmState::None),
            ..Default::default()
        };
        assert!(AppState::apply_failure_transition(&mut s, "build failed").is_none());
        assert_eq!(s.status, ServiceStatus::Failed);
    }

    #[test]
    fn supervision_is_none_without_a_handle_id_or_pid() {
        let container = ServiceState {
            runtime: Some(RuntimeKind::Container),
            vm_pid: Some(1),
            ..Default::default()
        };
        // A container is never PID-supervised, even with a stale vm_pid.
        assert_eq!(Supervision::for_service(&container), None);
        let microvm = ServiceState {
            runtime: Some(RuntimeKind::Microvm),
            ..Default::default()
        };
        assert_eq!(Supervision::for_service(&microvm), None);
    }

    fn ingress_host_from_metadata(meta: &serde_json::Value) -> Option<String> {
        DesiredStateSnapshot::from_metadata_desired_state(meta).ingress_host
    }

    #[test]
    fn status_route_host_reads_desired_state() {
        let metadata = serde_json::json!({
            "desired_state": {
                "ingress_host": "api.example.com"
            }
        });
        assert_eq!(
            ingress_host_from_metadata(&metadata).as_deref(),
            Some("api.example.com")
        );
    }

    #[test]
    fn status_route_host_is_none_without_desired_host() {
        let metadata = serde_json::json!({"desired_state": {"runtime": "container"}});
        assert_eq!(ingress_host_from_metadata(&metadata), None);
    }
}
