use super::agent::{
    AGENT_BUSYBOX_APPLETS, AGENT_INIT_SCRIPT, AGENT_INITRAMFS_BASENAME, pack_cpio_blocking,
};
use super::process::{cloud_hypervisor_stop_pattern, escape_pkill_literal, stop_tap_identity};
use super::runner::{MicrovmRunner, select_kernel_version};
use crate::network::{PortAllocator, lookup_subnet, release_subnet, subnet_for};
use std::path::PathBuf;

// ── Stop process-selection fallback (metadata / registry / service-path) ────

#[test]
fn stop_tap_prefers_metadata_over_registry() {
    crate::network::test_with_empty_registry(|| {
        let svc = "stop-meta-svc";
        // Registry holds a different TAP than metadata would report.
        let leased = subnet_for(svc).unwrap();
        let meta_tap = "rsl-deadbeef";
        assert_ne!(leased.tap_id, meta_tap);

        let chosen = stop_tap_identity(svc, Some(meta_tap));
        assert_eq!(chosen.as_deref(), Some(meta_tap));

        let pattern = cloud_hypervisor_stop_pattern(svc, chosen.as_deref());
        assert!(
            pattern.contains(&format!("tap={meta_tap}")),
            "pattern must pin metadata TAP: {pattern}"
        );
        assert!(
            !pattern.contains(&leased.tap_id),
            "must not fall through to registry TAP when metadata is present"
        );
        release_subnet(svc);
    });
}

#[test]
fn stop_tap_uses_registry_when_metadata_absent() {
    crate::network::test_with_empty_registry(|| {
        let svc = "stop-reg-svc";
        let leased = subnet_for(svc).unwrap();

        assert!(stop_tap_identity(svc, None).as_deref() == Some(leased.tap_id.as_str()));
        assert!(stop_tap_identity(svc, Some("")).as_deref() == Some(leased.tap_id.as_str()));

        let pattern = cloud_hypervisor_stop_pattern(svc, Some(&leased.tap_id));
        assert!(pattern.contains(&format!("tap={}", leased.tap_id)));
        assert!(
            !pattern.contains(&format!("russel/{svc}/")),
            "TAP path should not use service-path marker"
        );
        release_subnet(svc);
    });
}

#[test]
fn stop_pattern_falls_back_to_service_path_without_identity() {
    crate::network::test_with_empty_registry(|| {
        let svc = "stop-path-svc";
        assert!(lookup_subnet(svc).is_none());
        assert!(stop_tap_identity(svc, None).is_none());

        let pattern = cloud_hypervisor_stop_pattern(svc, None);
        assert!(
            pattern.contains(&format!("russel/{svc}/")),
            "expected service-path marker: {pattern}"
        );
        assert!(
            !pattern.contains("tap="),
            "must not invent a preferred TAP identity: {pattern}"
        );
    });
}

#[test]
fn cloud_hypervisor_stop_pattern_escapes_service_id_metacharacters() {
    // Even if an invalid ID reaches the helper, metacharacters must not
    // broaden the pkill match to other services.
    let evil = "svc.a*b|c";
    let escaped = escape_pkill_literal(evil);
    assert_eq!(escaped, r"svc\.a\*b\|c");

    let pattern = cloud_hypervisor_stop_pattern(evil, None);
    assert!(
        pattern.contains(r"russel/svc\.a\*b\|c/"),
        "expected escaped service path in pattern: {pattern}"
    );
    assert!(
        !pattern.contains("russel/svc.a*b|c/"),
        "raw metacharacters must not appear unescaped: {pattern}"
    );

    // TAP branch also escapes the TAP token.
    let tap_pattern = cloud_hypervisor_stop_pattern("ok-svc", Some("rsl-ab.cd"));
    assert!(
        tap_pattern.contains(r"tap=rsl-ab\.cd"),
        "expected escaped TAP in pattern: {tap_pattern}"
    );
}

#[test]
fn stop_pattern_does_not_select_other_service_identity() {
    crate::network::test_with_empty_registry(|| {
        let svc_a = "stop-iso-a";
        let svc_b = "stop-iso-b";
        let lease_b = subnet_for(svc_b).unwrap();

        // Service A has no metadata or lease — path fallback only.
        assert!(stop_tap_identity(svc_a, None).is_none());
        let pattern_a = cloud_hypervisor_stop_pattern(svc_a, None);

        // Must not match B's TAP or B's service path.
        assert!(!pattern_a.contains(&lease_b.tap_id));
        assert!(!pattern_a.contains(&format!("russel/{svc_b}/")));
        assert!(pattern_a.contains(&format!("russel/{svc_a}/")));

        // Service B with its own TAP must not match A's path either.
        let pattern_b = cloud_hypervisor_stop_pattern(svc_b, Some(&lease_b.tap_id));
        assert!(pattern_b.contains(&format!("tap={}", lease_b.tap_id)));
        assert!(!pattern_b.contains(&format!("russel/{svc_a}/")));
        assert!(!pattern_b.contains(&format!("russel/{svc_b}/")));

        release_subnet(svc_a);
        release_subnet(svc_b);
    });
}

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

// ── network_alloc_for_service ownership contract ────────────────────────────

#[test]
fn network_alloc_none_without_metadata_or_lease() {
    crate::network::test_with_empty_registry(|| {
        let id = "net-alloc-none-svc";
        assert!(
            crate::network::lookup_subnet(id).is_none(),
            "precondition: no lease"
        );
        // No on-disk metadata under /var/lib/russel in unit tests.
        assert!(
            super::process::network_alloc_for_service(id).is_none(),
            "must not invent preferred_subnet when unowned"
        );
    });
}

#[test]
fn network_alloc_uses_owned_registry_lease() {
    crate::network::test_with_empty_registry(|| {
        let id = "net-alloc-lease-svc";
        let alloc = crate::network::subnet_for(id).unwrap();
        let got = super::process::network_alloc_for_service(id)
            .expect("owned registry lease must resolve");
        assert_eq!(got.tap_id, alloc.tap_id);
        assert_eq!(got.host_ip, alloc.host_ip);
        assert_eq!(got.vm_ip, alloc.vm_ip);
        crate::network::release_subnet(id);
        assert!(
            super::process::network_alloc_for_service(id).is_none(),
            "after release, identity must clear"
        );
    });
}

#[test]
fn network_alloc_ignores_preferred_key_owned_by_other_service() {
    crate::network::test_with_empty_registry(|| {
        let victim = "net-alloc-victim-svc";
        let other = "net-alloc-other-svc";
        let preferred = crate::network::preferred_subnet(victim);
        let key = crate::network::network_key_from_host_ip(&preferred.host_ip)
            .expect("preferred host_ip must parse");
        // Other service owns victim's preferred TAP/IP identity.
        crate::network::claim_subnet_key(other, key).unwrap();
        assert!(
            super::process::network_alloc_for_service(victim).is_none(),
            "must not target another service's TAP via preferred_subnet"
        );
        // Owner still resolves via their lease.
        let owner = super::process::network_alloc_for_service(other).expect("owner lease");
        assert_eq!(owner.tap_id, preferred.tap_id);
        crate::network::release_subnet(other);
    });
}

/// Destroy with inventory but no live TAP/VM must free port + subnet.
///
/// Models partial destroy (missing TAP device / stop no-op). Control-plane
/// inventory ownership ends with destroy even when runtime cleanup is a no-op
/// or only partially successful — leases must not be retained for retry.
/// Covers the contract that TAP teardown issues must not permanently hold
/// inventory (release is unconditional after stop/TAP attempts), including
/// dropping any net-03 `TcpListener` hold so the OS port is bindable again.
#[tokio::test]
async fn destroy_releases_port_and_subnet_with_partial_state() {
    let svc = "destroy-partial-lease-svc";
    // Unique free port under locks (not across .await — clippy await_holding_lock).
    // Avoids racing a fixed port with parallel tests after destroy.
    let port = {
        let _subnet = crate::network::subnet_test_lock();
        let _port = crate::network::port_test_lock();
        crate::network::test_clear_subnet_registry();
        PortAllocator::release(svc);
        let _ = subnet_for(svc).unwrap();
        let port = PortAllocator.next(svc).expect("allocate free port");
        assert!(lookup_subnet(svc).is_some(), "precondition: subnet leased");
        assert_eq!(PortAllocator::allocated_port(svc), Some(port));
        assert!(
            PortAllocator::has_hold(svc),
            "precondition: next must open a hold listener"
        );
        port
    };

    // No metadata under /var/lib/russel — stop is a no-op; TAP teardown targets
    // the registry lease and treats a missing device as success (or records a
    // teardown error). Either way, inventory must be free afterward.
    let result = MicrovmRunner::new().destroy(svc).await;
    // Prefer Ok when only inventory existed; tolerate partial-failure Err so
    // the assertion below still checks the inventory contract.
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            msg.contains("partial") || msg.contains("tap teardown") || msg.contains("stop"),
            "unexpected destroy error: {msg}"
        );
    }

    {
        let _subnet = crate::network::subnet_test_lock();
        let _port = crate::network::port_test_lock();
        assert!(
            lookup_subnet(svc).is_none(),
            "subnet must be released after destroy (partial runtime state)"
        );
        assert!(
            PortAllocator::allocated_port(svc).is_none(),
            "port allocation must be cleared after destroy"
        );
        assert!(
            !PortAllocator::has_hold(svc),
            "port hold TcpListener must be dropped after destroy so the OS port is free"
        );
        // Re-claim proves registry + OS bind are free for a later deploy.
        PortAllocator::reserve(svc, port)
            .expect("port must be free after destroy so a later deploy can claim it");
        PortAllocator::release(svc);
        release_subnet(svc);
        crate::network::test_clear_subnet_registry();
    }
}
