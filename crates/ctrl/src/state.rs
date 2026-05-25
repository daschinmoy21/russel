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
    fn lock_inner(&self) -> anyhow::Result<std::sync::MutexGuard<'_, StateInner>> {
        self.inner.lock().map_err(|e| {
            tracing::error!("state lock is poisoned — prior panic in locked section");
            anyhow::anyhow!("state lock poisoned: {}", e)
        })
    }

    pub fn mark_building(&self, service_id: &str) {
        let Ok(mut inner) = self.lock_inner() else {
            tracing::error!("failed to mark state as building");
            return;
        };
        inner.service_id = service_id.to_string();
        inner.status = "building".to_string();
        inner.vm_state = "pending".to_string();
    }

    pub fn mark_deployed(&self, service_id: &str, child: Child) {
        let Ok(mut inner) = self.lock_inner() else {
            tracing::error!("failed to mark state as deployed");
            return;
        };
        inner.service_id = service_id.to_string();
        inner.status = "deployed".to_string();
        inner.vm_state = "running".to_string();
        inner.started_at = Instant::now();
        inner.vm_pid = child.id();
        inner.vm_process = Some(child);
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let Ok(mut inner) = self.lock_inner() else {
            tracing::error!("failed to mark state as failed");
            return;
        };
        inner.service_id = service_id.to_string();
        inner.status = "failed".to_string();
        inner.vm_state = "failed".to_string();
        inner.vm_pid = None;
        inner.logs.push_str(&error);
        inner.logs.push('\n');
    }

    pub fn attach_flake_path(&self, service_id: &str, flake_path: std::path::PathBuf) {
        let Ok(mut inner) = self.lock_inner() else {
            tracing::error!("failed to attach flake path");
            return;
        };
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
        let Ok(mut inner) = self.lock_inner() else {
            tracing::error!("failed to store aux process");
            return;
        };
        inner.aux_processes.push(child);
    }

    pub fn status(&self) -> StatusResponse {
        let Ok(inner) = self.lock_inner() else {
            return StatusResponse {
                service_id: "unknown".into(),
                status: "error".into(),
                vm_state: "error".into(),
                uptime_seconds: 0,
            };
        };
        StatusResponse {
            service_id: inner.service_id.clone(),
            status: inner.status.clone(),
            vm_state: inner.vm_state.clone(),
            uptime_seconds: inner.started_at.elapsed().as_secs(),
        }
    }

    pub fn logs(&self) -> LogsResponse {
        let Ok(inner) = self.lock_inner() else {
            return LogsResponse {
                output: String::new(),
            };
        };
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
