use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployRequest {
    pub repo_url: String,
    pub config_path: String,
    pub vm_id: Option<String>,
    pub port: Option<PortMapping>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployResponse {
    pub service_id: String,
    pub vm_id: String,
    pub status: String,
    pub store_path: Option<String>,
    pub microvm_config_path: Option<String>,
    pub runner_path: Option<String>,
    pub port: Option<PortMapping>,
    pub elapsed_ms: u128,
    pub message: String,
    /// Per-step timing breakdown (ms each step took).
    pub timing: Option<DeployTiming>,
    /// Direct VM IP for diagnostics (e.g. `curl 10.0.x.2:3000`).
    pub vm_ip: Option<String>,
}

/// Millisecond breakdown of each deploy phase, included in every successful response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployTiming {
    pub resolve_ms: u128,
    pub build_ms: u128,
    pub create_ms: u128,
    pub start_ms: u128,
    pub network_ms: u128,
    pub ready_ms: u128,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PortMapping {
    pub host: u16,
    pub guest: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusResponse {
    pub service_id: String,
    pub status: String,
    pub vm_state: String,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogsResponse {
    pub output: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmsResponse {
    pub vms: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum DeployEvent {
    Progress { phase: String, description: String },
    Complete(DeployResponse),
    Error(String),
}
