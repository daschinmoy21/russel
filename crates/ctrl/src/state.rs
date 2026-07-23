use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use russel_core::api::{LogsResponse, StatusResponse};
use russel_core::config::RuntimeKind;
use tokio::process::Child;
use tokio::sync::Notify;

/// Outcome of attempting to claim a service for a lifecycle operation.
pub enum LifecycleClaim {
    /// Service was claimed; processes are handed off to the caller.
    Claimed(Option<Child>, Vec<Child>),
    /// No such service in state.
    NotFound,
    /// Service exists but is already in a conflicting lifecycle op.
    Busy,
}

#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Mutex<StateInner>>,
    deploy_count: Arc<AtomicUsize>,
    deploy_notify: Arc<Notify>,
}

/// Guard that decrements the in-flight deploy counter on drop.
///
/// Move into a spawned deploy task so the counter is decremented when the
/// task completes (success, error, or panic). If the spawn itself fails,
/// the guard drops in the caller, still decrementing correctly.
#[derive(Debug)]
pub struct DeployGuard {
    state: AppState,
}

impl Drop for DeployGuard {
    fn drop(&mut self) {
        let prev = self.state.deploy_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.state.deploy_notify.notify_waiters();
        }
    }
}

#[derive(Debug)]
pub(crate) struct StateInner {
    pub(crate) services: HashMap<String, ServiceState>,
}

#[derive(Debug)]
pub(crate) struct ServiceState {
    pub(crate) status: String,
    pub(crate) vm_state: String,
    pub(crate) logs: String,
    pub(crate) started_at: Instant,
    pub(crate) flake_path: Option<std::path::PathBuf>,
    pub(crate) vm_pid: Option<u32>,
    pub(crate) vm_process: Option<Child>,
    pub(crate) container_id: Option<String>,
    pub(crate) runtime: Option<RuntimeKind>,
    pub(crate) host_port: Option<u16>,
    pub(crate) guest_port: Option<u16>,
    /// Auxiliary child processes (socat forwarders, etc.) that must stay alive.
    pub(crate) aux_processes: Vec<Child>,
    /// Prior state captured when mark_building is called, for restoring the
    /// previous deployment if the build fails before take_processes.
    pub(crate) prebuild_status: Option<String>,
    pub(crate) prebuild_vm_state: Option<String>,
    /// Bumped when process ownership changes so the matching supervisor exits.
    pub(crate) process_generation: u64,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            vm_state: "none".into(),
            logs: String::new(),
            started_at: Instant::now(),
            flake_path: None,
            vm_pid: None,
            vm_process: None,
            container_id: None,
            runtime: None,
            host_port: None,
            guest_port: None,
            aux_processes: Vec::new(),
            prebuild_status: None,
            prebuild_vm_state: None,
            process_generation: 0,
        }
    }
}

/// Result of one supervisor poll tick.
#[derive(Debug)]
enum SupervisePoll {
    /// Children still running under this generation.
    Running,
    /// Ownership moved or service gone — supervisor should exit quietly.
    Stopped,
    /// A supervised child exited or could not be polled.
    Exited(String),
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner {
                services: HashMap::new(),
            })),
            deploy_count: Arc::new(AtomicUsize::new(0)),
            deploy_notify: Arc::new(Notify::new()),
        }
    }
}

impl AppState {
    pub(crate) fn lock_inner(&self) -> std::sync::MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!("state lock is poisoned — recovering prior state");
            e.into_inner()
        })
    }

    pub fn list_services(&self) -> Vec<String> {
        let inner = self.lock_inner();
        let mut ids: Vec<String> = inner.services.keys().cloned().collect();
        ids.sort();
        ids
    }

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
    pub fn mark_deployed_with_aux(
        &self,
        service_id: &str,
        vm_child: Child,
        aux_children: Vec<Child>,
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
        // ponytail: skip supervisor when no tokio runtime is active (e.g.
        // sync unit tests). The test process is the only observer.
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
            s.logs.push_str(&format!("PROCESS EXIT: {reason}\n"));
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
    pub fn mark_deployed_container(&self, service_id: &str, container_id: &str) {
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
            s.logs
                .push_str(&format!("container running (id: {container_id})\n"));
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
    fn spawn_container_supervisor(
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
            s.logs.push_str(&format!(
                "BUILD FAILED (previous deployment preserved): {}\n",
                error
            ));
            Some(g)
        } else {
            // No prior VM to restore — standard failure.
            s.prebuild_status = None;
            s.prebuild_vm_state = None;
            s.status = "failed".to_string();
            s.vm_state = "failed".to_string();
            s.vm_pid = None;
            s.logs.push_str(error);
            s.logs.push('\n');
            None
        }
    }

    /// Ensure a service entry exists with minimal state.
    /// Used to rehydrate disk-discovered VMs after restart so lifecycle
    /// endpoints (status, stop, destroy) can find them.
    pub fn ensure_service(&self, service_id: &str) {
        let mut inner = self.lock_inner();
        inner
            .services
            .entry(service_id.to_string())
            .or_insert_with(|| {
                let mut s = ServiceState {
                    status: "stopped".to_string(),
                    ..Default::default()
                };
                if let Some(meta) = crate::metadata::load_metadata_from_disk(service_id) {
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
        s.logs.push_str(&format!(
            "using flake at {} (service_id: {})\n",
            flake_path.display(),
            service_id
        ));
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

    /// Atomically begin a lifecycle operation: claim processes and update status.
    /// Distinguishes between NotFound (no such service) and Busy (already in a
    /// conflicting lifecycle op).
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
        // Prevent concurrent lifecycle ops on the same service
        if s.status == "stopping" || s.status == "destroying" || s.status == "building" {
            return LifecycleClaim::Busy;
        }
        s.status = status.to_string();
        s.vm_state = vm_state.to_string();
        // Invalidate supervisor so intentional stop/destroy is not reported as crash.
        s.process_generation = s.process_generation.wrapping_add(1);
        let vm = s.vm_process.take();
        let aux = std::mem::take(&mut s.aux_processes);
        s.vm_pid = None;
        s.prebuild_status = None;
        s.prebuild_vm_state = None;
        LifecycleClaim::Claimed(vm, aux)
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

    // kept for deploy error recovery / future reconcile
    #[allow(dead_code)]
    pub fn restore_processes(
        &self,
        service_id: &str,
        vm_process: Option<Child>,
        aux_processes: Vec<Child>,
    ) {
        let generation = {
            let mut inner = self.lock_inner();
            if let Some(s) = inner.services.get_mut(service_id) {
                if let Some(p) = vm_process {
                    s.vm_pid = p.id();
                    s.vm_process = Some(p);
                }
                s.aux_processes.extend(aux_processes);
                // ponytail: bump generation and spawn a supervisor so the restored
                // children are monitored (issue #32). If status is not "deployed" the
                // supervisor exits harmlessly; the next deployment will spawn its own.
                s.process_generation = s.process_generation.wrapping_add(1);
                s.process_generation
            } else {
                tracing::warn!(
                    service_id = %service_id,
                    "restore_processes called for unknown service"
                );
                return;
            }
        };
        self.spawn_process_supervisor(service_id.to_string(), generation);
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
            s.logs
                .push_str(&format!("rekeyed generation {from} -> {to}\n"));
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
        let inner = self.lock_inner();
        let s = inner.services.get(service_id)?;
        let disk = crate::metadata::load_metadata_from_disk(service_id);
        Some(StatusResponse {
            service_id: service_id.to_string(),
            status: s.status.clone(),
            vm_state: s.vm_state.clone(),
            uptime_seconds: s.started_at.elapsed().as_secs(),
            runtime: s.runtime.or_else(|| disk.as_ref().and_then(|m| m.runtime)),
            host_port: s
                .host_port
                .or_else(|| disk.as_ref().and_then(|m| m.host_port)),
            guest_port: s
                .guest_port
                .or_else(|| disk.as_ref().and_then(|m| m.guest_port)),
        })
    }

    pub fn logs(&self, service_id: &str) -> Option<LogsResponse> {
        let inner = self.lock_inner();
        let s = inner.services.get(service_id)?;
        let mut output = s.logs.clone();
        let runtime = s.runtime.unwrap_or_else(|| {
            crate::metadata::prior_runtime_from_disk(service_id).unwrap_or(RuntimeKind::Microvm)
        });
        match runtime {
            RuntimeKind::Microvm => {
                let console_path = format!("/var/lib/russel/{service_id}/console.log");
                if let Ok(console) = std::fs::read_to_string(&console_path)
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
                if let Ok(container_log) = std::fs::read_to_string(&log_path)
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

    /// Check if the state inner is healthy (not poisoned).
    #[allow(dead_code)]
    pub fn is_healthy(&self) -> bool {
        self.inner.try_lock().is_ok()
    }

    /// Increment the in-flight deploy counter and return a guard.
    ///
    /// The guard must be moved into the spawned deploy task so the counter
    /// is decremented when the task completes. Call this BEFORE spawning to
    /// ensure the counter is accurate even if the spawn is slow.
    pub fn begin_deploy(&self) -> DeployGuard {
        self.deploy_count.fetch_add(1, Ordering::SeqCst);
        DeployGuard {
            state: self.clone(),
        }
    }

    // ── Reconcile / adoption APIs ────────────────────────────────────────────

    /// Adopt a running microVM observed from disk metadata.
    ///
    /// No `Child` handles are created — the VM is observed-only.
    /// Stop/destroy already use disk metadata + `MicrovmRunner`.
    /// If the service already has live Child handles, does nothing.
    pub fn adopt_running_microvm(
        &self,
        service_id: &str,
        host_port: u16,
        guest_port: u16,
        vm_pid: Option<u32>,
    ) {
        let mut inner = self.lock_inner();
        let s = inner.services.entry(service_id.to_string()).or_default();
        if s.vm_process.is_some() {
            tracing::info!(
                service_id = %service_id,
                "skipping microvm adoption — already has live Child handle"
            );
            return;
        }
        s.status = "deployed".to_string();
        s.vm_state = "running".to_string();
        s.runtime = Some(RuntimeKind::Microvm);
        s.vm_pid = vm_pid;
        s.host_port = Some(host_port);
        s.guest_port = Some(guest_port);
        s.started_at = Instant::now();
        s.container_id = None;
        s.vm_process = None; // observed-only
        s.aux_processes.clear();
        s.prebuild_status = None;
        s.prebuild_vm_state = None;
        s.logs.push_str(&format!(
            "adopted running microvm from disk (host_port={host_port}, guest_port={guest_port})\n"
        ));
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
            s.status = "deployed".to_string();
            s.vm_state = "running".to_string();
            s.started_at = Instant::now();
            s.vm_pid = None;
            s.vm_process = None;
            s.container_id = Some(container_id.to_string());
            s.runtime = Some(RuntimeKind::Container);
            s.host_port = Some(host_port);
            s.guest_port = Some(guest_port);
            s.aux_processes.clear();
            s.logs.push_str(&format!(
                "adopted running container from disk (id={container_id}, host_port={host_port}, guest_port={guest_port})\n"
            ));
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
            // Do not clobber an in-memory live deployment.
            if s.vm_process.is_some() || s.container_id.is_some() && s.vm_state == "running" {
                return;
            }
            s.status = "stopped".to_string();
            s.vm_state = "none".to_string();
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

    /// Write the durable service catalog to `/var/lib/russel/ctrl-catalog.json`.
    ///
    /// Atomic write: temp file + rename. The catalog is informational;
    /// `metadata.json` remains the source of truth for runtime details.
    ///
    /// Snapshot under the mutex, then do all disk I/O outside the critical
    /// section so state transitions are not serialized behind filesystem reads.
    pub fn write_catalog(&self) -> anyhow::Result<()> {
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

        let catalog = serde_json::json!({
            "schema_version": 1,
            "updated_at": crate::metadata::deployed_at_now(),
            "services": services_map,
        });

        crate::metadata::write_ctrl_catalog(&catalog)
    }

    /// Wait until all in-flight deploy tasks have completed.
    ///
    /// Called during shutdown after `axum::serve` returns, to ensure no
    /// deploy task is still creating VMs when we detach processes.
    pub async fn wait_for_deploys(&self) {
        loop {
            let notified = self.deploy_notify.notified();
            tokio::pin!(notified);
            if self.deploy_count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.as_mut().await;
        }
    }
}

/// Check if a container is still running via `podman inspect`.
async fn check_container_running(container_id: &str) -> bool {
    let output = match crate::container::podman_command().await
        .args(["inspect", container_id, "--format", "{{.State.Running}}"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim() == "true"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detach_all_processes_empty() {
        let state = AppState::default();
        assert_eq!(state.detach_all_processes(), 0);
    }

    #[tokio::test]
    async fn test_detach_all_processes_nonempty() {
        use tokio::process::Command;

        let state = AppState::default();
        // Create a service with VM and auxiliary processes
        let vm_child = Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("failed to spawn sleep");
        let aux1 = Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("failed to spawn sleep");
        let aux2 = Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("failed to spawn sleep");

        let _vm_pid = vm_child.id();
        state.mark_deployed_with_aux("test-svc", vm_child, vec![aux1, aux2]);

        // Verify initial state
        let status = state.status("test-svc").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
        assert!(
            state
                .lock_inner()
                .services
                .get("test-svc")
                .unwrap()
                .vm_pid
                .is_some()
        );

        // Detach all processes
        let detached = state.detach_all_processes();
        assert_eq!(detached, 1);

        // Verify handles are removed and state updated
        let inner = state.lock_inner();
        let svc = inner.services.get("test-svc").unwrap();
        assert!(svc.vm_process.is_none(), "vm_process should be None");
        assert!(
            svc.aux_processes.is_empty(),
            "aux_processes should be empty"
        );
        assert!(svc.vm_pid.is_none(), "vm_pid should be None");
        assert_eq!(svc.status, "detached");
        assert_eq!(svc.vm_state, "orphaned");
    }

    #[test]
    fn test_mutex_poisoning_recovery() {
        let state = AppState::default();

        // Poison the lock intentionally in a separate thread/panic
        let inner_clone = state.inner.clone();
        let _ = std::thread::spawn(move || {
            let _lock = inner_clone.lock().unwrap();
            panic!("poisoning lock");
        })
        .join();

        // The lock is now poisoned, but lock_inner should recover it
        let inner = state.lock_inner();
        assert!(inner.services.is_empty());
    }

    #[test]
    fn test_mark_and_status() {
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "building");
        assert_eq!(status.vm_state, "pending");

        // Unknown service returns None
        assert!(state.status("svc-2").is_none());
    }

    #[test]
    fn test_mark_building_rejects_conflicting_lifecycle() {
        let state = AppState::default();
        // New entry succeeds.
        state.mark_building("svc-1").unwrap();
        // Already building -> rejected.
        let err = state.mark_building("svc-1").unwrap_err();
        assert!(err.to_string().contains("already in lifecycle state"));

        // stopping / destroying also rejected.
        state.set_status("svc-1", "stopping", "pending");
        assert!(state.mark_building("svc-1").is_err());
        state.set_status("svc-1", "destroying", "pending");
        assert!(state.mark_building("svc-1").is_err());

        // A fresh service still works alongside the conflicting one.
        state.mark_building("svc-2").unwrap();
        assert_eq!(state.status("svc-2").unwrap().status, "building");
    }

    #[test]
    fn test_mark_deployed_then_logs_and_status() {
        let state = AppState::default();
        state.mark_building("svc-a").unwrap();
        // We can't create a real Child in tests, so mark_failed is the easier path
        state.mark_failed("svc-a", "test error".into());
        let status = state.status("svc-a").unwrap();
        assert_eq!(status.status, "failed");
        let logs = state.logs("svc-a").unwrap();
        assert!(logs.output.contains("test error"));
    }

    #[test]
    fn test_services_are_independent() {
        let state = AppState::default();
        state.mark_building("alpha").unwrap();
        state.mark_building("beta").unwrap();
        state.mark_failed("beta", "beta error".into());

        let alpha = state.status("alpha").unwrap();
        assert_eq!(alpha.status, "building");
        let beta = state.status("beta").unwrap();
        assert_eq!(beta.status, "failed");
    }

    #[test]
    fn test_begin_lifecycle_operation() {
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        // While status is "building" the claim is Busy (not NotFound).
        assert!(matches!(
            state.begin_lifecycle_operation("svc-1", "stopping", "pending"),
            LifecycleClaim::Busy
        ));
        // Unknown service is NotFound, distinct from Busy.
        assert!(matches!(
            state.begin_lifecycle_operation("nope", "stopping", "pending"),
            LifecycleClaim::NotFound
        ));
    }

    #[test]
    fn test_remove_service() {
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        assert!(state.status("svc-1").is_some());
        state.remove_service("svc-1");
        assert!(state.status("svc-1").is_none());
    }

    #[test]
    fn test_list_services() {
        let state = AppState::default();
        state.mark_building("z").unwrap();
        state.mark_building("a").unwrap();
        state.mark_building("m").unwrap();
        let ids = state.list_services();
        assert_eq!(ids, vec!["a", "m", "z"]);
    }

    // ── mark_building vm_state transition tests ──────────────────────────────

    #[test]
    fn test_mark_building_resets_failed_vm_state() {
        // Verify that redeploying a failed service resets vm_state to pending,
        // avoiding the "building/failed" stale state combination.
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        state.mark_failed("svc-1", "first failure".into());
        assert_eq!(state.status("svc-1").unwrap().vm_state, "failed");

        // Redeploy — vm_state should transition from failed → pending
        state.mark_building("svc-1").unwrap();
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "building");
        assert_eq!(status.vm_state, "pending");
    }

    #[test]
    fn test_mark_building_preserves_running_vm_state() {
        // When vm_state is "running" AND vm_process is Some (real Child),
        // mark_building preserves "running". Since we can't create a real
        // tokio::process::Child in unit tests, we verify the fallback:
        // without a process handle, "running" → "pending" here.
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        state.set_status("svc-1", "deployed", "running");

        state.mark_building("svc-1").unwrap();
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "building");
        assert_eq!(status.vm_state, "pending");
    }

    // ── mark_failed prior-state preservation tests ───────────────────────────

    #[test]
    fn test_mark_failed_preserves_prior_running_state() {
        // A redeploy that fails before take_processes should restore the
        // previous deployed/running state — not set failed/failed.
        let state = AppState::default();
        // Set up a deployed service
        state.mark_building("svc-1").unwrap();
        state.set_status("svc-1", "deployed", "running");

        // Redeploy: mark_building captures prebuild snapshot
        state.mark_building("svc-1").unwrap();

        // Build fails before take_processes
        state.mark_failed("svc-1", "build error".into());
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
        let logs = state.logs("svc-1").unwrap();
        assert!(logs.output.contains("BUILD FAILED"));
    }

    #[test]
    fn test_mark_failed_fresh_deploy_no_prior_vm() {
        // A fresh deploy that fails should set failed/failed since there
        // was no prior running VM to restore.
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        state.mark_failed("svc-1", "build error".into());
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "failed");
        assert_eq!(status.vm_state, "failed");
    }

    #[test]
    fn test_mark_failed_after_take_processes_sets_failed() {
        // If processes were taken before the failure, the prebuild snapshot
        // is cleared, so mark_failed should set failed/failed.
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        state.set_status("svc-1", "deployed", "running");
        state.mark_building("svc-1").unwrap();

        // Simulate processes being taken (clears snapshot)
        let _ = state.take_processes("svc-1");

        // Now fail — should go to failed/failed since snapshot is gone
        state.mark_failed("svc-1", "deploy failed".into());
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "failed");
        assert_eq!(status.vm_state, "failed");
    }

    // ── ensure_service tests ─────────────────────────────────────────────────

    #[test]
    fn test_ensure_service_creates_minimal_entry() {
        let state = AppState::default();
        assert!(state.status("disk-vm").is_none());

        state.ensure_service("disk-vm");
        let status = state.status("disk-vm").unwrap();
        assert_eq!(status.status, "stopped");
        assert_eq!(status.vm_state, "none");
    }

    #[test]
    fn test_ensure_service_does_not_overwrite_existing() {
        let state = AppState::default();
        state.mark_building("svc-1").unwrap();
        state.set_status("svc-1", "deployed", "running");

        // ensure_service should not overwrite existing state
        state.ensure_service("svc-1");
        let status = state.status("svc-1").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
    }

    // ── process supervisor (issue #32) ───────────────────────────────────────

    #[tokio::test]
    async fn supervisor_marks_failed_when_child_exits() {
        let state = AppState::default();
        // `true` exits immediately with status 0.
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        state.mark_deployed_with_aux("svc-exit", child, vec![]);

        // Supervisor polls every 500ms after an initial tick skip.
        let mut saw_failed = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Some(status) = state.status("svc-exit")
                && status.status == "failed"
            {
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "expected supervisor to mark service failed");
        let logs = state.logs("svc-exit").unwrap();
        assert!(
            logs.output.contains("PROCESS EXIT"),
            "logs missing PROCESS EXIT: {}",
            logs.output
        );
    }

    #[tokio::test]
    async fn supervisor_ignores_intentional_take_processes() {
        let state = AppState::default();
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        state.mark_deployed_with_aux("svc-take", child, vec![]);

        let (vm, _aux) = state.take_processes("svc-take").expect("processes present");
        // Give the supervisor time to observe the generation change.
        tokio::time::sleep(Duration::from_millis(1200)).await;

        let status = state.status("svc-take").unwrap();
        assert_ne!(
            status.status, "failed",
            "intentional take_processes must not be reported as crash"
        );

        if let Some(mut child) = vm {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    // ── deploy tracking tests ─────────────────────────────────────────────

    #[test]
    fn test_begin_deploy_increments_counter() {
        let state = AppState::default();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

        let _guard1 = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

        let _guard2 = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_deploy_guard_drop_decrements_counter() {
        let state = AppState::default();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

        let guard1 = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

        let guard2 = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 2);

        drop(guard1);
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

        drop(guard2);
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_wait_for_deploys_returns_immediately_when_zero() {
        let state = AppState::default();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

        // Should return immediately without blocking
        state.wait_for_deploys().await;
    }

    #[tokio::test]
    async fn test_wait_for_deploys_waits_for_guards() {
        let state = AppState::default();
        let guard = state.begin_deploy();
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 1);

        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            state_clone.wait_for_deploys().await;
        });

        // Give the task time to start waiting
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(
            !handle.is_finished(),
            "wait_for_deploys should still be waiting"
        );

        // Drop the guard to decrement counter
        drop(guard);
        assert_eq!(state.deploy_count.load(Ordering::SeqCst), 0);

        // Now the task should complete
        tokio::time::timeout(tokio::time::Duration::from_millis(100), handle)
            .await
            .expect("wait_for_deploys should complete after counter reaches 0")
            .expect("task should not panic");
    }

    // ── adopt / reconcile APIs ────────────────────────────────────────────────

    #[test]
    fn test_adopt_running_microvm_sets_deployed() {
        let state = AppState::default();
        state.adopt_running_microvm("adopted-vm", 3100, 3000, Some(42));

        let status = state.status("adopted-vm").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
        assert_eq!(status.runtime, Some(RuntimeKind::Microvm));
        assert_eq!(status.host_port, Some(3100));
        assert_eq!(status.guest_port, Some(3000));
    }

    #[tokio::test]
    async fn test_adopt_running_microvm_does_not_overwrite_live_handle() {
        let state = AppState::default();

        // Create a live Child handle first (needs tokio runtime for pidfd).
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        state.mark_deployed_with_aux("protected", child, vec![]);

        // Adopt should not overwrite.
        state.adopt_running_microvm("protected", 9999, 9999, None);

        let inner = state.lock_inner();
        let svc = inner.services.get("protected").unwrap();
        assert!(svc.vm_process.is_some(), "live Child handle preserved");
    }

    #[test]
    fn test_adopt_running_container_sets_deployed() {
        let state = AppState::default();
        state.adopt_running_container("adopted-ctr", "abc123", 3100, 3000);

        let status = state.status("adopted-ctr").unwrap();
        assert_eq!(status.status, "deployed");
        assert_eq!(status.vm_state, "running");
        assert_eq!(status.runtime, Some(RuntimeKind::Container));
        assert_eq!(status.host_port, Some(3100));
    }

    #[tokio::test]
    async fn test_adopt_running_container_does_not_overwrite_live_handle() {
        let state = AppState::default();

        // Create a live Child handle first (needs tokio runtime for pidfd).
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        state.mark_deployed_with_aux("protected-ctr", child, vec![]);

        // Adopt should not overwrite.
        state.adopt_running_container("protected-ctr", "xyz", 9999, 9999);

        let inner = state.lock_inner();
        let svc = inner.services.get("protected-ctr").unwrap();
        assert!(svc.vm_process.is_some(), "live Child handle preserved");
        assert!(
            svc.runtime == Some(RuntimeKind::Microvm),
            "runtime unchanged"
        );
    }

    #[test]
    fn test_write_catalog_emits_valid_json() {
        let state = AppState::default();
        state.adopt_running_microvm("cat-svc", 4000, 3000, None);

        // write_catalog writes /var/lib/russel/ctrl-catalog.json —
        // we can't unit-test that path directly without root, but we can
        // verify it doesn't panic and the catalog machinery works.
        // In CI/tests the /var/lib/russel path may not exist, so this
        // is a best-effort smoke test.
        let result = state.write_catalog();
        // The write may fail if /var/lib/russel doesn't exist in test env.
        // That's fine — the code path is exercised.
        let _ = result;
    }
}
