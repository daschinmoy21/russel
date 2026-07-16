use serde::{Deserialize, Serialize};

use crate::config::RuntimeKind;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployRequest {
    pub repo_url: String,
    pub config_path: String,
    pub vm_id: Option<String>,
    pub port: Option<PortMapping>,
    #[serde(default)]
    pub runtime: Option<RuntimeKind>,
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
    #[serde(default)]
    pub runtime: Option<RuntimeKind>,
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
    #[serde(default)]
    pub runtime: Option<RuntimeKind>,
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default)]
    pub guest_port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogsResponse {
    pub output: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceSummary {
    pub service_id: String,
    #[serde(default)]
    pub runtime: Option<RuntimeKind>,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmsResponse {
    pub vms: Vec<String>,
    #[serde(default)]
    pub services: Vec<ServiceSummary>,
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
                runtime: Some(RuntimeKind::Microvm),
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
            runtime: Some(RuntimeKind::Container),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("my-id"));
        assert!(json.contains("8080"));
        assert!(json.contains("\"runtime\":\"container\""));
    }

    #[test]
    fn deploy_request_deserializes_without_runtime() {
        let json = r#"{"repo_url":"https://example.com/repo.git","config_path":"Russelfile.toml"}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        assert!(req.runtime.is_none());
    }

    #[test]
    fn deploy_response_deserializes_without_runtime() {
        let json = r#"{
            "service_id":"svc",
            "vm_id":"vm1",
            "status":"deployed",
            "elapsed_ms":100,
            "message":"ok"
        }"#;
        let resp: DeployResponse = serde_json::from_str(json).unwrap();
        assert!(resp.runtime.is_none());
    }

    #[test]
    fn status_response_defaults() {
        let resp = StatusResponse {
            service_id: "test".into(),
            status: "idle".into(),
            vm_state: "none".into(),
            uptime_seconds: 0,
            runtime: None,
            host_port: None,
            guest_port: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "idle");
        assert!(parsed.runtime.is_none());
    }

    #[test]
    fn status_response_serializes_runtime_and_ports() {
        let resp = StatusResponse {
            service_id: "api".into(),
            status: "deployed".into(),
            vm_state: "running".into(),
            uptime_seconds: 42,
            runtime: Some(RuntimeKind::Container),
            host_port: Some(3100),
            guest_port: Some(3000),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"runtime\":\"container\""));
        let parsed: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.runtime, Some(RuntimeKind::Container));
        assert_eq!(parsed.host_port, Some(3100));
    }

    #[test]
    fn vms_response_deserializes_without_services() {
        let json = r#"{"vms":["api","demo"]}"#;
        let resp: VmsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.vms, vec!["api", "demo"]);
        assert!(resp.services.is_empty());
    }

    #[test]
    fn vms_response_serializes_service_summaries() {
        let resp = VmsResponse {
            vms: vec!["api".into()],
            services: vec![ServiceSummary {
                service_id: "api".into(),
                runtime: Some(RuntimeKind::Microvm),
                status: "deployed".into(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"services\""));
        let parsed: VmsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.services[0].runtime, Some(RuntimeKind::Microvm));
    }
}
