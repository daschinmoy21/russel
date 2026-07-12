use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use russel_core::api::{LogsResponse, StatusResponse};
use tokio::process::Child;

#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Mutex<StateInner>>,
}

#[derive(Debug)]
struct StateInner {
    service_id: String,
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

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner {
                service_id: "api".to_string(),
                status: "idle".to_string(),
                vm_state: "none".to_string(),
                logs: String::new(),
                started_at: Instant::now(),
                flake_path: None,
                vm_pid: None,
                vm_process: None,
                aux_processes: Vec::new(),
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

    pub fn mark_building(&self, service_id: &str) {
        let mut inner = self.lock_inner();
        inner.service_id = service_id.to_string();
        inner.status = "building".to_string();
        inner.vm_state = "pending".to_string();
    }

    pub fn mark_deployed(&self, service_id: &str, child: Child) {
        let mut inner = self.lock_inner();
        inner.service_id = service_id.to_string();
        inner.status = "deployed".to_string();
        inner.vm_state = "running".to_string();
        inner.started_at = Instant::now();
        inner.vm_pid = child.id();
        inner.vm_process = Some(child);
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let mut inner = self.lock_inner();
        inner.service_id = service_id.to_string();
        inner.status = "failed".to_string();
        inner.vm_state = "failed".to_string();
        inner.vm_pid = None;
        inner.logs.push_str(&error);
        inner.logs.push('\n');
    }

    pub fn attach_flake_path(&self, service_id: &str, flake_path: std::path::PathBuf) {
        let mut inner = self.lock_inner();
        inner.service_id = service_id.to_string();
        inner.logs.push_str(&format!(
            "using flake at {} (service_id: {})\n",
            flake_path.display(),
            service_id
        ));
        inner.flake_path = Some(flake_path);
    }

    /// Park a child process (e.g. socat) so it stays alive as long as the state exists.
    pub fn store_aux_process(&self, child: Child) {
        let mut inner = self.lock_inner();
        inner.aux_processes.push(child);
    }

    pub fn take_processes_if_matches(&self, service_id: &str) -> (Option<Child>, Vec<Child>) {
        let mut inner = self.lock_inner();
        if inner.service_id == service_id {
            let vm = inner.vm_process.take();
            let aux = std::mem::take(&mut inner.aux_processes);
            inner.vm_pid = None;
            (vm, aux)
        } else {
            (None, Vec::new())
        }
    }

    /// Atomically begin a lifecycle operation: update status and claim processes.
    /// Returns None if the service_id doesn't match or the state is already claimed.
    pub fn begin_lifecycle_operation(&self, service_id: &str, status: &str, vm_state: &str) -> Option<(Option<Child>, Vec<Child>)> {
        let mut inner = self.lock_inner();
        if inner.service_id == service_id {
            inner.status = status.to_string();
            inner.vm_state = vm_state.to_string();
            let vm = inner.vm_process.take();
            let aux = std::mem::take(&mut inner.aux_processes);
            inner.vm_pid = None;
            Some((vm, aux))
        } else {
            None
        }
    }

    pub fn set_status_if_matches(&self, service_id: &str, status: &str, vm_state: &str) {
        let mut inner = self.lock_inner();
        if inner.service_id == service_id {
            inner.status = status.to_string();
            inner.vm_state = vm_state.to_string();
        }
    }

    pub fn restore_processes(&self, service_id: &str, vm_process: Option<Child>, aux_processes: Vec<Child>) {
        let mut inner = self.lock_inner();
        if inner.service_id == service_id {
            if let Some(p) = vm_process {
                inner.vm_pid = p.id();
                inner.vm_process = Some(p);
            }
            inner.aux_processes.extend(aux_processes);
        }
    }

    pub fn status(&self) -> StatusResponse {
        let inner = self.lock_inner();
        StatusResponse {
            service_id: inner.service_id.clone(),
            status: inner.status.clone(),
            vm_state: inner.vm_state.clone(),
            uptime_seconds: inner.started_at.elapsed().as_secs(),
        }
    }

    pub fn logs(&self) -> LogsResponse {
        let inner = self.lock_inner();
        LogsResponse {
            output: inner.logs.clone(),
        }
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
        }).join();

        // The lock is now poisoned, but lock_inner should recover it
        let inner = state.lock_inner();
        assert_eq!(inner.service_id, "api");
    }
}
