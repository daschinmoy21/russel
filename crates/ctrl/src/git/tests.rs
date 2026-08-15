use super::allowlist::{
    extract_url_host, is_git_host_allowlisted, reject_blocked_ipv4, reject_blocked_ipv6,
    validate_remote_host, validate_remote_host_dns,
};
use super::client::{
    GitClient, checkout_dir_name, clone_security_config_args, fnv1a_u64, local_path_deploy_allowed,
    reserve_checkout_dir, unique_checkout_dir_name, unique_checkout_dir_name_with,
};
use super::gc::gc_old_checkouts;
use super::lease::active_checkouts;
use std::{
    collections::HashSet,
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr as _,
    time::{Duration, SystemTime},
};
use tempfile::TempDir;

#[test]
fn fnv1a_uses_xor_before_multiply() {
    assert_eq!(fnv1a_u64("hello"), 0xa430_d846_80aa_bd0b);
}

#[test]
fn checkout_dir_name_is_unique_for_distinct_urls() {
    let a = checkout_dir_name("https://github.com/org/repo-a.git");
    let b = checkout_dir_name("https://github.com/org/repo-b.git");
    assert_ne!(a, b);
    // Same URL is stable.
    assert_eq!(a, checkout_dir_name("https://github.com/org/repo-a.git"));
    // 16 hex digits in the hash suffix.
    let hash = a.rsplit('-').next().unwrap();
    assert_eq!(hash.len(), 16);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn checkout_dir_name_avoids_sanitize_collision() {
    // Identical 48-char sanitized prefix; only the hash must distinguish them.
    let base = "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let a = checkout_dir_name(&format!("{base}/one"));
    let b = checkout_dir_name(&format!("{base}/two"));
    assert_ne!(a, b);
    // Prefixes before the 16-hex hash should match.
    let pa = a.rsplit_once('-').unwrap().0;
    let pb = b.rsplit_once('-').unwrap().0;
    assert_eq!(pa, pb);
}

#[test]
fn gc_removes_only_stale_dirs() {
    let tmp = TempDir::new().unwrap();
    let stale = tmp.path().join("stale-checkout");
    let fresh = tmp.path().join("fresh-checkout");
    fs::create_dir_all(&stale).unwrap();
    fs::create_dir_all(&fresh).unwrap();

    let now = SystemTime::now();
    let stale_time = now - Duration::from_secs(2 * 3600);
    fs::File::open(&stale)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stale_time))
        .unwrap();
    fs::File::open(&fresh)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(now))
        .unwrap();

    let removed = gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap();
    assert_eq!(removed, 1);
    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn unique_checkout_dirs_share_stem() {
    let stem = checkout_dir_name("https://github.com/org/repo.git");
    let a = unique_checkout_dir_name("https://github.com/org/repo.git");
    assert!(a.starts_with(&stem));
    assert!(a.contains("-d"));
}

#[test]
fn unique_checkout_names_differ_with_equal_timestamps() {
    let repo = "https://github.com/org/repo.git";
    let a = unique_checkout_dir_name_with(repo, 42, 10);
    let b = unique_checkout_dir_name_with(repo, 42, 11);
    assert_ne!(a, b);
}

#[test]
fn concurrent_checkout_names_are_unique() {
    let repo = "https://github.com/org/repo.git";
    let names: HashSet<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| scope.spawn(|| unique_checkout_dir_name(repo)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(names.len(), 16);
}

#[test]
fn checkout_leases_are_ref_counted() {
    let tmp = TempDir::new().unwrap();
    let checkout = tmp.path().join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    fs::File::open(&checkout)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(2 * 3600)),
        )
        .unwrap();

    let first = GitClient.hold_checkout(&checkout);
    let second = first.clone();
    assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 2);
    drop(first);
    assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 1);
    assert_eq!(
        gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
        0
    );
    drop(second);
    assert!(!active_checkouts().contains_key(&checkout));
    assert_eq!(
        gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
        1
    );
}

#[test]
fn checkout_destination_is_atomically_reserved() {
    let tmp = TempDir::new().unwrap();
    let repo = "https://github.com/org/repo.git";
    let a = reserve_checkout_dir(tmp.path(), repo).unwrap();
    let b = reserve_checkout_dir(tmp.path(), repo).unwrap();
    assert_ne!(a, b);
    assert!(a.is_dir());
    assert!(b.is_dir());
}

#[test]
fn extract_url_host_handles_userinfo_ipv6_and_ports() {
    assert_eq!(
        extract_url_host("https://x@169.254.169.254/repo.git", "https").unwrap(),
        "169.254.169.254"
    );
    assert_eq!(
        extract_url_host("https://user:pass@github.com:8443/repo.git", "https").unwrap(),
        "github.com"
    );
    assert_eq!(
        extract_url_host("http://[fe80::1]:8080/repo.git", "http").unwrap(),
        "fe80::1"
    );
    assert_eq!(
        extract_url_host("http://[::1]/repo.git", "http").unwrap(),
        "::1"
    );
    assert_eq!(
        extract_url_host("ssh://git@github.com/user/repo.git", "ssh").unwrap(),
        "github.com"
    );
    assert_eq!(
        extract_url_host("ssh://git@github.com:22/user/repo.git", "ssh").unwrap(),
        "github.com"
    );
    assert_eq!(
        extract_url_host("git@github.com:user/repo.git", "git-scp").unwrap(),
        "github.com"
    );
}

#[test]
fn userinfo_metadata_bypass_is_rejected() {
    let repo = "https://x@169.254.169.254/repo.git";
    let err = extract_url_host(repo, "https")
        .and_then(|h| validate_remote_host(&h))
        .unwrap_err();
    assert!(err.to_string().contains("link-local"));
}

#[test]
fn ipv6_link_local_and_loopback_rejected() {
    for repo in [
        "http://[fe80::1]/repo.git",
        "http://[fe80::1%25eth0]/repo.git",
        "http://[::1]/repo.git",
        "http://[fd00:ec2::254]/repo.git",
    ] {
        let err = extract_url_host(repo, "http")
            .and_then(|h| validate_remote_host(&h))
            .unwrap_err();
        assert!(
            err.to_string().contains("link-local")
                || err.to_string().contains("loopback")
                || err.to_string().contains("EC2 metadata")
                || err.to_string().contains("percent-encoded"),
            "unexpected error for {repo}: {err}"
        );
    }
}

#[test]
fn cloud_metadata_ipv4_ranges_rejected() {
    for repo in [
        "https://169.254.169.254/repo.git",
        "https://169.254.0.1/repo.git",
        "https://100.100.100.100/repo.git",
        "https://100.100.2.34/repo.git",
        "https://0.0.0.0/repo.git",
    ] {
        assert!(
            extract_url_host(repo, "https")
                .and_then(|h| validate_remote_host(&h))
                .is_err(),
            "expected rejection for {repo}"
        );
    }
}

#[test]
fn loopback_hosts_rejected_for_remote_schemes() {
    for repo in [
        "https://127.0.0.1/repo.git",
        "https://localhost/repo.git",
        "https://127.1.0.1/repo.git",
        "ssh://127.0.0.1/repo.git",
        "git@127.0.0.1:repo.git",
        "git@localhost:repo.git",
    ] {
        assert!(
            extract_url_host(
                repo,
                if repo.starts_with("https://") {
                    "https"
                } else if repo.starts_with("ssh://") {
                    "ssh"
                } else {
                    "git-scp"
                }
            )
            .and_then(|h| validate_remote_host(&h))
            .is_err(),
            "expected rejection for {repo}"
        );
    }
}

#[test]
fn legitimate_remote_hosts_accepted() {
    for repo in [
        "https://github.com/org/repo.git",
        "https://git.example.com:8443/org/repo.git",
        "ssh://git@github.com/org/repo.git",
        "git@github.com:org/repo.git",
    ] {
        assert!(
            extract_url_host(
                repo,
                if repo.starts_with("https://") {
                    "https"
                } else if repo.starts_with("ssh://") {
                    "ssh"
                } else {
                    "git-scp"
                }
            )
            .and_then(|h| validate_remote_host(&h))
            .is_ok(),
            "expected acceptance for {repo}"
        );
    }
}

#[test]
fn non_canonical_ipv4_forms_rejected() {
    for host in [
        "0177.0.0.1",
        "0x7f.0.0.1",
        "127.0.0.01",
        "2130706433",
        "127.0.0.1.",
    ] {
        assert!(
            validate_remote_host(host).is_err(),
            "expected rejection for {host}"
        );
    }
}

#[test]
fn ipv4_mapped_and_unspecified_ipv6_rejected() {
    for host in [
        "::ffff:169.254.169.254",
        "::ffff:127.0.0.1",
        "::",
        "0:0:0:0:0:0:0:0",
    ] {
        assert!(
            validate_remote_host(host).is_err(),
            "expected rejection for {host}"
        );
    }
}

#[test]
fn rfc1918_and_cgnat_literal_ips_rejected() {
    for host in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.1",
        "192.168.0.1",
        "192.168.255.255",
        "100.64.0.1",
        "100.127.255.254",
        "100.100.100.200",
    ] {
        let err = validate_remote_host(host).unwrap_err().to_string();
        assert!(
            err.contains("private") || err.contains("carrier-grade") || err.contains("metadata"),
            "expected private/CGNAT/metadata rejection for {host}, got: {err}"
        );
    }
}

#[test]
fn ipv6_ula_rejected() {
    for host in ["fc00::1", "fd12:3456:789a::1", "fd00:ec2::254"] {
        let err = validate_remote_host(host).unwrap_err().to_string();
        assert!(
            err.contains("unique-local") || err.contains("EC2 metadata"),
            "expected ULA/EC2 rejection for {host}, got: {err}"
        );
    }
}

#[test]
fn http_clone_disables_follow_redirects() {
    let https_args = clone_security_config_args("https");
    assert_eq!(https_args, vec!["-c", "http.followRedirects=false"]);
    let http_args = clone_security_config_args("http");
    assert_eq!(http_args, vec!["-c", "http.followRedirects=false"]);
    // ssh / scp clones do not set http.* config
    assert!(clone_security_config_args("ssh").is_empty());
    assert!(clone_security_config_args("git-scp").is_empty());
}

/// Serialize mutations of `RUSSEL_GIT_HOST_ALLOWLIST` and restore even on panic.
fn with_git_host_allowlist<T>(value: &str, f: impl FnOnce() -> T) -> T {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    struct EnvRestore(Option<std::ffi::OsString>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: exclusive LOCK held; only test code mutates this var.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("RUSSEL_GIT_HOST_ALLOWLIST", v),
                    None => std::env::remove_var("RUSSEL_GIT_HOST_ALLOWLIST"),
                }
            }
        }
    }

    let prev = std::env::var_os("RUSSEL_GIT_HOST_ALLOWLIST");
    let _restore = EnvRestore(prev);
    // SAFETY: exclusive lock held for the duration of the mutation + body.
    unsafe {
        std::env::set_var("RUSSEL_GIT_HOST_ALLOWLIST", value);
    }
    f()
}

#[test]
fn git_host_allowlist_parses_comma_separated() {
    with_git_host_allowlist("git.internal.example, Other.Git.Local", || {
        assert!(is_git_host_allowlisted("git.internal.example"));
        assert!(is_git_host_allowlisted("GIT.INTERNAL.EXAMPLE"));
        assert!(is_git_host_allowlisted("other.git.local"));
        assert!(!is_git_host_allowlisted("evil.example"));
        assert!(!is_git_host_allowlisted("10.0.0.1"));
    });
}

#[test]
fn reject_blocked_helpers_cover_dns_path_ips() {
    // DNS results reuse the same helpers as literal hosts.
    assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(10, 1, 2, 3)).is_err());
    assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(100, 64, 0, 1)).is_err());
    assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(8, 8, 8, 8)).is_ok());
    assert!(reject_blocked_ipv6("resolved", Ipv6Addr::from_str("fd12::1").unwrap()).is_err());
    assert!(
        reject_blocked_ipv6(
            "resolved",
            Ipv6Addr::from_str("2001:4860:4860::8888").unwrap()
        )
        .is_ok()
    );
}

#[tokio::test]
async fn dns_check_skips_literal_ips() {
    // Literals are not re-resolved; private ones already fail validate_remote_host.
    assert!(validate_remote_host_dns("8.8.8.8").await.is_ok());
    assert!(validate_remote_host_dns("10.0.0.1").await.is_ok());
}

// Serialize env mutations: cargo runs tests in parallel by default.
// Sync + Drop restore (no await across MutexGuard — clippy await_holding_lock).

/// Serialize mutations of `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` and restore even on panic.
fn with_local_path_deploy_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    struct EnvRestore(Option<std::ffi::OsString>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: exclusive LOCK held; only test code mutates this var.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", v),
                    None => std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
                }
            }
        }
    }

    let prev = std::env::var_os("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY");
    let _restore = EnvRestore(prev);
    // SAFETY: exclusive lock held for the duration of the mutation + body.
    unsafe {
        match value {
            Some(v) => std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", v),
            None => std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
        }
    }
    f()
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn local_path_deploy_allowed_defaults_off() {
    with_local_path_deploy_env(None, || {
        assert!(!local_path_deploy_allowed());
    });
    with_local_path_deploy_env(Some("0"), || {
        assert!(!local_path_deploy_allowed());
    });
    with_local_path_deploy_env(Some("true"), || {
        assert!(!local_path_deploy_allowed());
    });
    with_local_path_deploy_env(Some("1"), || {
        assert!(local_path_deploy_allowed());
    });
    with_local_path_deploy_env(Some(" 1 "), || {
        assert!(local_path_deploy_allowed());
    });
}

#[test]
fn local_absolute_path_rejected_when_gate_off() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().canonicalize().unwrap();
    let path_str = path.display().to_string();
    let rt = current_thread_runtime();

    with_local_path_deploy_env(None, || {
        rt.block_on(async {
            let err = GitClient
                .clone_or_use_local(&path_str)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY") && err.contains("disabled"),
                "unexpected error: {err}"
            );
        });
    });

    with_local_path_deploy_env(Some("0"), || {
        rt.block_on(async {
            let err = GitClient
                .clone_or_use_local(&path_str)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
                "unexpected error: {err}"
            );
        });
    });
}

#[test]
fn local_absolute_path_accepted_when_gate_on() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().canonicalize().unwrap();
    let path_str = path.display().to_string();
    let rt = current_thread_runtime();

    with_local_path_deploy_env(Some("1"), || {
        rt.block_on(async {
            let (resolved, _lease) = GitClient
                .clone_or_use_local(&path_str)
                .await
                .expect("local path should be accepted when gate is on");
            assert_eq!(resolved, path);
        });
    });
}

#[test]
fn local_absolute_path_still_checks_exists_and_dotdot_when_gate_on() {
    let rt = current_thread_runtime();
    with_local_path_deploy_env(Some("1"), || {
        rt.block_on(async {
            let missing = "/tmp/russel-local-path-gate-missing-xyz-196";
            let err = GitClient
                .clone_or_use_local(missing)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("does not exist"),
                "expected existence check, got: {err}"
            );

            // Absolute path containing '..' components is rejected even when allowed.
            let with_dotdot = "/tmp/../etc";
            let err = GitClient
                .clone_or_use_local(with_dotdot)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("..") || err.contains("does not exist"),
                "expected .. or existence rejection, got: {err}"
            );
        });
    });
}
