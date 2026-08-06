use std::collections::HashMap;

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
    /// Extra `podman run` arguments (container runtime only).
    #[serde(default)]
    pub podman_args: Vec<String>,
    /// User-defined environment variables. Overrides Russelfile `[service.env]`.
    #[serde(default)]
    pub env: HashMap<String, String>,
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
    /// Traefik Host rule hostname (e.g. `api.russel.local`).
    #[serde(default)]
    pub route_host: Option<String>,
    /// Published host port (backend for Traefik).
    #[serde(default)]
    pub backend_port: Option<u16>,
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

/// One row in a service's deployment history journal.
///
/// `status` is one of: `"active"`, `"previous"`, `"superseded"`, `"failed"`,
/// `"rolled_back"`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentRecord {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
    /// RFC3339 timestamp of the deploy that produced this record.
    pub deployed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// True when this version can be targeted by explicit operator rollback
    /// (desired_state / source recorded so redeploy-from-history works).
    #[serde(default)]
    pub rollback_ready: bool,
}

/// Response for `GET /vm/{service_id}/deployments`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentsResponse {
    pub service_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<u32>,
    pub deployments: Vec<DeploymentRecord>,
}

/// Body for `POST /vm/{service_id}/rollback`.
///
/// `version = None` selects the latest entry with `status == "previous"` and
/// `rollback_ready == true`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RollbackRequest {
    #[serde(default)]
    pub version: Option<u32>,
}

// ---------------------------------------------------------------------------
// Agent API (horizontal scaling Phase 1 / #213)
// ---------------------------------------------------------------------------

/// Worker readiness reported on heartbeat.
///
/// `Ready` can receive new work; `NotReady` is draining or unhealthy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentNodeStatus {
    #[default]
    Ready,
    NotReady,
}

/// Host capacity snapshot attached to every agent heartbeat.
///
/// Units are operator-facing: memory in MiB, CPUs as logical processor counts.
/// Optional fields may be omitted when a probe is unavailable (tests, restricted
/// environments) so clients must tolerate missing data.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct NodeCapacity {
    /// Logical CPUs visible to the agent process (`available_parallelism`).
    pub cpus_total: u32,
    /// Approximate free/available memory in MiB (`MemAvailable` when present).
    pub mem_available_mb: u64,
    /// Total physical memory in MiB (`MemTotal`).
    pub mem_total_mb: u64,
    /// Count of non-reserved service dirs under the data root (observed load).
    pub running_services: u32,
    /// Host has `/dev/kvm` (microVM capable).
    pub kvm: bool,
    /// Rootless Podman appears usable (`podman info` reports rootless).
    pub rootless_podman: bool,
    /// Nix system triple when detected (e.g. `x86_64-linux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix_system: Option<String>,
}

/// Full heartbeat payload returned by `GET /agent/v1/heartbeat`.
///
/// Phase 1 skeleton: the agent *serves* this for ctrl (or ops) to poll.
/// Phase 3 may invert to agent→ctrl push; the JSON shape stays the contract.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AgentHeartbeat {
    /// Stable node identity (`RUSSEL_NODE_ID` → hostname → `local`).
    pub node_id: String,
    /// RFC3339 UTC timestamp when the sample was taken.
    pub timestamp: String,
    pub status: AgentNodeStatus,
    pub capacity: NodeCapacity,
    /// Optional operator labels (`RUSSEL_NODE_LABELS=key=val,key2=val2`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    /// Agent binary / API version string for mismatch detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
}

/// Minimal JSON body for agent routes that are not yet implemented (#214+).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentNotImplemented {
    pub error: String,
    pub phase: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
                route_host: Some("svc.russel.local".into()),
                backend_port: Some(8080),
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
            podman_args: vec!["-v".into(), "/data:/data:ro".into()],
            env: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("my-id"));
        assert!(json.contains("8080"));
        assert!(json.contains("\"runtime\":\"container\""));
        assert!(json.contains("\"podman_args\""));
        assert!(json.contains("/data:/data:ro"));
    }

    #[test]
    fn deploy_request_deserializes_podman_args_default_empty() {
        let json = r#"{"repo_url":"https://example.com/repo.git","config_path":"Russelfile.toml"}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        assert!(req.podman_args.is_empty());
    }

    #[test]
    fn deploy_request_env_roundtrip() {
        let mut env = HashMap::new();
        env.insert("LOG_LEVEL".to_string(), "debug".to_string());
        let req = DeployRequest {
            repo_url: "https://example.com/repo.git".into(),
            config_path: "Russelfile.toml".into(),
            vm_id: None,
            port: None,
            runtime: None,
            podman_args: vec![],
            env,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("LOG_LEVEL"));
        assert!(json.contains("debug"));
        let parsed: DeployRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.env.get("LOG_LEVEL"), Some(&"debug".to_string()));
    }

    #[test]
    fn deploy_request_env_defaults_empty() {
        let json = r#"{"repo_url":"https://example.com/repo.git","config_path":"Russelfile.toml"}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        assert!(req.env.is_empty());
    }

    #[test]
    fn deploy_request_podman_args_roundtrip() {
        let req = DeployRequest {
            repo_url: "https://example.com/repo.git".into(),
            config_path: "Russelfile.toml".into(),
            vm_id: None,
            port: None,
            runtime: Some(RuntimeKind::Container),
            podman_args: vec![
                "--mount".into(),
                "type=bind,source=/tmp/x,destination=/data".into(),
            ],
            env: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DeployRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.podman_args, req.podman_args);
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

    #[test]
    fn deployments_response_roundtrip() {
        let resp = DeploymentsResponse {
            service_id: "api".into(),
            active_version: Some(2),
            deployments: vec![
                DeploymentRecord {
                    version: 2,
                    generation_id: Some("abcd1234".into()),
                    status: "active".into(),
                    runtime: Some(RuntimeKind::Container),
                    deployed_at: "2026-07-29T12:00:00Z".into(),
                    store_path: Some("/nix/store/x".into()),
                    repo_url: Some("https://example.com/app.git".into()),
                    config_path: Some("Russelfile.toml".into()),
                    host_port: Some(8080),
                    guest_port: Some(3000),
                    message: Some("deploy complete".into()),
                    rollback_ready: false,
                },
                DeploymentRecord {
                    version: 1,
                    generation_id: None,
                    status: "previous".into(),
                    runtime: Some(RuntimeKind::Container),
                    deployed_at: "2026-07-28T09:00:00Z".into(),
                    store_path: None,
                    repo_url: Some("https://example.com/app.git".into()),
                    config_path: Some("Russelfile.toml".into()),
                    host_port: Some(8080),
                    guest_port: Some(3000),
                    message: None,
                    rollback_ready: true,
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DeploymentsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.active_version, Some(2));
        assert_eq!(parsed.deployments.len(), 2);
        assert!(parsed.deployments[1].rollback_ready);
    }

    #[test]
    fn rollback_request_defaults_version_none() {
        let req: RollbackRequest = serde_json::from_str("{}").unwrap();
        assert!(req.version.is_none());
        let req: RollbackRequest = serde_json::from_str(r#"{"version":3}"#).unwrap();
        assert_eq!(req.version, Some(3));
    }

    #[test]
    fn agent_heartbeat_roundtrip() {
        let mut labels = HashMap::new();
        labels.insert("zone".into(), "a".into());
        let hb = AgentHeartbeat {
            node_id: "worker-1".into(),
            timestamp: "2026-08-06T12:00:00Z".into(),
            status: AgentNodeStatus::Ready,
            capacity: NodeCapacity {
                cpus_total: 8,
                mem_available_mb: 4096,
                mem_total_mb: 16384,
                running_services: 2,
                kvm: true,
                rootless_podman: true,
                nix_system: Some("x86_64-linux".into()),
            },
            labels,
            agent_version: Some("0.1.0".into()),
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(json.contains("\"status\":\"ready\""));
        assert!(json.contains("worker-1"));
        let parsed: AgentHeartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, hb);
    }

    #[test]
    fn agent_heartbeat_deserializes_minimal() {
        let json = r#"{
            "node_id": "local",
            "timestamp": "2026-08-06T00:00:00Z",
            "status": "not_ready",
            "capacity": {
                "cpus_total": 1,
                "mem_available_mb": 0,
                "mem_total_mb": 0,
                "running_services": 0,
                "kvm": false,
                "rootless_podman": false
            }
        }"#;
        let hb: AgentHeartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(hb.status, AgentNodeStatus::NotReady);
        assert!(hb.labels.is_empty());
        assert!(hb.capacity.nix_system.is_none());
    }
}
