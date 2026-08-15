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

/// Resolve TAP / IP identity for teardown without registering a subnet lease.
///
/// Priority:
/// 1. Complete on-disk metadata (`tap_id`, `host_ip`, `vm_ip`)
/// 2. Registered lease via [`crate::network::lookup_subnet`]
/// 3. `None` — never invent preferred_subnet (hash collisions can hit another service)
pub(super) fn network_alloc_for_service(
    service_id: &str,
) -> Option<crate::network::SubnetAllocation> {
    if let Some(meta) = read_metadata(service_id)
        && let (Some(tap_id), Some(host_ip), Some(vm_ip)) = (meta.tap_id, meta.host_ip, meta.vm_ip)
    {
        // MAC only from host_ip key or this service's registry lease — never
        // invent preferred_subnet MAC (collision-sensitive, not authoritative).
        // Metadata TAP/IP alone without an authoritative MAC is not safe for teardown.
        let mac = crate::network::network_key_from_host_ip(&host_ip)
            .map(|k| crate::network::allocation_from_network_key(k).mac)
            .or_else(|| crate::network::lookup_subnet(service_id).map(|a| a.mac))?;
        return Some(crate::network::SubnetAllocation {
            host_ip,
            vm_ip,
            mac,
            tap_id,
        });
    }
    crate::network::lookup_subnet(service_id)
}

/// Authoritative TAP for stop process-selection: metadata TAP first, then this
/// service's registry lease. Never invents a preferred hash TAP.
pub(super) fn stop_tap_identity(service_id: &str, metadata_tap: Option<&str>) -> Option<String> {
    if let Some(tap) = metadata_tap.filter(|t| !t.is_empty()) {
        return Some(tap.to_string());
    }
    crate::network::lookup_subnet(service_id).map(|a| a.tap_id)
}

/// Escape a literal for embedding in a `pkill -f` ERE pattern.
///
/// Service IDs are normally constrained by `validate_service_id`, but this
/// helper is independently callable — unescaped metacharacters could broaden
/// the match and terminate another Cloud Hypervisor process.
pub(super) fn escape_pkill_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // POSIX ERE metacharacters (and common GNU extensions).
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// `pkill -f` pattern for Cloud Hypervisor stop fallback.
///
/// - With a known TAP: match that TAP only (literal-escaped).
/// - Without: match the service path marker under `russel/{service_id}/` so
///   cleanup cannot select another service's CH process via preferred hash.
///   `service_id` is regex-escaped so metacharacters cannot broaden the match.
pub(super) fn cloud_hypervisor_stop_pattern(service_id: &str, tap: Option<&str>) -> String {
    match tap {
        Some(tap) => {
            let tap = escape_pkill_literal(tap);
            format!("(^|[[:space:]])cloud-hypervisor .*tap={tap}(,|$)")
        }
        None => {
            let sid = escape_pkill_literal(service_id);
            format!("(^|[[:space:]])cloud-hypervisor .*russel/{sid}/")
        }
    }
}

/// Owned TAP ids from metadata and/or registry lease (no preferred invent).
fn owned_tap_ids(service_id: &str) -> Vec<String> {
    let mut tap_ids = Vec::new();
    if let Some(meta) = read_metadata(service_id)
        && let Some(tap) = meta.tap_id
    {
        tap_ids.push(tap);
    }
    if let Some(alloc) = crate::network::lookup_subnet(service_id)
        && !tap_ids.contains(&alloc.tap_id)
    {
        tap_ids.push(alloc.tap_id);
    }
    tap_ids
}

/// Trusted executable basename from `/proc/{pid}/exe` (not user-controlled argv0).
///
/// Linux may report `"path (deleted)"` when the binary was unlinked; strip that
/// suffix before taking the basename.
fn proc_exe_basename(pid: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let s = link.to_string_lossy();
    let s = s.strip_suffix(" (deleted)").unwrap_or(&s);
    std::path::Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
}

/// True when `arg` is exactly `tap={id}` or `tap={id},...` (CH multi-device form).
fn arg_matches_tap(arg: &str, tap_id: &str) -> bool {
    let tap_arg = format!("tap={tap_id}");
    arg == tap_arg || arg.starts_with(&format!("{tap_arg},"))
}

/// Socat process title / argv token bound to this service.
///
/// Accept only the exact title (or a trailing space boundary). Do **not** treat
/// `-` as a boundary: service IDs contain hyphens, so `socat-russel-foo-bar`
/// must not match service `foo`.
fn arg_matches_socat_title(arg: &str, service_id: &str) -> bool {
    let needle = format!("socat-russel-{service_id}");
    if arg == needle {
        return true;
    }
    arg.strip_prefix(&needle)
        .is_some_and(|rest| rest.starts_with(' '))
}

/// Verify PID ownership via trusted `/proc/<pid>/exe` + cmdline markers.
///
/// Executable identity comes only from `/proc/{pid}/exe` (kernel-resolved), never
/// from cmdline argv0. Cmdline is used only for owned TAP, path, and socat title
/// markers. Never registers a lease and never invents preferred_subnet.
pub(super) fn verify_process_ownership(pid: u32, service_id: &str) -> bool {
    let Some(base) = proc_exe_basename(pid) else {
        return false;
    };

    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(raw) = std::fs::read(cmdline_path) else {
        return false;
    };
    let args: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    if args.is_empty() {
        return false;
    }

    let tap_ids = owned_tap_ids(service_id);
    let trusted_path = format!("/var/lib/russel/{service_id}/");
    let path_marker = format!("russel/{service_id}/");

    // cloud-hypervisor: real executable + owned TAP device argument.
    if base == "cloud-hypervisor" {
        return args
            .iter()
            .any(|arg| tap_ids.iter().any(|tap| arg_matches_tap(arg, tap)));
    }

    // virtiofsd: real executable + trusted socket/path under this service tree.
    if base == "virtiofsd" {
        return args
            .iter()
            .any(|arg| arg.contains(&trusted_path) || arg.contains(&path_marker));
    }

    // socat: real executable must be socat; retitled argv0 is only a title marker
    // (spawned with `.arg0("socat-russel-{id}")` while the binary remains socat).
    if base == "socat" {
        return args
            .iter()
            .any(|arg| arg_matches_socat_title(arg, service_id));
    }

    // No unscoped substring match: unrelated processes that merely mention the
    // service id in an argument (or spoof argv0) must not pass ownership checks.
    false
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod ownership_tests {
    use super::*;

    #[test]
    fn socat_title_exact_match_only() {
        assert!(arg_matches_socat_title("socat-russel-foo", "foo"));
        assert!(arg_matches_socat_title(
            "socat-russel-foo TCP-LISTEN",
            "foo"
        ));
        // Hyphen is part of another service id — must not prefix-match.
        assert!(!arg_matches_socat_title("socat-russel-foo-bar", "foo"));
        assert!(!arg_matches_socat_title("socat-russel-foobar", "foo"));
        assert!(arg_matches_socat_title("socat-russel-foo-bar", "foo-bar"));
    }

    #[test]
    fn spoofed_argv0_without_real_exe_is_rejected() {
        // Current process can set nothing about /proc/self/exe — it is not socat/
        // cloud-hypervisor/virtiofsd, so even a crafted service id must fail.
        let me = std::process::id();
        assert!(!verify_process_ownership(me, "api"));
        assert!(!verify_process_ownership(me, "any-service"));
    }

    #[test]
    fn retitled_shell_is_not_owned_socat() {
        // Mimic a malicious/stale process: argv0 = socat-russel-{id} but real
        // executable is /bin/sh (as spawn does with CommandExt::arg0).
        let mut command = std::process::Command::new("/bin/sh");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0("socat-russel-api");
        }
        let mut child = command
            .args(["-c", "sleep 5"])
            .spawn()
            .expect("spawn retitled shell");
        let pid = child.id();
        // Give the kernel a moment to publish /proc entries.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !verify_process_ownership(pid, "api"),
            "retitled non-socat binary must not be treated as owned socat"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
