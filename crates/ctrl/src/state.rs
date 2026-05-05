use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use russel_core::api::{LogsResponse, StatusResponse};
use tokio::process::Child;

use crate::microvm::GeneratedMicrovmConfig;

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
    vm_config: Option<GeneratedMicrovmConfig>,
    vm_pid: Option<u32>,
    vm_process: Option<Child>,
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
                vm_config: None,
                vm_pid: None,
                vm_process: None,
            })),
        }
    }
}

impl AppState {
    pub fn mark_building(&self, service_id: &str) {
        let mut inner = self.inner.lock().expect("state lock poisoned");
        inner.service_id = service_id.to_string();
        inner.status = "building".to_string();
        inner.vm_state = "pending".to_string();
    }

    pub fn mark_deployed(&self, service_id: &str, child: Child) {
        let mut inner = self.inner.lock().expect("state lock poisoned");
        inner.service_id = service_id.to_string();
        inner.status = "deployed".to_string();
        inner.vm_state = "running".to_string();
        inner.started_at = Instant::now();
        inner.vm_pid = child.id();
        inner.vm_process = Some(child);
    }

    pub fn mark_failed(&self, service_id: &str, error: String) {
        let mut inner = self.inner.lock().expect("state lock poisoned");
        inner.service_id = service_id.to_string();
        inner.status = "failed".to_string();
        inner.vm_state = "failed".to_string();
        inner.vm_pid = None;
        inner.logs.push_str(&error);
        inner.logs.push('\n');
    }

    pub fn attach_vm_config(&self, service_id: &str, config: GeneratedMicrovmConfig) {
        let mut inner = self.inner.lock().expect("state lock poisoned");
        inner.service_id = service_id.to_string();
        inner.logs.push_str(&format!(
            "generated microvm.nix config at {} ({} bytes)\n",
            config.path.display(),
            config.contents.len()
        ));
        inner.vm_config = Some(config);
    }

    pub fn status(&self) -> StatusResponse {
        let inner = self.inner.lock().expect("state lock poisoned");
        StatusResponse {
            service_id: inner.service_id.clone(),
            status: inner.status.clone(),
            vm_state: inner.vm_state.clone(),
            uptime_seconds: inner.started_at.elapsed().as_secs(),
        }
    }

    pub fn logs(&self) -> LogsResponse {
        let inner = self.inner.lock().expect("state lock poisoned");
        LogsResponse {
            output: inner.logs.clone(),
        }
    }
}
