//! On-disk service metadata (`/var/lib/russel/<id>/metadata.json`).
//!
//! ## Schema (`schema_version` = 1)
//!
//! Shared fields: `schema_version`, `service_id`, `node_id`, `runtime`
//! (`microvm`|`container`), `host_port`, `guest_port`, `store_path`, `mem_mb`,
//! `deployed_at` (RFC3339), optional `bin_name`.
//!
//! `node_id` is the host that wrote the record (`RUSSEL_NODE_ID`, else hostname,
//! else `local`). Pre-#212 metadata may omit it; readers must tolerate missing.
//!
//! **microVM** also writes: `vm_ip`, `host_ip` (TAP host side), `kernel_path`,
//! optional `vm_pid` / `socat_pid` / `initramfs` / `app_path`, and
//! `virtiofsd_pids` (JSON array of u32). Older files may still have singular
//! `virtiofsd_pid`; readers that care about process cleanup should accept both
//! until all hosts have redeployed. Deploy/rollback must keep writers and
//! destroy/stop readers on the same shape — do not mix a new writer with an
//! old destroy path that only understands `virtiofsd_pid`.
//!
//! **container** also writes: `container_id`, `container_name`, `rootfs_path`,
//! optional `podman_args`.

use std::path::{Path, PathBuf};

use russel_core::config::RuntimeKind;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Process-local counter for unique temp file names, avoiding collisions
/// between concurrent atomic writes and stale temp files from a crashed writer.
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const SCHEMA_VERSION: u32 = 1;

/// Env override for stable node identity (horizontal scaling Phase 0 / #212).
pub const NODE_ID_ENV: &str = "RUSSEL_NODE_ID";

/// On-disk path for a service's metadata.json.
pub fn metadata_path(service_id: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/russel/{service_id}/metadata.json"))
}

/// Resolve the node id written into service `metadata.json`.
///
/// Order: `RUSSEL_NODE_ID` (trimmed, non-empty) → hostname → `"local"`.
/// Single-node installs need no config; multi-node sets a stable id per host.
pub fn resolve_node_id() -> String {
    resolve_node_id_with(
        std::env::var(NODE_ID_ENV).ok().as_deref(),
        hostname_for_node_id,
    )
}

/// Resolve node id with injectable env override and host fallback.
///
/// Order: non-empty trimmed `env_override` → `host_fallback()` → `"local"`.
/// Production uses [`resolve_node_id`]; tests pass fixed values so they never
/// mutate process-global `RUSSEL_NODE_ID`.
pub fn resolve_node_id_with(
    env_override: Option<&str>,
    host_fallback: impl FnOnce() -> Option<String>,
) -> String {
    if let Some(v) = env_override {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    host_fallback().unwrap_or_else(|| "local".to_string())
}

fn hostname_for_node_id() -> Option<String> {
    #[cfg(unix)]
    {
        // HOST_NAME_MAX is typically 64; 256 is a safe portable buffer.
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid writable region of length `buf.len()`.
        // gethostname writes a NUL-terminated name when it succeeds.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = std::str::from_utf8(&buf[..len]).ok()?.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    #[cfg(not(unix))]
    {
        None
    }
}

/// Directories under `/var/lib/russel` (and peers) that are **not** user services.
///
/// Used by list/reconcile/cleanup discovery so internal layout never appears as
/// deployable services (e.g. dashboard `GET /vms`).
///
/// - `*.bak` — dual-live / destroy backups
/// - `traefik` — ingress dynamic config root
/// - `secrets` — host secrets store
/// - `_pool` — warm-pool snapshot state
pub fn is_reserved_service_dir(name: &str) -> bool {
    name.ends_with(".bak") || name == "traefik" || name == "secrets" || name == "_pool"
}

/// Fields commonly loaded from on-disk metadata for API rehydration.
#[derive(Debug, Clone, Default)]
pub struct LoadedMetadata {
    pub runtime: Option<RuntimeKind>,
    pub host_port: Option<u16>,
    pub guest_port: Option<u16>,
    pub container_id: Option<String>,
    /// microVM TAP guest address (absent for container-only metadata).
    pub vm_ip: Option<String>,
}

/// Full on-disk record for a service, used by startup reconcile to rehydrate
/// observed state without Child process handles.
#[derive(Debug, Clone, Default)]
pub struct ServiceDiskRecord {
    pub service_id: Option<String>,
    pub runtime: Option<RuntimeKind>,
    pub host_port: Option<u16>,
    pub guest_port: Option<u16>,
    pub container_id: Option<String>,
    /// Podman name: canonical `russel-{id}` or gen-scoped `russel-{id}_g{hex}`.
    pub container_name: Option<String>,
    pub vm_pid: Option<u32>,
    pub socat_pid: Option<u32>,
    pub virtiofsd_pids: Vec<u32>,
}

/// Parse `runtime` from on-disk metadata JSON.
///
/// Returns the parsed runtime if the `runtime` key is present and valid.
/// Returns `None` when the `runtime` key is absent from valid JSON
/// (callers should warn about legacy metadata missing the runtime field).
/// Returns `None` only for unreadable or invalid JSON.
pub fn prior_runtime_from_metadata(content: &str) -> Option<RuntimeKind> {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return None,
    };
    value
        .get("runtime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
}

/// Read prior runtime from disk, returning None when no metadata exists
/// or the file is corrupt/unreadable.
pub fn prior_runtime_from_disk(service_id: &str) -> Option<RuntimeKind> {
    let path = metadata_path(service_id);
    let content = std::fs::read_to_string(&path).ok()?;
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
        host_port: value
            .get("host_port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok()),
        guest_port: value
            .get("guest_port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok()),
        container_id: value
            .get("container_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        vm_ip: value
            .get("vm_ip")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Load full service metadata from disk for reconcile.
/// Parses defensively: missing fields are left as None / empty.
#[allow(dead_code)] // public API — callers outside this crate may use it
pub fn load_service_disk_record(service_id: &str) -> Option<ServiceDiskRecord> {
    load_service_disk_record_from(&metadata_path(service_id))
}

/// Load service metadata from an arbitrary path (for tests / custom base dirs).
pub fn load_service_disk_record_from(path: &Path) -> Option<ServiceDiskRecord> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;

    let virtiofsd_pids = value
        .get("virtiofsd_pids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_else(|| {
            // Legacy: singular virtiofsd_pid
            value
                .get("virtiofsd_pid")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .map(|n| vec![n])
                .unwrap_or_default()
        });

    Some(ServiceDiskRecord {
        service_id: value
            .get("service_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        runtime: value
            .get("runtime")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok()),
        host_port: value
            .get("host_port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok()),
        guest_port: value
            .get("guest_port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok()),
        container_id: value
            .get("container_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        container_name: value
            .get("container_name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        vm_pid: value
            .get("vm_pid")
            .and_then(|v| v.as_u64())
            .and_then(|p| u32::try_from(p).ok()),
        socat_pid: value
            .get("socat_pid")
            .and_then(|v| v.as_u64())
            .and_then(|p| u32::try_from(p).ok()),
        virtiofsd_pids,
    })
}

/// Atomic write of the control plane catalog JSON to `/var/lib/russel/ctrl-catalog.json`.
pub fn write_ctrl_catalog(catalog: &serde_json::Value) -> anyhow::Result<()> {
    write_ctrl_catalog_to(&PathBuf::from("/var/lib/russel/ctrl-catalog.json"), catalog)
}

/// Atomic write of the control plane catalog JSON to an arbitrary path.
pub fn write_ctrl_catalog_to(path: &Path, catalog: &serde_json::Value) -> anyhow::Result<()> {
    let content = serde_json::to_string_pretty(catalog)
        .map_err(|e| anyhow::anyhow!("failed to serialize catalog: {}", e))?;
    atomic_write(path, content.as_bytes())
}

/// Sibling temp path for `path`, in the same directory so the final rename
/// stays atomic on the same filesystem.
///
/// Unlike `Path::with_extension("json.tmp")`, this appends `.tmp.<nonce>` to
/// the **full** file name, so non-`.json`-named destinations keep their name
/// and two sibling paths that differ only by extension cannot collide.
fn temp_sibling_path(path: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64
        ^ TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(".{file_name}.tmp.{nonce:x}"))
}

/// Atomically write `content` to `path` (temp file + rename), modeled on the
/// `secrets::secure_write` semantics:
///
/// - temp file in the same directory (`create_new`) so the rename is atomic,
/// - `O_NOFOLLOW` + `O_CLOEXEC` + mode `0600` applied at `open(2)` time,
/// - `write` + `sync_all` before `fs::rename` over the destination,
/// - best-effort `sync_all` of the parent directory so the rename is durable.
///
/// A failed write is cleaned up (temp file removed) and never clobbers the
/// destination.
fn atomic_write(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent).map_err(|e| {
        anyhow::anyhow!(
            "failed to create metadata parent {}: {}",
            parent.display(),
            e
        )
    })?;

    let tmp = temp_sibling_path(path);
    let write_result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            // Apply at open(2) time — not after write — so there is no umask
            // window (0o600), no fd inheritance to children (O_CLOEXEC), and
            // no symlink-replace race on the tmp path (O_NOFOLLOW).
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("failed to open tmp {}: {}", tmp.display(), e))?;
        file.write_all(content)
            .map_err(|e| anyhow::anyhow!("failed to write tmp {}: {}", tmp.display(), e))?;
        file.sync_all()
            .map_err(|e| anyhow::anyhow!("failed to fsync tmp {}: {}", tmp.display(), e))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("failed to rename tmp into place {}: {}", path.display(), e)
    })?;

    // fsync parent directory so the rename is durable.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// Resolve lifecycle runtime: prefer in-memory state, else on-disk metadata.
/// Defaults to `Microvm` only when no metadata file exists at all (first deploy).
/// Warns when valid metadata JSON is missing the `runtime` key (legacy).
pub fn resolve_lifecycle_runtime(
    state_runtime: Option<RuntimeKind>,
    service_id: &str,
) -> RuntimeKind {
    if let Some(rt) = state_runtime {
        return rt;
    }
    // Check if metadata file exists at all.
    let path = metadata_path(service_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return RuntimeKind::Microvm, // no metadata → first deploy
    };
    match prior_runtime_from_metadata(&content) {
        Some(rt) => rt,
        None => {
            tracing::warn!(
                service_id,
                "valid metadata.json missing 'runtime' key — defaulting to microvm"
            );
            RuntimeKind::Microvm
        }
    }
}

/// Build versioned metadata JSON for a microVM deployment.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // unit-tested; deploy path still builds metadata inline
pub fn build_microvm_metadata(
    service_id: &str,
    host_port: u16,
    guest_port: u16,
    vm_ip: &str,
    host_ip: &str,
    vm_pid: Option<u32>,
    virtiofsd_pids: &[u32],
    socat_pid: Option<u32>,
    kernel_path: &str,
    store_path: &str,
    mem_mb: u16,
    cpus: u8,
    app_path: Option<&str>,
    bin_name: Option<&str>,
    initramfs_path: Option<&str>,
) -> serde_json::Value {
    build_microvm_metadata_with_gen(
        service_id,
        host_port,
        guest_port,
        vm_ip,
        host_ip,
        vm_pid,
        virtiofsd_pids,
        socat_pid,
        kernel_path,
        store_path,
        mem_mb,
        cpus,
        app_path,
        bin_name,
        initramfs_path,
        None,
        None,
    )
}

/// Like [`build_microvm_metadata`] but records generation + TAP identity for
/// zero-downtime cutover (candidate may keep a gen-scoped TAP after promote).
#[allow(clippy::too_many_arguments)]
pub fn build_microvm_metadata_with_gen(
    service_id: &str,
    host_port: u16,
    guest_port: u16,
    vm_ip: &str,
    host_ip: &str,
    vm_pid: Option<u32>,
    virtiofsd_pids: &[u32],
    socat_pid: Option<u32>,
    kernel_path: &str,
    store_path: &str,
    mem_mb: u16,
    cpus: u8,
    app_path: Option<&str>,
    bin_name: Option<&str>,
    initramfs_path: Option<&str>,
    generation_id: Option<&str>,
    tap_id: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "service_id": service_id,
        "node_id": resolve_node_id(),
        "runtime": "microvm",
        "host_port": host_port,
        "guest_port": guest_port,
        "vm_ip": vm_ip,
        "host_ip": host_ip,
        "kernel_path": kernel_path,
        "store_path": store_path,
        "mem_mb": mem_mb,
        "cpus": cpus,
        "deployed_at": deployed_at_now(),
    });
    if let Some(pid) = vm_pid {
        meta["vm_pid"] = serde_json::json!(pid);
    }
    if !virtiofsd_pids.is_empty() {
        meta["virtiofsd_pids"] = serde_json::json!(virtiofsd_pids);
    }
    if let Some(pid) = socat_pid {
        meta["socat_pid"] = serde_json::json!(pid);
    }
    if let Some(path) = app_path {
        meta["app_path"] = serde_json::json!(path);
    }
    if let Some(name) = bin_name {
        meta["bin_name"] = serde_json::json!(name);
    }
    if let Some(path) = initramfs_path {
        meta["initramfs"] = serde_json::json!(path);
    }
    if let Some(gid) = generation_id {
        meta["generation_id"] = serde_json::json!(gid);
    }
    if let Some(tap) = tap_id {
        meta["tap_id"] = serde_json::json!(tap);
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
    podman_args: &[String],
) -> serde_json::Value {
    build_container_metadata_with_gen(
        service_id,
        host_port,
        guest_port,
        store_path,
        container_id,
        container_name,
        rootfs_path,
        mem_mb,
        bin_name,
        podman_args,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_container_metadata_with_gen(
    service_id: &str,
    host_port: u16,
    guest_port: u16,
    store_path: &str,
    container_id: &str,
    container_name: &str,
    rootfs_path: &str,
    mem_mb: u16,
    bin_name: Option<&str>,
    podman_args: &[String],
    generation_id: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "service_id": service_id,
        "node_id": resolve_node_id(),
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
    if !podman_args.is_empty() {
        meta["podman_args"] = serde_json::json!(podman_args);
    }
    if let Some(gid) = generation_id {
        meta["generation_id"] = serde_json::json!(gid);
    }
    meta
}

/// Rewrite `service_id` in an existing metadata file after generation promote.
pub fn rewrite_metadata_service_id(path: impl AsRef<Path>, service_id: &str) -> anyhow::Result<()> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read metadata {}: {e}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("parse metadata {}: {e}", path.display()))?;
    value["service_id"] = serde_json::json!(service_id);
    write_metadata(path, &value)
}

pub fn write_metadata(path: impl AsRef<Path>, metadata: &serde_json::Value) -> anyhow::Result<()> {
    let content = serde_json::to_string_pretty(metadata)
        .map_err(|e| anyhow::anyhow!("failed to serialize metadata: {}", e))?;
    // Atomic temp-file + rename write; mode 0600 is applied at open(2) time so
    // non-root (e.g. podman user) cannot read/rewrite control plane metadata
    // even if they can traverse the service dir (Issue #193).
    atomic_write(path.as_ref(), content.as_bytes())
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_node_id_prefers_env() {
        assert_eq!(
            resolve_node_id_with(Some("worker-a"), || Some("host".into())),
            "worker-a"
        );
        assert_eq!(
            resolve_node_id_with(Some("  worker-b  "), || Some("host".into())),
            "worker-b"
        );
    }

    #[test]
    fn resolve_node_id_ignores_blank_env() {
        // Blank/empty env falls through to host fallback — never a blank string.
        assert_eq!(
            resolve_node_id_with(Some("   "), || Some("from-host".into())),
            "from-host"
        );
        assert_eq!(
            resolve_node_id_with(Some(""), || Some("from-host".into())),
            "from-host"
        );
        assert_eq!(resolve_node_id_with(Some("   "), || None), "local");
        assert_eq!(resolve_node_id_with(Some(""), || None), "local");
    }

    #[test]
    fn resolve_node_id_without_env_is_nonempty() {
        assert_eq!(
            resolve_node_id_with(None, || Some("myhost".into())),
            "myhost"
        );
        assert_eq!(resolve_node_id_with(None, || None), "local");
        // Production path (real env + hostname) must also be non-empty.
        assert!(!resolve_node_id().is_empty());
    }

    #[test]
    fn metadata_includes_schema_version() {
        let expected_node = resolve_node_id();
        let meta = build_microvm_metadata(
            "api",
            3100,
            3000,
            "10.0.1.2",
            "10.0.1.1",
            Some(42),
            &[43u32],
            Some(44),
            "/nix/store/kernel",
            "/nix/store/app",
            512,
            1,
            Some("/nix/store/app/bin/myapp"),
            Some("myapp"),
            Some("/var/lib/russel/api/initramfs.cpio"),
        );
        assert_eq!(meta["schema_version"], SCHEMA_VERSION);
        assert_eq!(meta["node_id"], expected_node);
        assert_eq!(meta["runtime"], "microvm");
        assert_eq!(meta["host_ip"], "10.0.1.1");
        assert_eq!(meta["app_path"], "/nix/store/app/bin/myapp");
        assert_eq!(meta["virtiofsd_pids"], serde_json::json!([43]));
        assert_eq!(meta["bin_name"], "myapp");
        assert_eq!(meta["host_ip"], "10.0.1.1");
        assert_eq!(meta["virtiofsd_pids"], serde_json::json!([43]));
        assert_eq!(meta["app_path"], "/nix/store/app/bin/myapp");
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
            &[],
        );
        assert_eq!(container["schema_version"], SCHEMA_VERSION);
        assert_eq!(container["node_id"], expected_node);
        assert_eq!(container["runtime"], "container");
    }

    #[test]
    fn metadata_builders_emit_node_id_from_env() {
        // Builders call resolve_node_id(); env→value mapping is covered by
        // resolve_node_id_with unit tests above (no process env mutation here).
        let expected_node = resolve_node_id();
        let micro = build_microvm_metadata_with_gen(
            "api",
            3100,
            3000,
            "10.0.1.2",
            "10.0.1.1",
            Some(1),
            &[],
            None,
            "/nix/store/kernel",
            "/nix/store/app",
            512,
            1,
            None,
            None,
            None,
            Some("gen1"),
            Some("rsl-abc"),
        );
        assert_eq!(micro["node_id"], expected_node);
        assert_eq!(micro["generation_id"], "gen1");

        let container = build_container_metadata_with_gen(
            "api",
            3100,
            3000,
            "/nix/store/app",
            "cid",
            "russel-api",
            "/var/lib/russel/api/rootfs",
            512,
            None,
            &[],
            Some("gen2"),
        );
        assert_eq!(container["node_id"], expected_node);
        assert_eq!(container["generation_id"], "gen2");
    }

    #[test]
    fn prior_runtime_returns_none_when_missing() {
        // F-17: when `runtime` key is absent from valid JSON, return None
        // (callers should warn and default via resolve_lifecycle_runtime).
        let json = r#"{"service_id":"api","host_port":3100}"#;
        assert_eq!(prior_runtime_from_metadata(json), None);
    }

    #[test]
    fn prior_runtime_reads_container_field() {
        let json = r#"{"runtime":"container","service_id":"api"}"#;
        assert_eq!(
            prior_runtime_from_metadata(json),
            Some(RuntimeKind::Container)
        );
    }

    #[test]
    fn prior_runtime_reads_microvm_field() {
        let json = r#"{"runtime":"microvm","service_id":"api"}"#;
        assert_eq!(
            prior_runtime_from_metadata(json),
            Some(RuntimeKind::Microvm)
        );
    }

    #[test]
    fn prior_runtime_invalid_json_returns_none() {
        assert_eq!(prior_runtime_from_metadata("not json"), None);
    }

    #[test]
    fn resolve_lifecycle_runtime_prefers_state() {
        assert_eq!(
            resolve_lifecycle_runtime(Some(RuntimeKind::Container), "api"),
            RuntimeKind::Container
        );
    }

    #[test]
    fn container_metadata_includes_podman_args_when_non_empty() {
        let meta = build_container_metadata(
            "api",
            3100,
            3000,
            "/nix/store/app",
            "abc",
            "russel-api",
            "/var/lib/russel/api/rootfs",
            512,
            None,
            &["-v".into(), "/data:/data:ro".into()],
        );
        assert_eq!(
            meta["podman_args"],
            serde_json::json!(["-v", "/data:/data:ro"])
        );
    }

    #[test]
    fn container_metadata_omits_podman_args_when_empty() {
        let meta = build_container_metadata(
            "api",
            3100,
            3000,
            "/nix/store/app",
            "abc",
            "russel-api",
            "/var/lib/russel/api/rootfs",
            512,
            None,
            &[],
        );
        assert!(meta.get("podman_args").is_none());
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
            &[],
        ))
        .unwrap();
        assert_eq!(
            prior_runtime_from_metadata(&json),
            Some(RuntimeKind::Container)
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["host_port"], 3100);
        assert_eq!(value["guest_port"], 3000);
        assert_eq!(value["container_id"], "abc");
    }

    // ── ServiceDiskRecord tests ──────────────────────────────────────────────

    #[test]
    fn service_disk_record_parses_microvm_fields() {
        let meta = build_microvm_metadata(
            "api",
            3100,
            3000,
            "10.0.1.2",
            "10.0.1.1",
            Some(42),
            &[43, 44],
            Some(45),
            "/nix/store/kernel",
            "/nix/store/app",
            512,
            1,
            Some("/nix/store/app/bin/myapp"),
            Some("myapp"),
            None,
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.json");
        std::fs::write(&path, serde_json::to_string(&meta).unwrap()).unwrap();

        let rec = load_service_disk_record_from(&path).unwrap();
        assert_eq!(rec.service_id.as_deref(), Some("api"));
        assert_eq!(rec.host_port, Some(3100));
        assert_eq!(rec.guest_port, Some(3000));
        assert_eq!(rec.vm_pid, Some(42));
        assert_eq!(rec.socat_pid, Some(45));
        assert_eq!(rec.virtiofsd_pids, vec![43, 44]);
        assert_eq!(rec.runtime, Some(RuntimeKind::Microvm));
    }

    #[test]
    fn service_disk_record_legacy_singular_virtiofsd_pid() {
        let json = serde_json::json!({
            "schema_version": 1,
            "service_id": "api",
            "runtime": "microvm",
            "host_port": 3100,
            "guest_port": 3000,
            "vm_pid": 42,
            "socat_pid": 45,
            "virtiofsd_pid": 99
        });
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.json");
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let rec = load_service_disk_record_from(&path).unwrap();
        // Legacy singular field is expanded into virtiofsd_pids.
        assert_eq!(rec.virtiofsd_pids, vec![99]);
        assert_eq!(rec.vm_pid, Some(42));
        assert_eq!(rec.host_port, Some(3100));
    }

    #[test]
    fn service_disk_record_defensive_parse_missing_fields() {
        let json = serde_json::json!({
            "schema_version": 1,
            "service_id": "minimal"
        });
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata.json");
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let rec = load_service_disk_record_from(&path).unwrap();
        assert_eq!(rec.service_id.as_deref(), Some("minimal"));
        assert!(rec.vm_pid.is_none());
        assert!(rec.host_port.is_none());
        assert!(rec.virtiofsd_pids.is_empty());
        assert!(rec.runtime.is_none());
    }

    // ── Catalog write / read tests ───────────────────────────────────────────

    #[test]
    fn reserved_service_dirs_are_recognized() {
        assert!(is_reserved_service_dir("traefik"));
        assert!(is_reserved_service_dir("secrets"));
        assert!(is_reserved_service_dir("_pool"));
        assert!(is_reserved_service_dir("api.bak"));
        assert!(is_reserved_service_dir("svc.bak"));
        assert!(!is_reserved_service_dir("basic-http-tester-another"));
        assert!(!is_reserved_service_dir("api"));
        assert!(!is_reserved_service_dir("pooltpl"));
    }

    #[test]
    fn catalog_write_and_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_path = tmp.path().join("ctrl-catalog.json");
        let catalog = serde_json::json!({
            "schema_version": 1,
            "updated_at": deployed_at_now(),
            "services": {
                "api": {
                    "status": "deployed",
                    "runtime": "microvm",
                    "host_port": 3100
                }
            }
        });
        let content = serde_json::to_string_pretty(&catalog).unwrap();
        std::fs::write(&catalog_path, &content).unwrap();

        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        assert_eq!(read["schema_version"], 1);
        assert_eq!(read["services"]["api"]["status"], "deployed");
        assert_eq!(read["services"]["api"]["host_port"], 3100);
    }

    // ── Atomic write tests (C-3) ────────────────────────────────────────────

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn atomic_write_produces_correct_final_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("metadata.json");
        let meta = serde_json::json!({"service_id": "api", "host_port": 3100});
        write_metadata(&dest, &meta).unwrap();

        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(read, meta);
        // The temp file is renamed into place — no `.tmp` siblings remain.
        assert_eq!(dir_entries(tmp.path()), vec!["metadata.json".to_string()]);
    }

    #[test]
    fn atomic_write_keeps_non_json_filename() {
        // Regression for `with_extension("json.tmp")`: a non-`.json` name must
        // keep its full filename (and must not lose its real extension).
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("catalog.data");
        let catalog = serde_json::json!({"schema_version": 1});
        write_ctrl_catalog_to(&dest, &catalog).unwrap();

        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(read, catalog);
        assert_eq!(dir_entries(tmp.path()), vec!["catalog.data".to_string()]);
    }

    #[test]
    fn atomic_write_failure_does_not_clobber_destination() {
        // Simulate failure: an ancestor is a regular file, so `create_dir_all`
        // fails before any write — the destination must remain untouched.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "i am a file").unwrap();
        let dest = blocker.join("sub").join("metadata.json");

        let res = write_metadata(&dest, &serde_json::json!({"x": 1}));
        assert!(res.is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn atomic_write_overwrites_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("metadata.json");
        write_metadata(&dest, &serde_json::json!({"n": 1})).unwrap();
        write_metadata(&dest, &serde_json::json!({"n": 2})).unwrap();

        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(read, serde_json::json!({"n": 2}));
        // Only the destination remains — no stale temp files.
        assert_eq!(dir_entries(tmp.path()), vec!["metadata.json".to_string()]);
    }
}
