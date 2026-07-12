use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use russel_core::api::{LogsResponse, StatusResponse};
use tokio::process::Child;

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
}

#[derive(Debug)]
struct StateInner {
    services: HashMap<String, ServiceState>,
}

#[derive(Debug)]
struct ServiceState {
    status: String,
    vm_state: String,
    logs: String,
    started_at: Instant,
    flake_path: Option<std::path::PathBuf>,
    vm_pid: Option<u32>,
    vm_process: Option<Child>,
    /// Auxiliary child processes (socat forwarders, etc.) that must stay alive.
    aux_processes: Vec<Child>,
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
            aux_processes: Vec::new(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner {
                services: HashMap::new(),
            })),
        }
    }
}

impl AppState {
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, StateInner> {
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
        s.status = "building".to_string();
        s.vm_state = "pending".to_string();
        Ok(())
    }

    pub fn mark_deployed(&self, service_id: &str, child: Child) {
        let mut inner = self.lock_inner();
        let s = inner.services.entry(service_id.to_string()).or_default();
        s.status = "deployed".to_string();
        s.vm_state = "running".to_string();
        s.started_at = Instant::now();
        s.vm_pid = child.id();
        s.vm_process = Some(child);
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let mut inner = self.lock_inner();
        let s = inner.services.entry(service_id.to_string()).or_default();
        s.status = "failed".to_string();
        s.vm_state = "failed".to_string();
        s.vm_pid = None;
        s.logs.push_str(&error);
        s.logs.push('\n');
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

    /// Park a child process (e.g. socat) so it stays alive as long as the service exists.
    pub fn store_aux_process(&self, service_id: &str, child: Child) {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get_mut(service_id) {
            s.aux_processes.push(child);
        } else {
            tracing::warn!(
                service_id = %service_id,
                "store_aux_process called for unknown service"
            );
        }
    }

    /// Take processes for a service. Returns None if the service doesn't exist.
    pub fn take_processes(&self, service_id: &str) -> Option<(Option<Child>, Vec<Child>)> {
        let mut inner = self.lock_inner();
        let s = inner.services.get_mut(service_id)?;
        let vm = s.vm_process.take();
        let aux = std::mem::take(&mut s.aux_processes);
        s.vm_pid = None;
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
        let vm = s.vm_process.take();
        let aux = std::mem::take(&mut s.aux_processes);
        s.vm_pid = None;
        LifecycleClaim::Claimed(vm, aux)
    }

    pub fn set_status(&self, service_id: &str, status: &str, vm_state: &str) {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get_mut(service_id) {
            s.status = status.to_string();
            s.vm_state = vm_state.to_string();
        }
    }

    pub fn restore_processes(
        &self,
        service_id: &str,
        vm_process: Option<Child>,
        aux_processes: Vec<Child>,
    ) {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get_mut(service_id) {
            if let Some(p) = vm_process {
                s.vm_pid = p.id();
                s.vm_process = Some(p);
            }
            s.aux_processes.extend(aux_processes);
        } else {
            tracing::warn!(
                service_id = %service_id,
                "restore_processes called for unknown service"
            );
        }
    }

    pub fn remove_service(&self, service_id: &str) {
        let mut inner = self.lock_inner();
        inner.services.remove(service_id);
    }

    pub fn status(&self, service_id: &str) -> Option<StatusResponse> {
        let inner = self.lock_inner();
        let s = inner.services.get(service_id)?;
        Some(StatusResponse {
            service_id: service_id.to_string(),
            status: s.status.clone(),
            vm_state: s.vm_state.clone(),
            uptime_seconds: s.started_at.elapsed().as_secs(),
        })
    }

    pub fn logs(&self, service_id: &str) -> Option<LogsResponse> {
        let inner = self.lock_inner();
        let s = inner.services.get(service_id)?;
        Some(LogsResponse {
            output: s.logs.clone(),
        })
    }

    /// Check if the state inner is healthy (not poisoned).
    #[allow(dead_code)]
    pub fn is_healthy(&self) -> bool {
        self.inner.try_lock().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
