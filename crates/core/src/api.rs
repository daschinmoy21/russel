use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::RuntimeKind;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    pub repo_url: String,
    pub config_path: String,
    /// Optional check: when set it must equal the Russelfile `service.name`,
    /// which is the service id. It never renames the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    /// Build this commit (full 40-hex id) instead of the source's current HEAD.
    /// Update and rollback pin to the generation's recorded commit (#448).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Redeploy even when the service already runs this commit and Russelfile.
    /// Without it such a deploy is a no-op with status `unchanged`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeployResponse {
    pub service_id: String,
    pub vm_id: String,
    pub status: String,
    pub store_path: Option<String>,
    pub microvm_config_path: Option<String>,
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
    /// Commit this generation was built from; `None` for a source outside git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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

impl PortMapping {
    pub fn validate(&self) -> Result<(), String> {
        if self.host == 0 {
            return Err(
                "port.host must not be 0 (ephemeral bind is not a fixed publish port)".into(),
            );
        }
        if self.guest == 0 {
            return Err("port.guest must not be 0".into());
        }
        Ok(())
    }
}

/// Control-plane service status vocabulary.
///
/// This is the shared machine for the `status` / `vm_state` strings used across
/// ctrl's in-memory state and the wire API. It serializes as snake_case so the
/// JSON contract stays byte-identical to the historical string literals.
///
/// ctrl's `ServiceState.status` uses the lifecycle subset (`building`,
/// `deployed`, `failed`, `stopping`, `destroying`, `stopped`, `detached`,
/// `idle`); `ServiceState.vm_state` uses the liveness subset (`running`,
/// `failed`, `pending`, `orphaned`) plus [`VmState::None`]. The agent wire
/// status additionally reports `"running"` / `"stopped"` / `"destroyed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Pending,
    Building,
    Deployed,
    Running,
    Stopping,
    Stopped,
    Failed,
    Detached,
    Orphaned,
    Destroying,
    #[default]
    Idle,
}

impl ServiceStatus {
    /// The snake_case wire string for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Building => "building",
            Self::Deployed => "deployed",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Detached => "detached",
            Self::Orphaned => "orphaned",
            Self::Destroying => "destroying",
            Self::Idle => "idle",
        }
    }
}

impl std::fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<ServiceStatus> for &'static str {
    fn from(s: ServiceStatus) -> &'static str {
        s.as_str()
    }
}

impl std::str::FromStr for ServiceStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "building" => Ok(Self::Building),
            "deployed" => Ok(Self::Deployed),
            "running" => Ok(Self::Running),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            "failed" => Ok(Self::Failed),
            "detached" => Ok(Self::Detached),
            "orphaned" => Ok(Self::Orphaned),
            "destroying" => Ok(Self::Destroying),
            "idle" => Ok(Self::Idle),
            _ => Err(format!("unknown service status: {s}")),
        }
    }
}

impl TryFrom<&str> for ServiceStatus {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Control-plane VM liveness vocabulary (`ServiceState.vm_state`).
///
/// Distinct from [`ServiceStatus`] because the liveness machine has its own
/// values: [`VmState::None`] means "no VM process state at all" (e.g. a stopped
/// or never-booted service), which is not a valid `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    #[default]
    None,
    Running,
    Failed,
    Pending,
    Orphaned,
}

impl VmState {
    /// The snake_case wire string for this VM state.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::Pending => "pending",
            Self::Orphaned => "orphaned",
        }
    }
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<VmState> for &'static str {
    fn from(s: VmState) -> &'static str {
        s.as_str()
    }
}

impl std::str::FromStr for VmState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "running" => Ok(Self::Running),
            "failed" => Ok(Self::Failed),
            "pending" => Ok(Self::Pending),
            "orphaned" => Ok(Self::Orphaned),
            _ => Err(format!("unknown vm_state: {s}")),
        }
    }
}

impl TryFrom<&str> for VmState {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
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
    #[serde(default)]
    pub route_host: Option<String>,
    /// Times ctrl relaunched this microVM under `restart = "unless-stopped"`
    /// since its last deploy. Omitted when zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    /// Limits the Russelfile asked for, from the last deploy's metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<ResourceLimits>,
    /// Limits the runtime applied. Absent for deploys recorded before this
    /// field existed; `cpus: None` there means no CPU limit is enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective: Option<ResourceLimits>,
}

/// Resource limits for a service. `None` means unset (requested) or not
/// enforced (effective).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResourceLimits {
    /// Memory in MiB.
    #[serde(default)]
    pub memory_mb: Option<u16>,
    /// vCPUs (microVM) or the `--cpus` limit (container).
    #[serde(default)]
    pub cpus: Option<u8>,
}

impl StatusResponse {
    /// Human-readable notes for each limit the runtime did not apply as
    /// requested. Empty when nothing was recorded or everything matched.
    pub fn unapplied_limits(&self) -> Vec<String> {
        let (Some(req), Some(eff)) = (self.requested, self.effective) else {
            return Vec::new();
        };
        let mut notes = Vec::new();
        if let Some(want) = req.cpus
            && eff.cpus != Some(want)
        {
            notes.push(match eff.cpus {
                None => format!("cpus: requested {want}, not applied (no cpu limit)"),
                Some(got) => format!("cpus: requested {want}, applied {got}"),
            });
        }
        if let (Some(want), Some(got)) = (req.memory_mb, eff.memory_mb)
            && want != got
        {
            notes.push(format!("memory: requested {want} MiB, applied {got} MiB"));
        }
        notes
    }
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
    /// Full commit this version was built from; absent when not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The deployed tree had changes `rev` does not contain. Absent when
    /// `rev` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
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
    /// Build the version's recorded commit from source instead of
    /// relaunching its recorded build output (#558).
    #[serde(default)]
    pub rebuild: bool,
}

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
    /// Count of **running** service instances under the data root (not merely
    /// deployed): microVMs with a live `vm_pid`, containers with a non-empty
    /// `container_id` in on-disk metadata.
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
///
/// Note: not yet consumed by ctrl — the agent exposes the wire endpoint but
/// nothing polls it today (write-only API; see audit P1).
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

/// Result of an agent lifecycle operation (`POST /agent/v1/stop|destroy/{id}`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentLifecycleResponse {
    pub service_id: String,
    /// `"stop"` or `"destroy"`.
    pub operation: String,
    /// `"stopped"` or `"destroyed"`.
    pub status: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
}

/// Worker-side service status (`GET /agent/v1/status/{service_id}`).
///
/// `status` / `vm_state` are `"running"` or `"stopped"` — the agent probes
/// local process state (metadata PID liveness for microVM, `podman ps` for
/// containers). Wall-clock uptime is best-effort from `/proc`; control-plane
/// in-memory state may report richer status strings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentStatusResponse {
    pub service_id: String,
    /// `"running"` or `"stopped"`.
    pub status: String,
    pub vm_state: String,
    pub uptime_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_port: Option<u16>,
}

/// Structured error body returned by agent RPC routes (4xx/5xx).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentErrorResponse {
    pub error: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn port_mapping_rejects_host_zero() {
        let err = PortMapping {
            host: 0,
            guest: 3000,
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("host"));
        assert!(err.contains("0"));
    }

    #[test]
    fn port_mapping_rejects_guest_zero() {
        let err = PortMapping {
            host: 8080,
            guest: 0,
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("guest"));
        assert!(err.contains("0"));
    }

    #[test]
    fn port_mapping_valid() {
        PortMapping {
            host: 8080,
            guest: 3000,
        }
        .validate()
        .unwrap();
    }

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
                rev: None,
            })),
            DeployEvent::Error("build failed".into()),
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let _parsed: DeployEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn deploy_request_roundtrip_carries_only_source_and_id_check() {
        let req = DeployRequest {
            repo_url: "https://github.com/example/repo.git".into(),
            config_path: "Russelfile.toml".into(),
            vm_id: Some("my-id".into()),
            rev: None,
            force: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DeployRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.vm_id.as_deref(), Some("my-id"));

        let json = r#"{"repo_url":"https://example.com/repo.git","config_path":"Russelfile.toml"}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        assert!(req.vm_id.is_none());
        assert!(!serde_json::to_string(&req).unwrap().contains("vm_id"));
    }

    /// The Russelfile is the whole desired state (#447): former override
    /// fields are rejected instead of silently ignored.
    #[test]
    fn deploy_request_rejects_removed_override_fields() {
        for field in [
            r#""port":{"host":8080,"guest":3000}"#,
            r#""host":"api.example.com""#,
            r#""runtime":"container""#,
            r#""env":{"A":"1"}"#,
            r#""podman_args":["-v","/a:/b"]"#,
        ] {
            let json = format!(
                r#"{{"repo_url":"https://example.com/repo.git","config_path":"Russelfile.toml",{field}}}"#
            );
            let err = serde_json::from_str::<DeployRequest>(&json).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{field}: {err}");
        }
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
            route_host: None,
            restarts: None,
            requested: None,
            effective: None,
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
            route_host: Some("api.example.com".into()),
            restarts: None,
            requested: None,
            effective: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"runtime\":\"container\""));
        let parsed: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.runtime, Some(RuntimeKind::Container));
        assert_eq!(parsed.host_port, Some(3100));
        assert_eq!(parsed.route_host.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn status_response_route_host_defaults_for_legacy_json() {
        let parsed: StatusResponse = serde_json::from_str(
            r#"{
                "service_id":"api",
                "status":"deployed",
                "vm_state":"running",
                "uptime_seconds":42
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.route_host, None);
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
                    rev: Some("0123456789abcdef0123456789abcdef01234567".into()),
                    dirty: Some(true),
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
                    rev: None,
                    dirty: None,
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DeploymentsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.active_version, Some(2));
        assert_eq!(parsed.deployments.len(), 2);
        assert!(parsed.deployments[1].rollback_ready);
        assert_eq!(parsed.deployments[0].dirty, Some(true));
        assert_eq!(json.matches("\"rev\"").count(), 1);

        // Older servers omit both fields.
        let old: DeploymentRecord = serde_json::from_str(
            r#"{"version":1,"status":"active","deployed_at":"2026-07-28T09:00:00Z"}"#,
        )
        .unwrap();
        assert!(old.rev.is_none() && old.dirty.is_none());
    }

    #[test]
    fn rollback_request_defaults_version_none() {
        let req: RollbackRequest = serde_json::from_str("{}").unwrap();
        assert!(req.version.is_none());
        let req: RollbackRequest = serde_json::from_str(r#"{"version":3}"#).unwrap();
        assert_eq!(req.version, Some(3));
        assert!(!req.rebuild);
        let req: RollbackRequest = serde_json::from_str(r#"{"rebuild":true}"#).unwrap();
        assert!(req.rebuild);
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

    #[test]
    fn agent_lifecycle_response_roundtrip() {
        let resp = AgentLifecycleResponse {
            service_id: "svc-a".into(),
            operation: "stop".into(),
            status: "stopped".into(),
            message: "stopped container svc-a".into(),
            runtime: Some(RuntimeKind::Container),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"runtime\":\"container\""));
        let parsed: AgentLifecycleResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn agent_status_response_roundtrip_and_optional_runtime() {
        let resp = AgentStatusResponse {
            service_id: "svc-a".into(),
            status: "running".into(),
            vm_state: "running".into(),
            uptime_seconds: 37,
            runtime: None,
            host_port: Some(8080),
            guest_port: Some(3000),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: AgentStatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
        // runtime omitted from JSON when None (skip_serializing_if).
        assert!(!json.contains("runtime"));
    }

    #[test]
    fn agent_error_response_roundtrip() {
        let err = AgentErrorResponse {
            error: "service svc-a not found".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let parsed: AgentErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, err);
    }

    #[test]
    fn service_status_serde_roundtrip_every_variant() {
        let variants = [
            ServiceStatus::Pending,
            ServiceStatus::Building,
            ServiceStatus::Deployed,
            ServiceStatus::Running,
            ServiceStatus::Stopping,
            ServiceStatus::Stopped,
            ServiceStatus::Failed,
            ServiceStatus::Detached,
            ServiceStatus::Orphaned,
            ServiceStatus::Destroying,
            ServiceStatus::Idle,
        ];
        let expected = [
            "pending",
            "building",
            "deployed",
            "running",
            "stopping",
            "stopped",
            "failed",
            "detached",
            "orphaned",
            "destroying",
            "idle",
        ];
        for (variant, wire) in variants.iter().zip(expected) {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(
                json,
                format!("\"{wire}\""),
                "wire format must stay snake_case"
            );
            assert_eq!(variant.as_str(), wire);
            let parsed: ServiceStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }
    }

    #[test]
    fn service_status_from_str_roundtrip_and_unknown() {
        for s in [
            "pending",
            "building",
            "deployed",
            "running",
            "stopping",
            "stopped",
            "failed",
            "detached",
            "orphaned",
            "destroying",
            "idle",
        ] {
            let parsed: ServiceStatus = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
            assert_eq!(parsed.to_string(), s);
            assert_eq!(ServiceStatus::try_from(s).unwrap(), parsed);
        }
        assert!("rolled_back".parse::<ServiceStatus>().is_err());
        assert!(ServiceStatus::try_from("rolled_back").is_err());
    }

    #[test]
    fn vm_state_serde_roundtrip_every_variant() {
        let variants = [
            VmState::None,
            VmState::Running,
            VmState::Failed,
            VmState::Pending,
            VmState::Orphaned,
        ];
        let expected = ["none", "running", "failed", "pending", "orphaned"];
        for (variant, wire) in variants.iter().zip(expected) {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(
                json,
                format!("\"{wire}\""),
                "wire format must stay snake_case"
            );
            assert_eq!(variant.as_str(), wire);
            let parsed: VmState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *variant);
        }
    }

    #[test]
    fn vm_state_from_str_unknown_is_err() {
        assert!("deployed".parse::<VmState>().is_err());
        assert!("".parse::<VmState>().is_err());
        assert!("NONE".parse::<VmState>().is_err());
        assert!(VmState::try_from("orphaned").is_ok());
    }

    fn status_with(
        requested: Option<ResourceLimits>,
        effective: Option<ResourceLimits>,
    ) -> StatusResponse {
        StatusResponse {
            service_id: "svc".into(),
            status: "deployed".into(),
            vm_state: "running".into(),
            uptime_seconds: 0,
            runtime: None,
            host_port: None,
            guest_port: None,
            route_host: None,
            restarts: None,
            requested,
            effective,
        }
    }

    #[test]
    fn unapplied_limits_reports_dropped_cpus_and_raised_memory() {
        let req = ResourceLimits {
            memory_mb: Some(128),
            cpus: Some(2),
        };
        let eff = ResourceLimits {
            memory_mb: Some(256),
            cpus: None,
        };
        let notes = status_with(Some(req), Some(eff)).unapplied_limits();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("not applied"));
        assert!(notes[1].contains("256 MiB"));
    }

    #[test]
    fn unapplied_limits_empty_when_matching_or_unrecorded() {
        let same = ResourceLimits {
            memory_mb: Some(256),
            cpus: Some(1),
        };
        assert!(
            status_with(Some(same), Some(same))
                .unapplied_limits()
                .is_empty()
        );
        assert!(status_with(Some(same), None).unapplied_limits().is_empty());
        let legacy: StatusResponse = serde_json::from_str(
            r#"{"service_id":"a","status":"idle","vm_state":"none","uptime_seconds":0}"#,
        )
        .unwrap();
        assert!(legacy.requested.is_none() && legacy.effective.is_none());
    }
}
