//! Host capacity probes for agent heartbeats.
//!
//! Reads Linux `/proc` and filesystem checks only — no heavy sysinfo crate.
//! Each probe is best-effort so a missing `/proc` (tests, weird hosts) still
//! yields a usable zeroed snapshot.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use russel_core::api::NodeCapacity;
use tokio::process::Command;

/// Default Russel data root (service dirs live under here).
pub const DEFAULT_DATA_ROOT: &str = "/var/lib/russel";

/// Directories that are not user services (keep in sync with ctrl metadata).
fn is_reserved_service_dir(name: &str) -> bool {
    name.ends_with(".bak") || name == "traefik" || name == "secrets" || name == "_pool"
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
        probe_rootless_podman().await
    } else {
        false
    };
    let nix_system = detect_nix_system().await;

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

/// Count non-reserved immediate children of `data_root` that look like services.
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
        if ft.is_dir() {
            // Prefer dirs that have metadata.json (deployed service).
            let meta = ent.path().join("metadata.json");
            if meta.is_file() {
                n = n.saturating_add(1);
            }
        }
    }
    n
}

/// True when `podman info` JSON reports rootless mode.
async fn probe_rootless_podman() -> bool {
    let output = Command::new("podman")
        .args(["info", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(out) = output else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
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
async fn detect_nix_system() -> Option<String> {
    let output = Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
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
    fn count_services_skips_reserved_and_requires_metadata() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.join("api")).unwrap();
        std::fs::write(dir.join("api/metadata.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("traefik")).unwrap();
        std::fs::create_dir_all(dir.join("orphan")).unwrap(); // no metadata
        std::fs::create_dir_all(dir.join("old.bak")).unwrap();
        assert_eq!(count_running_services(&dir), 1);
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
