//! Service lifecycle transitions, process ownership, and supervisors.

use std::time::{Duration, Instant};

use russel_core::api::{LogsResponse, StatusResponse};
use russel_core::config::RuntimeKind;
use tokio::process::Child;

use super::app::{AppState, LifecycleClaim, ServiceState, SupervisePoll};
use super::helpers::{check_container_running, pid_is_alive, push_capped, read_tail_of_file};

impl AppState {
    /// Mark a service as building. Rejects if the service already exists in a
    /// conflicting lifecycle state (building/stopping/destroying). Creates a
    /// new entry if the service does not yet exist.
    pub fn mark_building(&self, service_id: &str) -> anyhow::Result<()> {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get(service_id)
            && (s.status == "building" || s.status == "stopping" || s.status == "destroying")
        {
            anyhow::bail!(
                "service {} is already in lifecycle state '{}'",
                service_id,
                s.status
            );
        }
        let s = inner.services.entry(service_id.to_string()).or_default();
        // Capture prior state for failure recovery during redeployment.
        s.prebuild_status = Some(s.status.clone());
        s.prebuild_vm_state = Some(s.vm_state.clone());
        s.status = "building".to_string();
        // Reset stale vm_state: "failed" → "pending", preserve "running" for
        // an existing VM process, set new entries to "pending".
        match s.vm_state.as_str() {
            "failed" => s.vm_state = "pending".to_string(),
            "none" => s.vm_state = "pending".to_string(),
            "running" if s.vm_process.is_none() => s.vm_state = "pending".to_string(),
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
            s.status = "deployed".to_string();
            s.vm_state = "running".to_string();
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
                    if s.status != "deployed" || s.vm_state != "running" {
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
                    state.mark_failed_if_generation(
                        &service_id,
                        generation,
                        format!("adopted microVM PID {pid} no longer alive"),
                    );
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
        if s.status != "deployed" || s.vm_state != "running" {
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
            s.status = "failed".to_string();
            s.vm_state = "failed".to_string();
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
    /// Spawns a lightweight liveness supervisor that polls podman.
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
            s.status = "deployed".to_string();
            s.vm_state = "running".to_string();
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
        );

        // Best-effort catalog update.
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_deployed_container");
        }
    }

    /// Lightweight container liveness supervisor: polls podman inspect every 30s.
    /// If the container is no longer running, marks the service failed.
    pub(super) fn spawn_container_supervisor(
        &self,
        service_id: String,
        container_id: String,
        generation: u64,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            // Initial delay to let container settle.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut consecutive_failures: u32 = 0;
            loop {
                interval.tick().await;
                // Check we still own this generation.
                {
                    let inner = state.lock_inner();
                    let Some(s) = inner.services.get(&service_id) else {
                        return;
                    };
                    if s.process_generation != generation {
                        return;
                    }
                    if s.status != "deployed" || s.vm_state != "running" {
                        return;
                    }
                }
                // Poll podman inspect for the container.
                // Require two consecutive failures before marking failed —
                // podman inspect can transiently return false even when the
                // container is healthy (e.g. brief podman state inconsistency).
                let alive = check_container_running(&container_id).await;
                if alive {
                    consecutive_failures = 0;
                    continue;
                }
                consecutive_failures += 1;
                if consecutive_failures < 2 {
                    tracing::warn!(
                        service_id = %service_id,
                        container_id = %container_id,
                        consecutive_failures,
                        "container appears down — will retry next cycle"
                    );
                    continue;
                }
                tracing::warn!(
                    service_id = %service_id,
                    container_id = %container_id,
                    "container is no longer running after {consecutive_failures} checks — marking failed"
                );
                // Use generation-conditional mark so a concurrent redeploy
                // that bumps process_generation is not wrongly marked failed.
                state.mark_failed_if_generation(
                    &service_id,
                    generation,
                    format!("container {container_id} is not running"),
                );
                return;
            }
        });
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let needs_supervisor = {
            let mut inner = self.lock_inner();
            let s = inner.services.entry(service_id.to_string()).or_default();
            Self::apply_failure_transition(s, &error)
        };

        if let Some(g) = needs_supervisor {
            self.spawn_process_supervisor(service_id.to_string(), g);
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
        let needs_supervisor = {
            let mut inner = self.lock_inner();
            let Some(s) = inner.services.get_mut(service_id) else {
                return false;
            };
            if s.process_generation != expected_generation {
                return false;
            }
            if s.status != "deployed" || s.vm_state != "running" {
                return false;
            }
            Self::apply_failure_transition(s, &error)
        };

        if let Some(g) = needs_supervisor {
            self.spawn_process_supervisor(service_id.to_string(), g);
        }

        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after mark_failed_if_generation");
        }

        true
    }

    /// Apply the failure state transition to a ServiceState in-place.
    ///
    /// If a prebuild snapshot exists (previous deployment still running),
    /// restores that state and returns the new generation for supervisor
    /// restart. Otherwise sets standard `failed`/`failed` state.
    fn apply_failure_transition(s: &mut ServiceState, error: &str) -> Option<u64> {
        if s.prebuild_vm_state.as_deref() == Some("running") {
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
            Some(g)
        } else {
            // No prior VM to restore — standard failure.
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.status = "failed".to_string();
            s.vm_state = "failed".to_string();
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
        // Read disk metadata before acquiring the state lock so filesystem I/O
        // is never performed while holding the global mutex. A concurrent
        // insert between the check and the re-acquire is benign: `entry` then
        // no-ops and this thread's disk read is simply discarded.
        let disk_meta = {
            let inner = self.lock_inner();
            if inner.services.contains_key(service_id) {
                return;
            }
            crate::metadata::load_metadata_from_disk(service_id)
        };

        let mut inner = self.lock_inner();
        inner
            .services
            .entry(service_id.to_string())
            .or_insert_with(|| {
                let mut s = ServiceState {
                    status: "stopped".to_string(),
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
        status: &str,
        vm_state: &str,
    ) -> LifecycleClaim {
        let mut inner = self.lock_inner();
        let Some(s) = inner.services.get_mut(service_id) else {
            return LifecycleClaim::NotFound;
        };
        let busy = match s.status.as_str() {
            "building" | "destroying" => true,
            "stopping" => status != "destroying",
            _ => false,
        };
        if busy {
            return LifecycleClaim::Busy;
        }
        let prior_status = s.status.clone();
        let prior_vm_state = s.vm_state.clone();
        s.status = status.to_string();
        s.vm_state = vm_state.to_string();
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
        expected_status: &str,
        prior_status: &str,
        prior_vm_state: &str,
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
            s.status = prior_status.to_string();
            s.vm_state = prior_vm_state.to_string();
            s.process_generation = s.process_generation.wrapping_add(1);
            let generation = s.process_generation;
            let has_children = s.vm_process.is_some() || !s.aux_processes.is_empty();
            if has_children { Some(generation) } else { None }
        };
        if let Some(generation) = needs_supervisor {
            self.spawn_process_supervisor(service_id.to_string(), generation);
        }
        if let Err(e) = self.write_catalog() {
            tracing::warn!(error = %e, "failed to write catalog after abort_lifecycle_operation");
        }
    }

    pub fn set_status(&self, service_id: &str, status: &str, vm_state: &str) {
        {
            let mut inner = self.lock_inner();
            if let Some(s) = inner.services.get_mut(service_id) {
                s.status = status.to_string();
                s.vm_state = vm_state.to_string();
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
    pub fn rekey_service(&self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let mut inner = self.lock_inner();
        if let Some(mut s) = inner.services.remove(from) {
            // Drop residual entry for `to` (old generation already drained).
            let _ = inner.services.remove(to);
            push_capped(&mut s.logs, &format!("rekeyed generation {from} -> {to}\n"));
            inner.services.insert(to.to_string(), s);
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
                if s.status == "deployed" || s.vm_state == "running" {
                    s.status = "detached".to_string();
                    s.vm_state = "orphaned".to_string();
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
        let (status, vm_state, runtime, host_port, guest_port, uptime_seconds) = {
            let inner = self.lock_inner();
            let s = inner.services.get(service_id)?;
            // Only count wall-clock uptime while the workload is actually running.
            // Stopped/failed services must not report a growing timer from started_at.
            let uptime_seconds = if s.vm_state == "running" {
                s.started_at.elapsed().as_secs()
            } else {
                0
            };
            (
                s.status.clone(),
                s.vm_state.clone(),
                s.runtime,
                s.host_port,
                s.guest_port,
                uptime_seconds,
            )
        };

        let disk = crate::metadata::load_metadata_from_disk(service_id);
        Some(StatusResponse {
            service_id: service_id.to_string(),
            status,
            vm_state,
            uptime_seconds,
            runtime: runtime.or_else(|| disk.as_ref().and_then(|m| m.runtime)),
            host_port: host_port.or_else(|| disk.as_ref().and_then(|m| m.host_port)),
            guest_port: guest_port.or_else(|| disk.as_ref().and_then(|m| m.guest_port)),
        })
    }

    pub fn logs(&self, service_id: &str) -> Option<LogsResponse> {
        // Snapshot needed fields under the lock, then release before file I/O.
        let (logs_snapshot, runtime) = {
            let inner = self.lock_inner();
            let s = inner.services.get(service_id)?;
            (s.logs.clone(), s.runtime)
        };

        let runtime = runtime.unwrap_or_else(|| {
            crate::metadata::prior_runtime_from_disk(service_id).unwrap_or(RuntimeKind::Microvm)
        });

        let mut output = logs_snapshot;
        let max_file_read: u64 = 256 * 1024; // 256 KiB cap per file

        match runtime {
            RuntimeKind::Microvm => {
                let console_path = format!("/var/lib/russel/{service_id}/console.log");
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
                .map(|(id, s)| (id.clone(), s.status.clone(), s.runtime, s.host_port))
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
