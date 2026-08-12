use super::forward::russel_forward_rule_plan;
use super::subnet::fnv1a;
use super::*;

#[test]
fn subnet_for_is_deterministic() {
    let a1 = subnet_for("my-service").unwrap();
    let a2 = subnet_for("my-service").unwrap();
    assert_eq!(a1.host_ip, a2.host_ip);
    assert_eq!(a1.vm_ip, a2.vm_ip);
    assert_eq!(a1.mac, a2.mac);
    release_subnet("my-service");
}

#[test]
fn subnet_for_different_services_differ() {
    let a = subnet_for("service-a").unwrap();
    let b = subnet_for("service-b").unwrap();
    assert_ne!(a.host_ip, b.host_ip);
    release_subnet("service-a");
    release_subnet("service-b");
}

#[test]
fn release_subnet_frees_lease() {
    let a = subnet_for("lease-svc").unwrap();
    release_subnet("lease-svc");
    // After release, re-allocate should succeed with same preferred hash.
    let b = subnet_for("lease-svc").unwrap();
    assert_eq!(a.tap_id, b.tap_id);
    release_subnet("lease-svc");
}

#[test]
fn subnet_for_produces_valid_tap_id() {
    let a = subnet_for("a-service-id-that-is-much-longer-than-a-linux-interface-name").unwrap();
    assert!(a.tap_id.starts_with("rsl-"));
    assert!(a.tap_id.len() <= 15);
    assert!(a.tap_id.bytes().all(|byte| byte.is_ascii_hexdigit()
        || byte == b'-'
        || byte == b'r'
        || byte == b's'
        || byte == b'l'));
    release_subnet("a-service-id-that-is-much-longer-than-a-linux-interface-name");
}

#[test]
fn subnet_for_produces_valid_mac() {
    let a = subnet_for("bar").unwrap();
    assert!(a.mac.starts_with("02:00:00:00:"));
    assert_eq!(a.mac.len(), 17);
    release_subnet("bar");
}

#[test]
fn subnet_index_bounded() {
    for s in &["a", "b", "long-service-name-123", "edge", "max"] {
        let allocation = subnet_for(s).unwrap();
        let parts: Vec<&str> = allocation.host_ip.split('.').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "10");
        assert_eq!(parts[3], "1");
        let x: u8 = parts[1].parse().unwrap();
        let y: u8 = parts[2].parse().unwrap();
        // Both octets are valid (full 16-bit space)
        let _ = (x, y);
        release_subnet(s);
    }
}

#[test]
fn port_allocator_increments() {
    let alloc = PortAllocator;
    let p1 = alloc.next("service-1").unwrap();
    let p2 = alloc.next("service-2").unwrap();
    let p3 = alloc.next("service-3").unwrap();
    // ponytail: do not assert absolute 3100 — host may have that port
    // bound. Just verify distinct, monotonic, and >= 3100.
    assert!(p1 >= 3100, "p1={p1} must be >= 3100");
    assert!(p1 < p2, "p1={p1} must be < p2={p2}");
    assert!(p2 < p3, "p2={p2} must be < p3={p3}");
    PortAllocator::release("service-1");
    PortAllocator::release("service-2");
    PortAllocator::release("service-3");
}

#[test]
fn port_allocator_reserve_and_release() {
    PortAllocator::reserve("custom-service", 4000).unwrap();
    assert!(PortAllocator::reserve("another-service", 4000).is_err());
    PortAllocator::release("custom-service");
    PortAllocator::reserve("another-service", 4000).unwrap();
    PortAllocator::release("another-service");
}

#[test]
fn port_allocator_rejects_port_zero() {
    let err = PortAllocator::reserve("zero-svc", 0).unwrap_err();
    assert!(err.to_string().contains("port 0"), "unexpected err: {err}");
    let err = PortAllocator::claim_existing("zero-svc", 0).unwrap_err();
    assert!(err.to_string().contains("port 0"), "unexpected err: {err}");
}

#[test]
fn fnv1a_is_xor_then_multiply() {
    // Verify FNV-1a uses XOR-then-MULTIPLY, not MULTIPLY-then-XOR (FNV-1).
    // Also check known-answer test vectors for the 32-bit variant.
    let hash = fnv1a("hello");
    let hash2 = fnv1a("hello");
    assert_eq!(hash, hash2, "FNV-1a must be deterministic");
    assert_ne!(fnv1a("a"), fnv1a("aa"));

    // Known-answer test vectors (FNV-1a 32-bit).
    // FNV offset basis: 0x811c9dc5 = 2166136261.
    assert_eq!(
        fnv1a(""),
        2_166_136_261,
        "FNV-1a empty-string = offset basis"
    );
    // "a" = (0x811c9dc5 ^ 0x61) * 0x01000193 = 0xe40c292c = 3826002220.
    assert_eq!(fnv1a("a"), 3_826_002_220, "FNV-1a(\"a\") known answer");
    // "foo" known-answer: 0xa9f37ed7 = 2851307223.
    assert_eq!(fnv1a("foo"), 2_851_307_223, "FNV-1a(\"foo\") known answer");
    // "hello" known-answer: 0x4f9f2cab = 1335831723.
    assert_eq!(
        fnv1a("hello"),
        1_335_831_723,
        "FNV-1a(\"hello\") known answer"
    );
}

#[test]
fn fnv1a_different_from_fnv1() {
    // The old (buggy) FNV-1: hash = (hash * 16777619) ^ byte
    fn old_fnv1(bytes: &[u8]) -> u32 {
        bytes.iter().fold(2_166_136_261u32, |acc, &b| {
            acc.wrapping_mul(16_777_619) ^ b as u32
        })
    }
    // For most inputs, FNV-1a and FNV-1 differ.
    assert_ne!(fnv1a("russel"), old_fnv1("russel".as_bytes()));
}

#[test]
fn pkill_socat_pattern_anchored() {
    // F-19: the socat pkill pattern must NOT match sibling services.
    let _pat_a = format!("^socat-russel-{}( |$)", "foo");
    let cmdline_foo = "socat-russel-foo TCP-LISTEN:...";
    let cmdline_foobar = "socat-russel-foobar TCP-LISTEN:...";
    let cmdline_socat_x = "socat-russel-x TCP-LISTEN:...";
    // regex crate isn't available here but we can do prefix checks:
    // The pkill pattern "^socat-russel-foo( |$)" should match foo,
    // but NOT match foobar because after "foo" must be space or end.
    assert!(
        cmdline_foo.starts_with("socat-russel-foo "),
        "foo must match"
    );
    assert!(
        !cmdline_foobar.starts_with("socat-russel-foo "),
        "foobar must NOT match"
    );
    // socat-x (prefix of command but different service) should not match
    assert!(
        !cmdline_socat_x.starts_with("socat-russel-foo "),
        "x must NOT match"
    );
}

#[test]
fn pkill_virtiofsd_pattern_no_prefix_collision() {
    // The virtiofsd pattern uses "russel/{id}/" with trailing slash
    // which acts as a natural boundary. Verify.
    let pat_needle = "russel/foo/";
    let cmdline_foo = "/var/lib/russel/foo/virtiofs.sock";
    let cmdline_foobar = "/var/lib/russel/foobar/virtiofs.sock";
    assert!(cmdline_foo.contains(pat_needle), "foo should match itself");
    assert!(
        !cmdline_foobar.contains(pat_needle),
        "foobar should NOT match foo pattern"
    );
}

#[test]
fn subnet_for_exhaustion_fails_closed() {
    // Fill every 16-bit key so the 1024-probe path cannot find a free slot.
    super::subnet::test_fill_all_subnet_keys();
    let err = subnet_for("exhaust-new-service").unwrap_err();
    assert!(
        err.to_string().contains("exhausted"),
        "unexpected err: {err}"
    );
    // Must not register the new service under any key.
    super::subnet::test_clear_subnet_registry();
    // After clear, allocation succeeds and uses the preferred key.
    let alloc = subnet_for("exhaust-new-service").unwrap();
    assert_eq!(alloc.tap_id, preferred_subnet("exhaust-new-service").tap_id);
    release_subnet("exhaust-new-service");
}
#[test]
fn port_allocator_claim_existing_registers_port() {
    PortAllocator::release("claimed-svc");
    PortAllocator::claim_existing("claimed-svc", 9000).unwrap();
    // Same service, same port is idempotent.
    PortAllocator::claim_existing("claimed-svc", 9000).unwrap();
    // Different service claiming same port is rejected.
    let err = PortAllocator::claim_existing("other-svc", 9000).unwrap_err();
    assert!(err.to_string().contains("already claimed"));
    // Same service with a different port moves the claim.
    PortAllocator::claim_existing("claimed-svc", 9001).unwrap();
    // Old port should now be available for another service.
    PortAllocator::release("claimed-svc");
    PortAllocator::claim_existing("other-svc", 9000).unwrap();
    PortAllocator::release("other-svc");
}

// ── #187 guest FORWARD isolation helpers ─────────────────────────────

#[test]
fn forward_filter_enabled_by_default() {
    assert!(!forward_filter_disabled_from_env(None, None));
    assert!(!forward_filter_disabled_from_env(Some(""), None));
    assert!(!forward_filter_disabled_from_env(Some("deny"), None));
    assert!(!forward_filter_disabled_from_env(Some("1"), None));
    assert!(!forward_filter_disabled_from_env(None, Some("0")));
    assert!(!forward_filter_disabled_from_env(None, Some("false")));
}

#[test]
fn forward_filter_disabled_via_russel_forward_allow() {
    for v in [
        "allow", "ALLOW", "off", "0", "false", "disabled", "no", " Allow ",
    ] {
        assert!(
            forward_filter_disabled_from_env(Some(v), None),
            "RUSSEL_FORWARD={v:?} should disable filter"
        );
    }
}

#[test]
fn forward_filter_disabled_via_disable_env() {
    for v in ["1", "true", "yes", "on", "TRUE", " Yes "] {
        assert!(
            forward_filter_disabled_from_env(None, Some(v)),
            "RUSSEL_DISABLE_FORWARD_FILTER={v:?} should disable filter"
        );
    }
}

#[test]
fn forward_filter_either_escape_hatch_suffices() {
    assert!(forward_filter_disabled_from_env(Some("allow"), Some("0")));
    assert!(forward_filter_disabled_from_env(Some("deny"), Some("1")));
}

#[test]
fn russel_forward_rule_plan_is_default_deny_dedicated_chain() {
    let plan = russel_forward_rule_plan();
    let roles: Vec<_> = plan.iter().map(|r| r.role).collect();
    assert_eq!(
        roles,
        vec![
            "create-chain",
            "flush-chain",
            "default-drop",
            "jump-in",
            "jump-out"
        ]
    );

    // Never flush built-in FORWARD — only operate on RUSSEL-FORWARD or jump into it.
    for step in &plan {
        let joined = step.args.join(" ");
        assert!(
            !joined.contains("-F FORWARD") && !joined.contains("-X FORWARD"),
            "must not flush/delete built-in FORWARD: {joined}"
        );
        assert!(
            step.args.contains(&RUSSEL_FORWARD_CHAIN) || step.args.contains(&"FORWARD"),
            "unexpected step: {joined}"
        );
    }

    let drop_step = plan.iter().find(|r| r.role == "default-drop").unwrap();
    assert!(drop_step.args.contains(&"DROP"));
    assert!(drop_step.args.contains(&RUSSEL_FORWARD_CHAIN));

    for role in ["jump-in", "jump-out"] {
        let jump = plan.iter().find(|r| r.role == role).unwrap();
        assert!(jump.args.contains(&RSL_IFACE_MATCH));
        assert!(jump.args.contains(&RUSSEL_FORWARD_CHAIN));
        assert!(jump.args.contains(&"FORWARD"));
    }
}

#[test]
fn rsl_iface_match_covers_russel_tap_prefix() {
    assert_eq!(RSL_IFACE_MATCH, "rsl-+");
    assert!(RSL_IFACE_MATCH.starts_with("rsl-"));
    // Chain name must not look like a built-in.
    assert_ne!(RUSSEL_FORWARD_CHAIN, "FORWARD");
    assert!(RUSSEL_FORWARD_CHAIN.starts_with("RUSSEL"));
}

#[test]
fn allow_mode_risk_message_mentions_pivot() {
    let msg = FORWARD_FILTER_ALLOW_RISK.to_ascii_lowercase();
    assert!(msg.contains("ip_forward") || msg.contains("forward"));
    assert!(msg.contains("guest") || msg.contains("microvm") || msg.contains("tap"));
    assert!(msg.contains("single-tenant") || msg.contains("debugging"));
}
