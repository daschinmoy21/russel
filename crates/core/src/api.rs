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
    Complete(Box<DeployResponse>),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_event_progress_serializes_as_tagged() {
        let event = DeployEvent::Progress {
            phase: "build".into(),
            description: "compiling".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DeployEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            DeployEvent::Progress { phase, description } => {
                assert_eq!(phase, "build");
                assert_eq!(description, "compiling");
            }
            _ => panic!("expected Progress variant"),
        }
    }

    #[test]
    fn deploy_event_serialization_roundtrip() {
        let events = vec![
            DeployEvent::Progress {
                phase: "resolve".into(),
                description: "cloning".into(),
            },
            DeployEvent::Complete(Box::new(DeployResponse {
                service_id: "svc".into(),
                vm_id: "vm1".into(),
                status: "deployed".into(),
                store_path: Some("/nix/store/abc".into()),
                microvm_config_path: None,
                runner_path: None,
                port: Some(PortMapping {
                    host: 8080,
                    guest: 3000,
                }),
                elapsed_ms: 1234,
                message: "ok".into(),
                timing: None,
                vm_ip: Some("10.0.5.2".into()),
            })),
            DeployEvent::Error("build failed".into()),
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let _parsed: DeployEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn deploy_request_serializes_correctly() {
        let req = DeployRequest {
            repo_url: "https://github.com/example/repo.git".into(),
            config_path: "Russelfile.toml".into(),
            vm_id: Some("my-id".into()),
            port: Some(PortMapping {
                host: 8080,
                guest: 3000,
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("my-id"));
        assert!(json.contains("8080"));
    }

    #[test]
    fn status_response_defaults() {
        let resp = StatusResponse {
            service_id: "test".into(),
            status: "idle".into(),
            vm_state: "none".into(),
            uptime_seconds: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "idle");
    }
}
