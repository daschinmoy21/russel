use super::agent::{
    AGENT_BUSYBOX_APPLETS, AGENT_INIT_SCRIPT, AGENT_INITRAMFS_BASENAME, pack_cpio_blocking,
};
use super::runner::{MicrovmRunner, select_kernel_version};
use std::path::PathBuf;

#[test]
fn validate_service_id_accepts_normal_ids() {
    MicrovmRunner::validate_service_id("api").unwrap();
    MicrovmRunner::validate_service_id("basic-http-tester").unwrap();
    MicrovmRunner::validate_service_id("svc_01").unwrap();
    MicrovmRunner::validate_service_id("pooltpl").unwrap();
}

#[test]
fn validate_service_id_rejects_reserved_ids() {
    for id in ["secrets", "traefik", "_pool"] {
        let err = MicrovmRunner::validate_service_id(id).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("reserved"),
            "expected reserved error for {id}, got: {msg}"
        );
    }
    // .bak ids fail charset (dot) and/or reserved check — must not pass.
    let err = MicrovmRunner::validate_service_id("foo.bak").unwrap_err();
    assert!(
        err.to_string().contains("reserved")
            || err.to_string().contains("alphanumeric")
            || err.to_string().contains("path"),
        "expected rejection for foo.bak, got: {err}"
    );
}

#[test]
fn validate_service_id_rejects_empty_and_path_chars() {
    assert!(MicrovmRunner::validate_service_id("").is_err());
    assert!(MicrovmRunner::validate_service_id("../etc").is_err());
    assert!(MicrovmRunner::validate_service_id("a/b").is_err());
    assert!(MicrovmRunner::validate_service_id("a\\b").is_err());
}

#[test]
fn select_kernel_version_single() {
    let mut versions = vec!["6.1.0".to_string()];
    let result = select_kernel_version(&mut versions).unwrap();
    assert_eq!(result, "6.1.0");
}

#[test]
fn select_kernel_version_lexicographic_selects_greatest() {
    // Lexicographic sort: "6.1.0" < "6.10.0" < "6.2.0"
    // (NOT numeric: 6.10.0 would be between 6.1.0 and 6.2.0 numerically,
    //  but lexicographically "6.10.0" < "6.2.0" because '1' < '2')
    let mut versions = vec![
        "6.1.0".to_string(),
        "6.10.0".to_string(),
        "6.2.0".to_string(),
    ];
    let result = select_kernel_version(&mut versions).unwrap();
    // Lexicographic sort: "6.1.0", "6.10.0", "6.2.0" → last is "6.2.0"
    assert_eq!(result, "6.2.0");
}

#[test]
fn select_kernel_version_with_dash_suffixes() {
    // Versions like "6.6.60-rt" vs "6.6.60".
    // Shorter string sorts first: "6.6.60" < "6.6.60-rt".
    let mut versions = vec!["6.6.60-rt".to_string(), "6.6.60".to_string()];
    let result = select_kernel_version(&mut versions).unwrap();
    // After sort: "6.6.60", "6.6.60-rt" → last is "6.6.60-rt"
    assert_eq!(result, "6.6.60-rt");
}

#[test]
fn select_kernel_version_empty_is_error() {
    let mut versions: Vec<String> = vec![];
    let result = select_kernel_version(&mut versions);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no kernel versions")
    );
}

#[test]
fn agent_init_script_uses_short_usleep_retries() {
    // Mount retries use 5ms.
    assert!(AGENT_INIT_SCRIPT.contains("/bin/usleep 5000"));
    // Deploy.env wait uses 10ms.
    assert!(AGENT_INIT_SCRIPT.contains("/bin/usleep 10000"));

    // Old 100ms sleep must not be present.
    assert!(!AGENT_INIT_SCRIPT.contains("usleep 100000"));
    // Old sleep 0.01 must not be present.
    assert!(!AGENT_INIT_SCRIPT.contains("sleep 0.01"));

    // Old fallback pattern must not be present.
    assert!(!AGENT_INIT_SCRIPT.contains("/bin/sleep 0.01 2>/dev/null || /bin/sleep 1"));
    assert!(!AGENT_INIT_SCRIPT.contains("/bin/usleep 100000 2>/dev/null || /bin/sleep 1"));
}

#[test]
fn agent_initramfs_cache_version_is_v3() {
    assert_eq!(AGENT_INITRAMFS_BASENAME, "agent-initramfs-v3.cpio");
}

#[test]
fn busybox_agent_symlinks_include_usleep() {
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"usleep"));
    // Spot-check a few other expected applets.
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"sh"));
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"mount"));
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"ip"));
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"sleep"));
}

/// Regression: F-27 pack_cpio must set cwd on cpio and must not use
/// Command::output() (which would discard the archive file and SIGPIPE find).
#[test]
fn pack_cpio_blocking_writes_nonempty_archive() {
    let bb = std::env::var_os("RUSSEL_TEST_BUSYBOX")
        .map(PathBuf::from)
        .or_else(|| {
            // Prefer a nix-built busybox if present on PATH as multi-call.
            which_busybox()
        });
    let Some(bb) = bb else {
        eprintln!("skip pack_cpio_blocking test: no busybox (set RUSSEL_TEST_BUSYBOX)");
        return;
    };

    let work = tempfile::tempdir().unwrap();
    let bin = work.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("hello"), b"hi").unwrap();
    std::fs::write(work.path().join("init"), b"#!/bin/sh\n").unwrap();

    let out = work.path().join("out.cpio");
    pack_cpio_blocking(work.path(), &out, bb.to_str().unwrap()).unwrap();
    let size = std::fs::metadata(&out).unwrap().len();
    assert!(size > 64, "expected non-trivial cpio, got {size} bytes");
}

fn which_busybox() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("busybox");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Known store path from recent deploys (optional local convenience).
    let store =
        PathBuf::from("/nix/store/4s514kmhnmncvcsvjh3d17y7y0psbyc1-busybox-1.37.0/bin/busybox");
    store.is_file().then_some(store)
}
