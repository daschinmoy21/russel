use super::agent::{
    AGENT_BUSYBOX_APPLETS, AGENT_INIT_SCRIPT, AGENT_INITRAMFS_BASENAME, pack_cpio_blocking,
};
use super::process::{
    cloud_hypervisor_stop_pattern, escape_pkill_literal, stop_tap_identity, virtiofsd_stop_pattern,
};
use super::runner::{MicrovmRunner, remove_service_dir_keep_volumes};
use super::spec::service_fs_mounts;
use crate::network::{PortAllocator, lookup_subnet, release_subnet, subnet_for};
use std::path::{Path, PathBuf};

fn service_dir_pkill_needle(service_id: &str) -> String {
    escape_pkill_literal(&format!(
        "{}/",
        crate::paths::service_dir(service_id).display()
    ))
}

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
            !pattern.contains(&service_dir_pkill_needle(svc)),
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
            pattern.contains(&service_dir_pkill_needle(svc)),
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
    let escaped_path = service_dir_pkill_needle(evil);
    assert!(
        pattern.contains(&escaped_path),
        "expected escaped service path in pattern: {pattern}"
    );
    let raw_path = format!("{}/", crate::paths::service_dir(evil).display());
    assert!(
        !pattern.contains(&raw_path),
        "raw metacharacters must not appear unescaped: {pattern}"
    );

    // TAP branch also escapes the TAP token.
    let tap_pattern = cloud_hypervisor_stop_pattern("ok-svc", Some("rsl-ab.cd"));
    assert!(
        tap_pattern.contains(r"tap=rsl-ab\.cd"),
        "expected escaped TAP in pattern: {tap_pattern}"
    );
}

/// `pkill -f` uses POSIX ERE, same as `grep -E`.
fn ere_matches(pattern: &str, line: &str) -> bool {
    use std::io::Write;
    let mut child = std::process::Command::new("grep")
        .args(["-Eq", "--", pattern])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn grep");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}

#[test]
fn virtiofsd_stop_pattern_spares_candidate_volume_share() {
    let svc = "vfs-stop-svc";
    let stable = crate::paths::service_dir(svc);
    let candidate = crate::paths::service_dir(&format!("{svc}_g1234abcd"));
    let pattern = virtiofsd_stop_pattern(svc);

    let own = format!(
        "virtiofsd --socket-path={}/virtiofs-vol0.sock --shared-dir={}/volumes/data",
        stable.display(),
        stable.display()
    );
    assert!(ere_matches(&pattern, &own), "{pattern} vs {own}");

    // During a dual-live cutover the candidate shares the stable volume dir.
    let candidate_vol = format!(
        "virtiofsd --socket-path={}/virtiofs-vol0.sock --shared-dir={}/volumes/data",
        candidate.display(),
        stable.display()
    );
    assert!(
        !ere_matches(&pattern, &candidate_vol),
        "stopping the old generation must not kill the candidate's volume: {pattern}"
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
        assert!(!pattern_a.contains(&service_dir_pkill_needle(svc_b)));
        assert!(pattern_a.contains(&service_dir_pkill_needle(svc_a)));

        // Service B with its own TAP must not match A's path either.
        let pattern_b = cloud_hypervisor_stop_pattern(svc_b, Some(&lease_b.tap_id));
        assert!(pattern_b.contains(&format!("tap={}", lease_b.tap_id)));
        assert!(!pattern_b.contains(&service_dir_pkill_needle(svc_a)));
        assert!(!pattern_b.contains(&service_dir_pkill_needle(svc_b)));

        release_subnet(svc_a);
        release_subnet(svc_b);
    });
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
fn agent_initramfs_cache_version_is_v10() {
    assert_eq!(AGENT_INITRAMFS_BASENAME, "agent-initramfs-v10.cpio");
}

#[test]
fn agent_matches_the_container_filesystem_contract() {
    let at = |needle: &str| {
        AGENT_INIT_SCRIPT
            .find(needle)
            .unwrap_or_else(|| panic!("agent init lacks {needle:?}"))
    };
    // The /run tmpfs must not hide the scratch share mounted under it.
    assert!(at("tmpfs /run") < at("russelscratch /run/russel"));
    // Volumes need their mount points created while / is still writable.
    let read_only = at("remount,ro /");
    assert!(at("done < /config/mounts") < read_only);
    assert!(at("tmpfs /tmp") < read_only);
    assert!(at("tmpfs /dev/shm") < read_only);
    assert!(read_only < at("\n$RUN_AS \"$APP\" \"$@\""));
}

#[test]
fn agent_drops_to_the_app_user_before_starting_it() {
    let at = |needle: &str| {
        AGENT_INIT_SCRIPT
            .find(needle)
            .unwrap_or_else(|| panic!("agent init lacks {needle:?}"))
    };
    // /etc/passwd and the sysctl need / and /proc writable.
    let read_only = at("remount,ro /");
    assert!(at("> /etc/passwd") < read_only);
    assert!(at("ip_unprivileged_port_start") < read_only);
    assert!(at("RUN_AS=\"/bin/chpst -u app:app\"") < at("\n$RUN_AS \"$APP\" \"$@\""));
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"chpst"));
}

#[test]
fn agent_logs_app_exit_and_powers_off() {
    // pid 1 exiting panics and reboots the guest, which truncates console.log.
    assert!(!AGENT_INIT_SCRIPT.contains("exec \"$APP\""));
    let app = AGENT_INIT_SCRIPT.find("\n$RUN_AS \"$APP\" \"$@\"").unwrap();
    let logged = AGENT_INIT_SCRIPT.find("app exited with status $?").unwrap();
    let off = AGENT_INIT_SCRIPT.find("/bin/poweroff -f").unwrap();
    assert!(app < logged && logged < off);
    assert!(AGENT_BUSYBOX_APPLETS.contains(&"poweroff"));
}

#[test]
fn agent_init_script_marks_network_ready_before_app() {
    let net = AGENT_INIT_SCRIPT
        .find(".net_ready")
        .expect("net_ready marker");
    let route = AGENT_INIT_SCRIPT.find("ip route add default").unwrap();
    let exec = AGENT_INIT_SCRIPT.find("\n$RUN_AS \"$APP\" \"$@\"").unwrap();
    assert!(route < net && net < exec);
}

#[test]
fn agent_init_script_writes_ready_to_scratch_not_cfg() {
    assert!(AGENT_INIT_SCRIPT.contains("mount -t virtiofs -o ro russelcfg /config"));
    assert!(AGENT_INIT_SCRIPT.contains("mount -t virtiofs russelscratch /run/russel"));
    assert!(AGENT_INIT_SCRIPT.contains("/run/russel/.agent_ready"));
    assert!(!AGENT_INIT_SCRIPT.contains("/config/.agent_ready"));
    assert!(AGENT_INIT_SCRIPT.contains(". /config/deploy.env"));
}

#[test]
fn service_fs_mounts_cfg_readonly_scratch_separate() {
    let sock = Path::new("/var/lib/russel/svc-1");
    let cfg = Path::new("/var/lib/russel/svc-1/cfg");
    let scratch = Path::new("/var/lib/russel/svc-1/scratch");
    let mounts = service_fs_mounts(sock, cfg, scratch);

    assert_eq!(mounts.len(), 3);
    assert_eq!(mounts[0].tag, "nixstore");
    assert!(mounts[0].readonly);
    assert_eq!(mounts[0].shared_dir, PathBuf::from("/nix/store"));

    assert_eq!(mounts[1].tag, "russelcfg");
    assert!(
        mounts[1].readonly,
        "cfg share must be read-only so the guest cannot write deploy.env or fill the host cfg dir"
    );
    assert_eq!(mounts[1].shared_dir, cfg);
    assert_eq!(
        mounts[1].socket,
        PathBuf::from("/var/lib/russel/svc-1/virtiofs-cfg.sock")
    );

    assert_eq!(mounts[2].tag, "russelscratch");
    assert!(
        !mounts[2].readonly,
        "scratch is the only guest-writable host share"
    );
    assert_eq!(mounts[2].shared_dir, scratch);
    assert_ne!(
        mounts[2].shared_dir, mounts[1].shared_dir,
        "scratch must not be the cfg directory"
    );
    assert_ne!(
        mounts[2].shared_dir.as_path(),
        sock,
        "scratch must not be the service root (metadata.json lives there)"
    );
    assert_eq!(
        mounts[2].socket,
        PathBuf::from("/var/lib/russel/svc-1/virtiofs-scratch.sock")
    );
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
#[test]
fn destroy_releases_port_and_subnet_with_partial_state() {
    let svc = "destroy-partial-lease-svc";
    // Hold both locks through destroy and cleanup so setup, release, and the
    // final registry/port assertions form one isolated transaction.
    let _port = crate::network::port_test_lock();
    let _subnet = crate::network::subnet_test_lock();
    crate::network::test_clear_subnet_registry();
    PortAllocator::release(svc);
    let _ = subnet_for(svc).unwrap();
    // Not `next`: its 3100+ range is shared with a dev ctrl, rootless Podman,
    // and parallel deploy tests, any of which can take the port after destroy
    // drops the hold and before the final re-reserve below.
    let port = crate::network::reserve_test_port(svc);
    assert!(lookup_subnet(svc).is_some(), "precondition: subnet leased");
    assert_eq!(PortAllocator::allocated_port(svc), Some(port));
    assert!(
        PortAllocator::has_hold(svc),
        "precondition: reserve must open a hold listener"
    );
    assert!(
        self_holds_listener(port),
        "precondition: this process must hold a listening socket on {port}"
    );

    // Use a local runtime so the test locks can stay held without an `.await`.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // No metadata under /var/lib/russel — stop is a no-op; TAP teardown targets
    // the registry lease and treats a missing device as success (or records a
    // teardown error). Either way, inventory must be free afterward.
    let result = runtime.block_on(MicrovmRunner::new().destroy(svc));
    // Prefer Ok when only inventory existed; tolerate partial-failure Err so
    // the assertion below still checks the inventory contract.
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            msg.contains("partial") || msg.contains("tap teardown") || msg.contains("stop"),
            "unexpected destroy error: {msg}"
        );
    }

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
    // The hold must be closed by the time destroy returns, not later.
    assert!(
        !self_holds_listener(port),
        "destroy must close this process's listening socket on {port} synchronously"
    );
    // Re-claim proves registry + OS bind are free for a later deploy. A child
    // that another test forked while the hold was open has its own copy of the
    // socket until it execs (CLOEXEC), so the bind can briefly see EADDRINUSE.
    // The check above already proved our own fd is gone, so only those
    // transient copies are left to wait out.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Err(e) = PortAllocator::reserve(svc, port) {
        assert!(
            std::time::Instant::now() < deadline,
            "port must be free after destroy so a later deploy can claim it: {e}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    PortAllocator::release(svc);
    release_subnet(svc);
    crate::network::test_clear_subnet_registry();
}

/// Whether one of this process's own fds is a listening TCP socket on `port`.
///
/// Reads the fd table, not the port: a socket shared with a forked child
/// still shows in `/proc/net/tcp*`, but only our fds count here.
fn self_holds_listener(port: u16) -> bool {
    const TCP_LISTEN: &str = "0A";
    let local_port = format!(":{port:04X}");
    let mut inodes = std::collections::HashSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 9 && cols[1].ends_with(&local_port) && cols[3] == TCP_LISTEN {
                inodes.insert(format!("socket:[{}]", cols[9]));
            }
        }
    }
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .any(|target| inodes.contains(target.to_string_lossy().as_ref()))
}

/// Restore `RUSSEL_KERNEL_POOL` / `RUSSEL_KERNEL_PATH` on every exit path.
/// No other test in this binary reads or writes these two variables, so the
/// process-global mutation stays isolated from the parallel test harness.
struct KernelEnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for KernelEnvRestore {
    fn drop(&mut self) {
        // SAFETY: only this test mutates these variables; exclusive until drop.
        for (name, value) in self.0.drain(..) {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// A kernel placed in the pool (RUSSEL_KERNEL_POOL) resolves before any
/// flake/nix build is attempted.
#[test]
fn ensure_kernel_resolves_kernel_pool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let kernel = dir.path().join("bzImage");
    std::fs::write(&kernel, b"fake bzImage").expect("write pool kernel");

    let _restore = KernelEnvRestore(vec![
        ("RUSSEL_KERNEL_POOL", std::env::var_os("RUSSEL_KERNEL_POOL")),
        ("RUSSEL_KERNEL_PATH", std::env::var_os("RUSSEL_KERNEL_PATH")),
    ]);
    // SAFETY: only this test mutates these variables; values restored on drop.
    unsafe {
        std::env::remove_var("RUSSEL_KERNEL_PATH");
        std::env::set_var("RUSSEL_KERNEL_POOL", &kernel);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let info = runtime
        .block_on(MicrovmRunner::new().ensure_kernel())
        .expect("pool kernel must resolve");
    assert_eq!(info.path, kernel);
}

#[test]
fn cloud_hypervisor_identity_requires_ch_and_service_dir() {
    let api_sock = format!(
        "{}/cloud-hypervisor.sock",
        crate::paths::service_dir("api").display()
    );
    let other_sock = format!(
        "{}/cloud-hypervisor.sock",
        crate::paths::service_dir("other").display()
    );
    assert!(super::cloud_hypervisor_cmdline_matches(
        &format!("cloud-hypervisor --api-socket {api_sock}"),
        "api"
    ));
    // Unrelated process with reused PID shape.
    assert!(!super::cloud_hypervisor_cmdline_matches(
        "/usr/bin/sleep 999",
        "api"
    ));
    // CH for a different service — must not match "api" via --api-socket.
    assert!(!super::cloud_hypervisor_cmdline_matches(
        &format!("cloud-hypervisor --api-socket {other_sock}"),
        "api"
    ));
    assert!(!super::cloud_hypervisor_cmdline_matches(
        "cloud-hypervisor --api-socket /tmp/x.sock",
        "api"
    ));
    assert!(!super::cloud_hypervisor_cmdline_matches(
        "cloud-hypervisor",
        ""
    ));
}

#[tokio::test]
async fn destroy_dir_removal_keeps_container_volumes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("svc");
    std::fs::create_dir_all(dir.join("volumes/data")).unwrap();
    std::fs::write(dir.join("volumes/data/keep.txt"), b"kept").unwrap();
    std::fs::create_dir_all(dir.join("cfg")).unwrap();
    std::fs::write(dir.join("metadata.json"), b"{}").unwrap();

    remove_service_dir_keep_volumes(&dir).await.unwrap();

    assert!(dir.join("volumes/data/keep.txt").is_file());
    assert!(!dir.join("cfg").exists());
    assert!(!dir.join("metadata.json").exists());

    let plain = tmp.path().join("vm");
    std::fs::create_dir_all(plain.join("cfg")).unwrap();
    remove_service_dir_keep_volumes(&plain).await.unwrap();
    assert!(!plain.exists());

    remove_service_dir_keep_volumes(&tmp.path().join("missing"))
        .await
        .unwrap();
}

/// Run the agent's argv block (from `set --` to the app exec) in a real
/// shell, with the app replaced by a printer, and return what it received.
fn run_agent_argv_block(argv_file: &Path) -> Vec<String> {
    let start = AGENT_INIT_SCRIPT.find("\nset --\n").expect("argv block");
    let end = AGENT_INIT_SCRIPT
        .find("\n$RUN_AS \"$APP\" \"$@\"")
        .expect("app exec");
    let block = AGENT_INIT_SCRIPT[start..end]
        .replace("/config/argv", argv_file.to_str().unwrap())
        .replace("echo \"starting $APP ($# args)\"", "");
    let script = format!("{block}\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done\n");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", "/should-not-expand")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .split_last()
        .map(|(_, rest)| rest.to_vec())
        .unwrap_or_default()
}

#[test]
fn agent_passes_service_args_verbatim() {
    let args: Vec<String> = [
        "file-server",
        "--listen",
        ":8080",
        "two words",
        "it's",
        "\"quoted\"",
        "$HOME",
        "`id`",
        "$(id)",
        "a;b",
        "  lead",
        "trail  ",
        "back\\slash",
        "",
        "*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("argv");
    std::fs::write(&file, crate::deploy::render_argv(&args).unwrap()).unwrap();
    assert_eq!(run_agent_argv_block(&file), args);
}

#[test]
fn agent_runs_app_without_args_when_argv_missing_or_empty() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(run_agent_argv_block(&tmp.path().join("missing")).is_empty());
    let empty = tmp.path().join("argv");
    std::fs::write(&empty, "").unwrap();
    assert!(run_agent_argv_block(&empty).is_empty());
}

fn resolved(guest: &str, host: &str, rw: bool) -> russel_core::volumes::ResolvedVolume {
    russel_core::volumes::ResolvedVolume {
        guest: guest.into(),
        host_path: PathBuf::from(host),
        rw,
        keep: false,
        managed: true,
        name: None,
    }
}

#[test]
fn virtiofs_cache_policy_per_share() {
    use super::spec::{FsCache, volume_fs_mounts};
    let dir = Path::new("/srv/svc");
    let base = service_fs_mounts(dir, &dir.join("cfg"), &dir.join("scratch"));
    let cache = |tag: &str| base.iter().find(|m| m.tag == tag).unwrap().cache;
    assert_eq!(cache("nixstore"), FsCache::Always);
    assert_eq!(cache("russelcfg"), FsCache::Always);
    // The host polls scratch for .agent_ready, so the guest must not cache it.
    assert_eq!(cache("russelscratch"), FsCache::Never);

    let vols = volume_fs_mounts(
        dir,
        &[
            resolved("/data", "/v/data", true),
            resolved("/conf", "/v/conf", false),
        ],
    );
    // `never` is FUSE direct I/O: no shared writable mmap, so SQLite (WAL) and
    // LMDB fail with EIO. vaultwarden, navidrome and meilisearch hit this.
    assert_eq!(vols[0].cache, FsCache::Auto);
    assert_eq!(vols[1].cache, FsCache::Always);
}

#[test]
fn volume_guest_paths_cannot_shadow_agent_mounts() {
    use super::spec::check_volume_guest_paths;
    for bad in [
        "/nix",
        "/nix/store",
        "/nix/store/x",
        "/config",
        "/run",
        "/run/russel/x",
        "/proc",
        "/dev/",
        "/nix/./store",
        "//nix//store",
        "/config/./x",
    ] {
        assert!(
            check_volume_guest_paths(&[resolved(bad, "/h", true)]).is_err(),
            "{bad} should be rejected"
        );
    }
    for ok in [
        "/data",
        "/var/lib/app",
        "/nixos",
        "/configs",
        "/runner",
        "/srv/my data",
    ] {
        check_volume_guest_paths(&[resolved(ok, "/h", true)]).unwrap();
    }
}

#[test]
fn volume_shares_and_guest_mounts_agree() {
    use super::spec::{render_guest_mounts, volume_fs_mounts};
    let vols = [
        resolved("/data", "/var/lib/russel/api/volumes/data", true),
        resolved("/srv/my media", "/srv/media", false),
    ];
    let fs = volume_fs_mounts(Path::new("/var/lib/russel/api"), &vols);
    assert_eq!(fs.len(), 2);
    assert_eq!(fs[0].tag, "vol0");
    assert_eq!(
        fs[0].socket,
        PathBuf::from("/var/lib/russel/api/virtiofs-vol0.sock")
    );
    assert_eq!(
        fs[0].shared_dir,
        PathBuf::from("/var/lib/russel/api/volumes/data")
    );
    assert!(!fs[0].readonly);
    assert!(fs[1].readonly);
    assert_eq!(
        render_guest_mounts(&vols),
        "vol0 rw /data\nvol1 ro /srv/my media\n"
    );
}

#[test]
fn agent_mounts_each_volume_line() {
    use super::spec::render_guest_mounts;
    let start = AGENT_INIT_SCRIPT
        .find("if [ -f /config/mounts ]")
        .expect("mounts block");
    let end = start
        + AGENT_INIT_SCRIPT[start..]
            .find("\nfi\n")
            .expect("mounts block end")
        + 4;
    let tmp = tempfile::tempdir().unwrap();
    let mounts = tmp.path().join("mounts");
    std::fs::write(
        &mounts,
        render_guest_mounts(&[
            resolved("/data", "/h1", true),
            resolved("/srv/my media", "/h2", false),
        ]),
    )
    .unwrap();
    let block = AGENT_INIT_SCRIPT[start..end]
        .replace("/config/mounts", mounts.to_str().unwrap())
        .replace("/bin/mkdir -p", ": mkdir")
        .replace("/bin/mount -t virtiofs", "printf 'MOUNT %s|%s|%s|%s\\n'");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&block)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let calls: Vec<&str> = stdout.lines().filter(|l| l.starts_with("MOUNT ")).collect();
    // rw: no -o ro; ro: `-o ro` splits into two words before tag and path.
    assert_eq!(
        calls,
        vec!["MOUNT vol0|/data||", "MOUNT -o|ro|vol1|/srv/my media"]
    );
}
