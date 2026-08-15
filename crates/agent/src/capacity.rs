//! Host capacity probes for agent heartbeats.
//!
//! Reads Linux `/proc` and filesystem checks only — no heavy sysinfo crate.
//! Each probe is best-effort so a missing `/proc` (tests, weird hosts) still
//! yields a usable zeroed snapshot.
//!
//! Subprocess probes (`podman`, `nix`) are **timeout-bounded** and **TTL-cached**
//! so a slow/hung binary cannot stall every heartbeat.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use russel_core::api::NodeCapacity;
use russel_core::reserved::is_reserved_service_dir;
use tokio::process::Command;

/// Default Russel data root (service dirs live under here).
pub const DEFAULT_DATA_ROOT: &str = "/var/lib/russel";

/// Bound for `podman info` (rootless detection).
const PODMAN_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Bound for `nix eval … builtins.currentSystem`.
const NIX_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Reuse probe results across heartbeats so we do not re-spawn every request.
const PROBE_CACHE_TTL: Duration = Duration::from_secs(45);

/// Process-local cache for expensive capacity subprocesses.
struct ProbeCache {
    last_podman: Option<(Instant, bool)>,
    last_nix: Option<(Instant, Option<String>)>,
}

fn probe_cache() -> &'static Mutex<ProbeCache> {
    static CACHE: OnceLock<Mutex<ProbeCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(ProbeCache {
            last_podman: None,
            last_nix: None,
        })
    })
}

/// Collect a capacity snapshot.
///
/// `data_root` is where service directories live (default `/var/lib/russel`).
/// `podman_probe` when false skips the `podman info` subprocess (tests).
pub async fn collect_capacity(data_root: &Path, podman_probe: bool) -> NodeCapacity {
    let (mem_total_mb, mem_available_mb) = read_meminfo_mb();
    let cpus_total = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let running_services = count_running_services(data_root);
    let kvm = Path::new("/dev/kvm").exists();
    let rootless_podman = if podman_probe {
        probe_rootless_podman_cached().await
    } else {
        false
    };
    let nix_system = detect_nix_system_cached().await;

    NodeCapacity {
        cpus_total,
        mem_available_mb,
        mem_total_mb,
        running_services,
        kvm,
        rootless_podman,
        nix_system,
    }
}

/// Parse `MemTotal` / `MemAvailable` from `/proc/meminfo` into MiB.
fn read_meminfo_mb() -> (u64, u64) {
    read_meminfo_mb_from(Path::new("/proc/meminfo"))
}

/// Testable meminfo parser (path injectable).
pub fn read_meminfo_mb_from(path: &Path) -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut total_kb: Option<u64> = None;
    let mut avail_kb: Option<u64> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_meminfo_kb(rest);
        }
    }
    let total_mb = total_kb.unwrap_or(0) / 1024;
    // Fall back to MemFree-ish if MemAvailable missing (old kernels): use 0.
    let avail_mb = avail_kb.unwrap_or(0) / 1024;
    (total_mb, avail_mb)
}

fn parse_meminfo_kb(rest: &str) -> Option<u64> {
    // Format: "   16384000 kB"
    let mut parts = rest.split_whitespace();
    let n = parts.next()?.parse::<u64>().ok()?;
    Some(n)
}

/// Count services under `data_root` that look **actually running**.
///
/// Only non-reserved dirs with a readable `metadata.json` are considered.
/// Running is determined from on-disk metadata signals (same fields ctrl writes):
/// - **microVM:** `vm_pid` present and process still alive (`/proc/{pid}` exists).
/// - **container:** non-empty `container_id` (heartbeat stays cheap — no per-container
///   `podman inspect`; absence of `container_id` means not running / never started).
///
/// Metadata-only deploys with neither live `vm_pid` nor `container_id` are **not**
/// counted (deployed ≠ running).
pub fn count_running_services(data_root: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(data_root) else {
        return 0;
    };
    let mut n = 0u32;
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_reserved_service_dir(name) {
            continue;
        }
        let Ok(ft) = ent.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let meta_path = ent.path().join("metadata.json");
        if !meta_path.is_file() {
            continue;
        }
        if service_metadata_looks_running(&meta_path) {
            n = n.saturating_add(1);
        }
    }
    n
}

/// True when metadata indicates a live microVM or a started container.
fn service_metadata_looks_running(meta_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(meta_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    service_value_looks_running(&value)
}

/// Pure helper for tests: decide running from a metadata JSON value.
pub fn service_value_looks_running(value: &serde_json::Value) -> bool {
    let runtime = value.get("runtime").and_then(|v| v.as_str());
    let vm_pid = value
        .get("vm_pid")
        .and_then(|v| v.as_u64())
        .and_then(|p| u32::try_from(p).ok());
    let container_id = value
        .get("container_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match runtime {
        Some("container") => container_id.is_some(),
        Some("microvm") => vm_pid.is_some_and(pid_is_alive),
        // Legacy / missing runtime: accept either authoritative live signal.
        _ => {
            if let Some(pid) = vm_pid
                && pid_is_alive(pid)
            {
                return true;
            }
            container_id.is_some()
        }
    }
}

/// Linux: process exists if `/proc/{pid}` is present.
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Cached rootless-podman probe (TTL); on miss runs timeout-bounded subprocess.
async fn probe_rootless_podman_cached() -> bool {
    {
        let cache = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, val)) = cache.last_podman
            && at.elapsed() < PROBE_CACHE_TTL
        {
            return val;
        }
    }
    let val = probe_rootless_podman().await;
    if let Ok(mut cache) = probe_cache().lock() {
        cache.last_podman = Some((Instant::now(), val));
    }
    val
}

/// Cached nix-system probe (TTL); on miss runs timeout-bounded subprocess.
async fn detect_nix_system_cached() -> Option<String> {
    {
        let cache = probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, ref val)) = cache.last_nix
            && at.elapsed() < PROBE_CACHE_TTL
        {
            return val.clone();
        }
    }
    let val = detect_nix_system().await;
    if let Ok(mut cache) = probe_cache().lock() {
        cache.last_nix = Some((Instant::now(), val.clone()));
    }
    val
}

/// True when `podman info` JSON reports rootless mode.
///
/// Hard-capped at [`PODMAN_PROBE_TIMEOUT`]; errors/timeouts → `false`.
/// Child is `kill_on_drop` so a hung `podman` does not leak forever after timeout.
async fn probe_rootless_podman() -> bool {
    let child = Command::new("podman")
        .args(["info", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(child) = child else {
        return false;
    };
    let output = match tokio::time::timeout(PODMAN_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) | Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };
    // podman info: host.security.rootless == true, or rootlessNetworkCmd present.
    if let Some(b) = v
        .pointer("/host/security/rootless")
        .and_then(|x| x.as_bool())
    {
        return b;
    }
    // Older shapes: top-level "rootless"
    v.get("rootless").and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Best-effort Nix system triple via `nix eval --impure --raw --expr builtins.currentSystem`.
///
/// Hard-capped at [`NIX_PROBE_TIMEOUT`]; errors/timeouts → `None`.
async fn detect_nix_system() -> Option<String> {
    let child = Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = match tokio::time::timeout(NIX_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) | Err(_) => return None,
    };
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Parse `RUSSEL_NODE_LABELS=key=val,key2=val2` into a map (invalid pairs skipped).
pub fn parse_node_labels(raw: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(raw) = raw else {
        return map;
    };
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() {
            continue;
        }
        map.insert(k.to_string(), v.to_string());
    }
    map
}

/// Data root from env or default.
pub fn data_root_from_env() -> PathBuf {
    std::env::var("RUSSEL_DATA_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_ROOT))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn meminfo_parser_reads_total_and_available() {
        let dir = tempfile_dir();
        let path = dir.join("meminfo");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    8192000 kB\n"
        )
        .unwrap();
        let (total, avail) = read_meminfo_mb_from(&path);
        assert_eq!(total, 16000);
        assert_eq!(avail, 8000);
    }

    #[test]
    fn count_services_skips_reserved_and_metadata_only_not_running() {
        let dir = tempfile_dir();
        // Deployed metadata without live signals → not running.
        std::fs::create_dir_all(dir.join("api")).unwrap();
        std::fs::write(dir.join("api/metadata.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("traefik")).unwrap();
        std::fs::create_dir_all(dir.join("orphan")).unwrap(); // no metadata
        std::fs::create_dir_all(dir.join("old.bak")).unwrap();
        assert_eq!(count_running_services(&dir), 0);
    }

    #[test]
    fn count_services_microvm_live_pid() {
        let dir = tempfile_dir();
        let pid = std::process::id();
        std::fs::create_dir_all(dir.join("vm-live")).unwrap();
        std::fs::write(
            dir.join("vm-live/metadata.json"),
            format!(r#"{{"runtime":"microvm","vm_pid":{pid}}}"#),
        )
        .unwrap();
        assert_eq!(count_running_services(&dir), 1);
    }

    #[test]
    fn count_services_microvm_dead_pid_not_counted() {
        let dir = tempfile_dir();
        // Extremely unlikely to be a live PID on a Linux host.
        std::fs::create_dir_all(dir.join("vm-dead")).unwrap();
        std::fs::write(
            dir.join("vm-dead/metadata.json"),
            r#"{"runtime":"microvm","vm_pid":2147483646}"#,
        )
        .unwrap();
        assert_eq!(count_running_services(&dir), 0);
    }

    #[test]
    fn count_services_container_with_id_counted() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join("ctr")).unwrap();
        std::fs::write(
            dir.join("ctr/metadata.json"),
            r#"{"runtime":"container","container_id":"abc123"}"#,
        )
        .unwrap();
        // Empty container_id does not count.
        std::fs::create_dir_all(dir.join("ctr-empty")).unwrap();
        std::fs::write(
            dir.join("ctr-empty/metadata.json"),
            r#"{"runtime":"container","container_id":""}"#,
        )
        .unwrap();
        assert_eq!(count_running_services(&dir), 1);
    }

    #[test]
    fn service_value_looks_running_pure() {
        let live = std::process::id();
        assert!(!service_value_looks_running(&serde_json::json!({})));
        assert!(!service_value_looks_running(&serde_json::json!({
            "runtime": "microvm"
        })));
        assert!(service_value_looks_running(&serde_json::json!({
            "runtime": "microvm",
            "vm_pid": live
        })));
        assert!(!service_value_looks_running(&serde_json::json!({
            "runtime": "microvm",
            "vm_pid": 2147483646u32
        })));
        assert!(service_value_looks_running(&serde_json::json!({
            "runtime": "container",
            "container_id": "x"
        })));
        assert!(!service_value_looks_running(&serde_json::json!({
            "runtime": "container"
        })));
        // Legacy without runtime: live pid counts.
        assert!(service_value_looks_running(&serde_json::json!({
            "vm_pid": live
        })));
    }

    #[test]
    fn parse_labels() {
        let m = parse_node_labels(Some("zone=a, role = worker ,bad,=x,ok="));
        assert_eq!(m.get("zone"), Some(&"a".to_string()));
        assert_eq!(m.get("role"), Some(&"worker".to_string()));
        assert_eq!(m.get("ok"), Some(&"".to_string()));
        assert!(!m.contains_key("bad"));
    }

    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "russel-agent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
