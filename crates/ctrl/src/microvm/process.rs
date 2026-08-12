//! Process metadata, BootOutput, and process lifecycle helpers.

use std::time::Duration;

use tokio::process::Command;

// ── BootOutput ───────────────────────────────────────────────────────────────

/// Output of `MicrovmRunner::boot()` / `boot_vm()`.
pub struct BootOutput {
    pub vm_child: tokio::process::Child,
    /// All virtiofsd children (one for nixstore, optionally one for config).
    pub virtiofsd_children: Vec<tokio::process::Child>,
}

// ── Process metadata (reused by stop/destroy) ───────────────────────────────

#[derive(Debug, Default)]
pub(super) struct ProcessMetadata {
    pub(super) vm_pid: Option<u32>,
    pub(super) virtiofsd_pids: Vec<u32>,
    pub(super) socat_pid: Option<u32>,
    pub(super) tap_id: Option<String>,
    pub(super) host_ip: Option<String>,
    pub(super) vm_ip: Option<String>,
}

pub(super) fn read_metadata(service_id: &str) -> Option<ProcessMetadata> {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let virtiofsd_pids = value
        .get("virtiofsd_pids")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
        .or_else(|| {
            // Legacy: singular virtiofsd_pid field.
            value
                .get("virtiofsd_pid")
                .and_then(|pid| pid.as_u64())
                .map(|pid| vec![pid as u32])
        })
        .unwrap_or_default();
    Some(ProcessMetadata {
        vm_pid: value
            .get("vm_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        virtiofsd_pids,
        socat_pid: value
            .get("socat_pid")
            .and_then(|pid| pid.as_u64())
            .map(|pid| pid as u32),
        tap_id: value
            .get("tap_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        host_ip: value
            .get("host_ip")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        vm_ip: value
            .get("vm_ip")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Prefer generation-recorded TAP / IPs so destroy still works after promote.
///
/// Does not register a subnet lease. Metadata is the source of truth for
/// teardown; without it, use a read-only registry lookup, then the unregistered
/// preferred key only for best-effort cleanup identifiers.
pub(super) fn network_alloc_for_service(service_id: &str) -> crate::network::SubnetAllocation {
    if let Some(meta) = read_metadata(service_id)
        && let (Some(tap_id), Some(host_ip), Some(vm_ip)) = (meta.tap_id, meta.host_ip, meta.vm_ip)
    {
        let mac = crate::network::network_key_from_host_ip(&host_ip)
            .map(|k| crate::network::allocation_from_network_key(k).mac)
            .unwrap_or_else(|| {
                crate::network::lookup_subnet(service_id)
                    .unwrap_or_else(|| crate::network::preferred_subnet(service_id))
                    .mac
            });
        return crate::network::SubnetAllocation {
            host_ip,
            vm_ip,
            mac,
            tap_id,
        };
    }
    crate::network::lookup_subnet(service_id)
        .unwrap_or_else(|| crate::network::preferred_subnet(service_id))
}

/// Verify PID ownership via /proc/<pid>/cmdline.
pub(super) fn verify_process_ownership(pid: u32, service_id: &str) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    // Prefer metadata TAP; fall back to preferred (unregistered) key for matching.
    let tap_id = network_alloc_for_service(service_id).tap_id;
    let tap_arg = format!("tap={tap_id}");
    std::fs::read(cmdline_path)
        .map(|cmdline| {
            cmdline.split(|byte| *byte == 0).any(|arg| {
                let arg = String::from_utf8_lossy(arg);
                arg == tap_arg
                    || arg.starts_with(&format!("{tap_arg},"))
                    || arg.contains(&format!("russel/{service_id}/"))
                    || arg.contains(&format!("socat-russel-{service_id}"))
            })
        })
        .unwrap_or(false)
}

pub(super) async fn terminate_owned_process(pid: u32, service_id: &str) -> anyhow::Result<bool> {
    if !verify_process_ownership(pid, service_id) {
        return Ok(false);
    }
    let output = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await?;
    if !output.status.success() && process_is_alive(pid) {
        anyhow::bail!(
            "failed to terminate process {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(wait_for_process_exit(pid, Duration::from_secs(2)).await)
}

pub(super) fn process_is_alive(pid: u32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(path) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

pub(super) async fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while process_is_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !process_is_alive(pid)
}
