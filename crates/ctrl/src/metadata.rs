use std::path::{Path, PathBuf};

use russel_core::config::RuntimeKind;

pub const SCHEMA_VERSION: u32 = 1;

/// On-disk path for a service's metadata.json.
pub fn metadata_path(service_id: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/russel/{service_id}/metadata.json"))
}

/// Fields commonly loaded from on-disk metadata for API rehydration.
#[derive(Debug, Clone, Default)]
pub struct LoadedMetadata {
    pub runtime: Option<RuntimeKind>,
    pub host_port: Option<u16>,
    pub guest_port: Option<u16>,
    pub container_id: Option<String>,
}

/// Parse `runtime` from on-disk metadata JSON; legacy entries without the field
/// default to microVM.
pub fn prior_runtime_from_metadata(content: &str) -> RuntimeKind {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return RuntimeKind::Microvm,
    };
    value
        .get("runtime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(RuntimeKind::Microvm)
}

pub fn prior_runtime_from_disk(service_id: &str) -> RuntimeKind {
    let path = metadata_path(service_id);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return RuntimeKind::Microvm;
    };
    prior_runtime_from_metadata(&content)
}

pub fn load_metadata_from_disk(service_id: &str) -> Option<LoadedMetadata> {
    let content = std::fs::read_to_string(metadata_path(service_id)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some(LoadedMetadata {
        runtime: value
            .get("runtime")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok()),
        host_port: value.get("host_port").and_then(|v| v.as_u64()).map(|p| p as u16),
        guest_port: value
            .get("guest_port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16),
        container_id: value
            .get("container_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Resolve lifecycle runtime: prefer in-memory state, else on-disk metadata.
pub fn resolve_lifecycle_runtime(
    state_runtime: Option<RuntimeKind>,
    service_id: &str,
) -> RuntimeKind {
    state_runtime.unwrap_or_else(|| prior_runtime_from_disk(service_id))
}

/// Build versioned metadata JSON for a microVM deployment.
#[allow(clippy::too_many_arguments)]
pub fn build_microvm_metadata(
    service_id: &str,
    host_port: u16,
    guest_port: u16,
    vm_ip: &str,
    vm_pid: Option<u32>,
    virtiofsd_pid: Option<u32>,
    socat_pid: Option<u32>,
    kernel_path: &str,
    store_path: &str,
    mem_mb: u16,
    bin_name: Option<&str>,
    initramfs_path: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "service_id": service_id,
        "runtime": "microvm",
        "host_port": host_port,
        "guest_port": guest_port,
        "vm_ip": vm_ip,
        "kernel_path": kernel_path,
        "store_path": store_path,
        "mem_mb": mem_mb,
        "deployed_at": deployed_at_now(),
    });
    if let Some(pid) = vm_pid {
        meta["vm_pid"] = serde_json::json!(pid);
    }
    if let Some(pid) = virtiofsd_pid {
        meta["virtiofsd_pid"] = serde_json::json!(pid);
    }
    if let Some(pid) = socat_pid {
        meta["socat_pid"] = serde_json::json!(pid);
    }
    if let Some(name) = bin_name {
        meta["bin_name"] = serde_json::json!(name);
    }
    if let Some(path) = initramfs_path {
        meta["initramfs"] = serde_json::json!(path);
    }
    meta
}

/// Build versioned metadata JSON for a container deployment.
pub fn build_container_metadata(
    service_id: &str,
    host_port: u16,
    guest_port: u16,
    store_path: &str,
    container_id: &str,
    container_name: &str,
    rootfs_path: &str,
    mem_mb: u16,
    bin_name: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "service_id": service_id,
        "runtime": "container",
        "host_port": host_port,
        "guest_port": guest_port,
        "store_path": store_path,
        "container_id": container_id,
        "container_name": container_name,
        "rootfs_path": rootfs_path,
        "mem_mb": mem_mb,
        "deployed_at": deployed_at_now(),
    });
    if let Some(name) = bin_name {
        meta["bin_name"] = serde_json::json!(name);
    }
    meta
}

pub fn write_metadata(path: impl AsRef<Path>, metadata: &serde_json::Value) -> anyhow::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!("failed to create metadata parent {}: {}", parent.display(), e)
        })?;
    }
    let content = serde_json::to_string_pretty(metadata)
        .map_err(|e| anyhow::anyhow!("failed to serialize metadata: {}", e))?;
    std::fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("failed to write metadata to {}: {}", path.display(), e))
}

/// Current UTC time as RFC3339 (second precision).
pub fn deployed_at_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    rfc3339_from_unix(secs)
}

fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let h = (secs / 3_600) % 24;
    let min = (secs / 60) % 60;
    let s = secs % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_includes_schema_version() {
        let meta = build_microvm_metadata(
            "api",
            3100,
            3000,
            "10.0.1.2",
            Some(42),
            Some(43),
            Some(44),
            "/nix/store/kernel",
            "/nix/store/app",
            512,
            Some("myapp"),
            Some("/var/lib/russel/api/initramfs.cpio"),
        );
        assert_eq!(meta["schema_version"], SCHEMA_VERSION);
        assert_eq!(meta["runtime"], "microvm");
        assert_eq!(meta["bin_name"], "myapp");
        assert!(meta["deployed_at"].as_str().unwrap().ends_with('Z'));

        let container = build_container_metadata(
            "api",
            3100,
            3000,
            "/nix/store/app",
            "abc123",
            "russel-api",
            "/var/lib/russel/api/rootfs",
            512,
            Some("myapp"),
        );
        assert_eq!(container["schema_version"], SCHEMA_VERSION);
        assert_eq!(container["runtime"], "container");
    }

    #[test]
    fn prior_runtime_defaults_to_microvm_when_missing() {
        let json = r#"{"service_id":"api","host_port":3100}"#;
        assert_eq!(prior_runtime_from_metadata(json), RuntimeKind::Microvm);
    }

    #[test]
    fn prior_runtime_reads_container_field() {
        let json = r#"{"runtime":"container","service_id":"api"}"#;
        assert_eq!(prior_runtime_from_metadata(json), RuntimeKind::Container);
    }

    #[test]
    fn prior_runtime_reads_microvm_field() {
        let json = r#"{"runtime":"microvm","service_id":"api"}"#;
        assert_eq!(prior_runtime_from_metadata(json), RuntimeKind::Microvm);
    }

    #[test]
    fn prior_runtime_invalid_json_defaults_microvm() {
        assert_eq!(prior_runtime_from_metadata("not json"), RuntimeKind::Microvm);
    }

    #[test]
    fn resolve_lifecycle_runtime_prefers_state() {
        assert_eq!(
            resolve_lifecycle_runtime(Some(RuntimeKind::Container), "api"),
            RuntimeKind::Container
        );
    }

    #[test]
    fn load_metadata_parses_container_fields() {
        let json = serde_json::to_string(&build_container_metadata(
            "api",
            3100,
            3000,
            "/nix/store/app",
            "abc",
            "russel-api",
            "/var/lib/russel/api/rootfs",
            512,
            None,
        ))
        .unwrap();
        assert_eq!(prior_runtime_from_metadata(&json), RuntimeKind::Container);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["host_port"], 3100);
        assert_eq!(value["guest_port"], 3000);
        assert_eq!(value["container_id"], "abc");
    }
}