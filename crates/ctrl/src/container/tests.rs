use super::passthrough::validate_nix_store_source;
use super::podman_user::{configured_podman_user, resolve_podman_user};
use super::rootfs::select_nix_tool_store_path;
use super::runner::{
    NIX_STORE_MOUNT, is_trusted_container_name, resolve_container_name, rootless_required_error,
};
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn fake_store(base: &Path, hash: &str, bin_name: &str, contents: &[u8]) -> PathBuf {
    let store = base.join("nix").join("store").join(hash);
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin_path = bin_dir.join(bin_name);
    std::fs::write(&bin_path, contents).unwrap();
    std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    store
}

fn fake_nix_tool(base: &Path, name: &str) -> PathBuf {
    let store = base.join("nix").join("store").join(format!("fake-{name}"));
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin_path = bin_dir.join(name);
    std::fs::write(&bin_path, format!("#!/bin/sh\necho {name}\n")).unwrap();
    std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    store
}

#[test]
fn select_nix_tool_store_path_picks_bin_output_from_multi_line() {
    let tmp = tempfile::tempdir().unwrap();
    let man = tmp.path().join("bash-man");
    let out = tmp.path().join("bash-out");
    std::fs::create_dir_all(man.join("share/man")).unwrap();
    std::fs::create_dir_all(out.join("bin")).unwrap();
    std::fs::write(out.join("bin/bash"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(out.join("bin/bash"), std::fs::Permissions::from_mode(0o755)).unwrap();

    // man listed first (as real `nix build bash` often does), then out.
    let stdout = format!("{}\n{}\n", man.display(), out.display());
    let path = select_nix_tool_store_path(stdout.as_bytes(), "bash").unwrap();
    assert_eq!(path, out);

    let err =
        select_nix_tool_store_path(format!("{}\n", man.display()).as_bytes(), "bash").unwrap_err();
    assert!(err.to_string().contains("none of the nix build outputs"));

    let empty = select_nix_tool_store_path(b"\n  \n", "bash").unwrap_err();
    assert!(empty.to_string().contains("no store paths"));
}

#[test]
fn select_nix_tool_store_path_prefers_curl_bin_over_lib_out() {
    // Mirrors nixpkgs curl: `bin` has bin/curl, `out` is libs only, `man` is manpages.
    let tmp = tempfile::tempdir().unwrap();
    let lib_out = tmp.path().join("curl-out");
    let bin_out = tmp.path().join("curl-bin");
    let man_out = tmp.path().join("curl-man");
    std::fs::create_dir_all(lib_out.join("lib")).unwrap();
    std::fs::create_dir_all(bin_out.join("bin")).unwrap();
    std::fs::create_dir_all(man_out.join("share/man")).unwrap();
    std::fs::write(bin_out.join("bin/curl"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        bin_out.join("bin/curl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let stdout = format!(
        "{}\n{}\n{}\n",
        bin_out.display(),
        man_out.display(),
        lib_out.display()
    );
    let path = select_nix_tool_store_path(stdout.as_bytes(), "curl").unwrap();
    assert_eq!(path, bin_out);
}

#[tokio::test]
async fn prepare_rootfs_creates_layout_and_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = fake_store(tmp.path(), "fake-app-package", "app", b"\x7fELF");
    let bash = fake_nix_tool(tmp.path(), "bash");
    let curl = fake_nix_tool(tmp.path(), "curl");

    let spec = RootfsSpec {
        service_id: "test-svc".into(),
        store_path: store,
        bin_name: "app".into(),
        base_dir: tmp.path().to_path_buf(),
        bash_store: Some(bash),
        curl_store: Some(curl),
        debug: false,
    };

    let prepared = prepare_rootfs(&spec).await.unwrap();
    let rootfs = &prepared.rootfs_path;

    for dir in [
        "tmp", "var", "var/tmp", "etc", "bin", "usr", "usr/bin", "dev", "proc", "sys", "run",
        "home", "root",
    ] {
        assert!(rootfs.join(dir).is_dir(), "missing directory {dir}");
    }

    let tmp_mode = std::fs::metadata(rootfs.join("tmp"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(tmp_mode, 0o1777);

    assert_eq!(prepared.entrypoint, PathBuf::from("/bin/app"));

    // debug: false — bash, curl, and env wrapper must be absent.
    assert!(
        !rootfs.join("bin/bash").exists(),
        "bash must not be present when debug=false"
    );
    assert!(
        !rootfs.join("bin/curl").exists(),
        "curl must not be present when debug=false"
    );
    assert!(
        !rootfs.join("usr/bin/env").exists(),
        "/usr/bin/env must not be present when debug=false"
    );
}

#[tokio::test]
async fn prepare_rootfs_creates_bin_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let store = fake_store(tmp.path(), "fake-myapp-package", "myapp", b"\x7fELF");
    let bash = fake_nix_tool(tmp.path(), "bash");
    let curl = fake_nix_tool(tmp.path(), "curl");

    let spec = RootfsSpec {
        service_id: "test-svc".into(),
        store_path: store.clone(),
        bin_name: "myapp".into(),
        base_dir: tmp.path().to_path_buf(),
        bash_store: Some(bash),
        curl_store: Some(curl),
        debug: false,
    };

    let prepared = prepare_rootfs(&spec).await.unwrap();
    let link = prepared.rootfs_path.join("bin/myapp");
    assert!(link.is_symlink());
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(
        target,
        PathBuf::from("/nix/store/fake-myapp-package/bin/myapp")
    );
}

#[test]
fn validate_entrypoint_fails_when_bin_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("empty-store");
    std::fs::create_dir_all(store.join("bin")).unwrap();

    let err = validate_entrypoint(&store, "missing", false).unwrap_err();
    assert!(err.to_string().contains("entrypoint missing"));
}

#[test]
fn validate_entrypoint_succeeds_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let store = fake_store(tmp.path(), "fake-app-package", "app", b"\x7fELF");

    let path = validate_entrypoint(&store, "app", false).unwrap();
    assert_eq!(path, store.join("bin/app"));
}

#[test]
fn validate_entrypoint_checks_shebang_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("app-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    // Place interpreter under the store path so starts_with(store_path) matches.
    let bash = bin_dir.join("bash");
    std::fs::write(&bash, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = format!("#!{}/bin/bash\necho hi\n", store.display());
    let bin_path = bin_dir.join("runner");
    std::fs::write(&bin_path, script).unwrap();

    validate_entrypoint(&store, "runner", false).unwrap();
}

#[test]
fn validate_entrypoint_rejects_missing_shebang_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("app-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(
        bin_dir.join("runner"),
        "#!/nix/store/does-not-exist-bash/bin/bash\necho hi\n",
    )
    .unwrap();

    let err = validate_entrypoint(&store, "runner", false).unwrap_err();
    assert!(err.to_string().contains("shebang interpreter"));
}

#[test]
fn container_name_uses_russel_prefix() {
    assert_eq!(
        ContainerRunner::container_name("my-service"),
        "russel-my-service"
    );
}

#[test]
fn container_name_rejects_invalid_service_id_in_run_args() {
    let spec = ContainerStartSpec {
        service_id: "../evil".into(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from("/var/lib/russel/evil/rootfs"),
            entrypoint: PathBuf::from("/bin/app"),
        },
        host_port: 8080,
        guest_port: 3000,
        memory_mb: 256,
        env: vec![],
        extra_args: vec![],
    };
    let err = build_run_args(&spec, &container_log_path("evil")).unwrap_err();
    assert!(err.to_string().contains("path separators"));
}

#[test]
fn build_run_args_includes_rootfs_mount_ports_memory_and_labels() {
    let spec = ContainerStartSpec {
        service_id: "api-1".into(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from("/var/lib/russel/api-1/rootfs"),
            entrypoint: PathBuf::from("/bin/api"),
        },
        host_port: 8080,
        guest_port: 3000,
        memory_mb: 512,
        env: vec![
            ("PORT".into(), "3000".into()),
            ("RUSSEL".into(), "1".into()),
        ],
        extra_args: vec![],
    };
    let log_path = PathBuf::from("/var/lib/russel/api-1/container.log");
    let args = build_run_args(&spec, &log_path).unwrap();

    assert_eq!(args[0], "run");
    assert!(args.contains(&"-d".to_string()));
    assert!(args.contains(&"--name".to_string()));
    assert!(args.contains(&"russel-api-1".to_string()));
    assert!(args.contains(&"--label".to_string()));
    assert!(args.contains(&"russel.service=api-1".to_string()));
    assert!(args.contains(&"russel.runtime=container".to_string()));
    assert!(args.contains(&"--rootfs".to_string()));
    assert!(args.contains(&"/var/lib/russel/api-1/rootfs".to_string()));
    assert!(args.contains(&"--mount".to_string()));
    assert!(args.contains(&NIX_STORE_MOUNT.to_string()));
    assert!(args.contains(&"-p".to_string()));
    // Publish bind may be 127.0.0.1 (default) or 0.0.0.0 (legacy wildcard).
    let has_port = args
        .iter()
        .any(|a| a == "8080:3000" || a.ends_with(":8080:3000") || a == "127.0.0.1:8080:3000");
    assert!(has_port, "expected host:guest port mapping in {args:?}");
    assert!(args.contains(&"--memory".to_string()));
    assert!(args.contains(&"512m".to_string()));
    assert!(args.contains(&"--workdir".to_string()));
    assert!(args.contains(&"/".to_string()));
    assert!(args.contains(&"--log-driver".to_string()));
    assert!(args.contains(&"k8s-file".to_string()));
    assert!(args.contains(&"--log-opt".to_string()));
    assert!(args.contains(&"path=/var/lib/russel/api-1/container.log".to_string()));
    assert!(args.contains(&"-e".to_string()));
    assert!(args.contains(&"PORT=3000".to_string()));
    assert!(args.contains(&"RUSSEL=1".to_string()));
    assert_eq!(args.last().unwrap(), "/bin/api");

    // ── Hardening flags: verify flag/value adjacency, not just presence ──
    let has_adjacent =
        |flag: &str, val: &str| -> bool { args.windows(2).any(|w| w[0] == flag && w[1] == val) };
    assert!(
        has_adjacent("--cap-drop", "ALL"),
        "--cap-drop ALL must be adjacent"
    );
    assert!(
        has_adjacent("--security-opt", "no-new-privileges"),
        "--security-opt no-new-privileges must be adjacent"
    );
    assert!(
        args.contains(&"--read-only".to_string()),
        "--read-only must be present"
    );
    assert!(
        has_adjacent("--tmpfs", "/tmp"),
        "--tmpfs /tmp must be adjacent"
    );
    assert!(
        has_adjacent("--tmpfs", "/run"),
        "--tmpfs /run must be adjacent"
    );

    // --rootfs PATH must be immediately before COMMAND so flags are not
    // misparsed as the container executable (crun: `--mount` not found).
    let rootfs_pos = args.iter().position(|a| a == "--rootfs").unwrap();
    let mount_pos = args.iter().position(|a| a == "--mount").unwrap();
    assert!(
        mount_pos < rootfs_pos,
        "--mount must appear before --rootfs; got {args:?}"
    );
    assert_eq!(args[rootfs_pos + 1], "/var/lib/russel/api-1/rootfs");
    assert_eq!(args[rootfs_pos + 2], "/bin/api");
    assert_eq!(rootfs_pos + 3, args.len());
}

#[test]
fn parse_podman_rootless_reads_host_security_field() {
    let info = r#"{"host":{"security":{"rootless":true}}}"#;
    assert!(parse_podman_rootless(info).unwrap());

    let info = r#"{"host":{"security":{"rootless":false}}}"#;
    assert!(!parse_podman_rootless(info).unwrap());
}

#[test]
fn parse_podman_rootless_errors_when_field_missing() {
    let info = r#"{"host":{"security":{}}}"#;
    let err = parse_podman_rootless(info).unwrap_err();
    assert!(err.to_string().contains("missing .host.security.rootless"));
}

#[test]
fn rootless_required_error_message_is_actionable() {
    let msg = rootless_required_error("false");
    assert!(msg.contains("Russel containers require rootless Podman"));
    assert!(msg.contains(".host.security.rootless detected false"));
}

#[test]
fn validate_podman_passthrough_rejects_reserved_flags() {
    for arg in [
        "--rootfs",
        "--rootfs=/tmp",
        "--name",
        "--name=evil",
        "-n",
        "--replace",
        "--replace=true",
        "-d",
        "--detach",
        "--detach=true",
    ] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("reserved by Russel"),
            "expected rejection for {arg}, got {err}"
        );
    }
}

#[test]
fn validate_podman_passthrough_accepts_common_flags() {
    let args = vec![
        "-v".into(),
        "/nix/store/abc123:/dest:ro".into(),
        "--mount".into(),
        "type=bind,src=/nix/store/abc123,dst=/dest,readonly".into(),
        "--network".into(),
        "bridge".into(),
        "--cap-drop".into(),
        "NET_RAW".into(),
        "--env".into(),
        "FOO=bar".into(),
    ];
    validate_podman_passthrough_args(&args).unwrap();
}

// ── Issue #2: PORT override rejection ───────────────────────────────────

#[test]
fn reject_port_override_via_e_flag() {
    for (arg, next) in [("-e", Some("PORT=3000")), ("--env", Some("PORT=3000"))] {
        let err = validate_podman_passthrough_args(&[arg.to_string(), next.unwrap().to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("PORT"),
            "expected PORT rejection for {arg}"
        );
    }
}

#[test]
fn reject_port_override_via_e_equals_form() {
    for arg in ["-ePORT=3000", "--env=PORT=3000"] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("PORT"),
            "expected PORT rejection for {arg}"
        );
    }
}

#[test]
fn managed_port_appears_after_passthrough_env() {
    let spec = ContainerStartSpec {
        service_id: "api-1".into(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from("/var/lib/russel/api-1/rootfs"),
            entrypoint: PathBuf::from("/bin/api"),
        },
        host_port: 8080,
        guest_port: 3000,
        memory_mb: 512,
        env: vec![("PORT".into(), "3000".into())],
        extra_args: vec!["-e".into(), "FOO=bar".into()],
    };
    let log_path = PathBuf::from("/var/lib/russel/api-1/container.log");
    let args = build_run_args(&spec, &log_path).unwrap();
    // Passthrough -e FOO=bar must appear before managed -e PORT=3000
    let foo_pos = args.iter().position(|a| a == "FOO=bar").unwrap();
    let port_pos = args.iter().position(|a| a == "PORT=3000").unwrap();
    assert!(
        foo_pos < port_pos,
        "passthrough -e FOO=bar should appear before managed PORT"
    );
}

// ── Issue #3: isolation-weakening rejection ─────────────────────────────

#[test]
fn reject_privileged() {
    for arg in ["--privileged", "--privileged=true"] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("privileged"),
            "expected rejection for {arg}"
        );
    }
}

#[test]
fn reject_network_host() {
    for (flag, value) in [("--network", "host"), ("--net", "host"), ("-net", "host")] {
        let err =
            validate_podman_passthrough_args(&[flag.to_string(), value.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("host"),
            "expected rejection for {flag}"
        );
    }
    for eq in ["--network=host", "--net=host"] {
        let err = validate_podman_passthrough_args(&[eq.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("host"),
            "expected rejection for {eq}"
        );
    }
}

#[test]
fn accept_network_bridge() {
    validate_podman_passthrough_args(&["--network".into(), "bridge".into()]).unwrap();
    validate_podman_passthrough_args(&["--network=bridge".into()]).unwrap();
}

#[test]
fn reject_cap_add() {
    for form in ["--cap-add".to_string(), "--cap-add=SYS_ADMIN".to_string()] {
        let err = validate_podman_passthrough_args(&[form]).unwrap_err();
        assert!(err.to_string().contains("cap-add"), "expected rejection");
    }
}

#[test]
fn accept_cap_drop() {
    validate_podman_passthrough_args(&["--cap-drop".into(), "NET_RAW".into()]).unwrap();
    validate_podman_passthrough_args(&["--cap-drop=NET_RAW".into()]).unwrap();
}

#[test]
fn reject_device_and_add_device() {
    for (flag, next) in [
        ("--device", Some("/dev/sda")),
        ("--device=/dev/sda", None),
        ("--add-device", Some("/dev/sda")),
        ("--add-device=/dev/sda", None),
    ] {
        let mut args = vec![flag.to_string()];
        if let Some(v) = next {
            args.push(v.to_string());
        }
        let err = validate_podman_passthrough_args(&args).unwrap_err();
        assert!(
            err.to_string().contains("device"),
            "expected rejection for {flag}"
        );
    }
}

#[test]
fn reject_port_publish() {
    for flag in [
        "-p",
        "--publish",
        "--publish-all",
        "-P",
        "--publish=0:8080:80",
    ] {
        let err = validate_podman_passthrough_args(&[flag.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("publish") || err.to_string().contains("port"),
            "expected rejection for {flag}"
        );
    }
}

// ── Volume / mount denials ────────────────────────────────────────────

#[test]
fn reject_volume_non_nix_store_source() {
    for val in [
        "/data:/dest:ro",
        "/etc/passwd:/dest:ro",
        "relative:/dest:ro",
    ] {
        let err = validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("not under /nix/store/"),
            "expected rejection for -v {val}: {err}"
        );
    }
}

#[test]
fn reject_volume_missing_ro() {
    let err = validate_podman_passthrough_args(&["-v".into(), "/nix/store/abc123:/dest".into()])
        .unwrap_err();
    assert!(
        err.to_string().contains("not read-only"),
        "expected ro rejection: {err}"
    );
}

#[test]
fn accept_volume_nix_store_with_ro() {
    validate_podman_passthrough_args(&["-v".into(), "/nix/store/abc123:/dest:ro".into()]).unwrap();
    // :ro anywhere in options is accepted.
    validate_podman_passthrough_args(&["-v".into(), "/nix/store/abc123:/dest:z,ro".into()])
        .unwrap();
}

#[test]
fn reject_volume_compact_form_non_nix_store() {
    let err = validate_podman_passthrough_args(&["-v/data:/dest:ro".into()]).unwrap_err();
    assert!(err.to_string().contains("not under /nix/store/"), "{err}");
}

#[test]
fn accept_volume_compact_form_valid() {
    validate_podman_passthrough_args(&["-v/nix/store/abc123:/dest:ro".into()]).unwrap();
}

#[test]
fn reject_volume_equals_form_non_nix_store() {
    let err = validate_podman_passthrough_args(&["--volume=/data:/dest:ro".into()]).unwrap_err();
    assert!(err.to_string().contains("not under /nix/store/"), "{err}");
}

#[test]
fn accept_volume_equals_form_valid() {
    validate_podman_passthrough_args(&["--volume=/nix/store/abc123:/dest:ro".into()]).unwrap();
}

#[test]
fn accept_volume_space_form_valid() {
    // --volume (space form) should work identically to -v
    validate_podman_passthrough_args(&["--volume".into(), "/nix/store/abc123:/dest:ro".into()])
        .unwrap();
    validate_podman_passthrough_args(&["--volume".into(), "/nix/store/abc123:/dest:z,ro".into()])
        .unwrap();
}

#[test]
fn reject_volume_space_form_non_nix_store() {
    let err = validate_podman_passthrough_args(&["--volume".into(), "/data:/dest:ro".into()])
        .unwrap_err();
    assert!(err.to_string().contains("not under /nix/store/"), "{err}");
}

#[test]
fn reject_volume_empty_source() {
    let err = validate_podman_passthrough_args(&["-v".into(), ":/dest:ro".into()]).unwrap_err();
    assert!(err.to_string().contains("empty source"), "{err}");
}

#[test]
fn reject_volume_missing_colon() {
    let err = validate_podman_passthrough_args(&["-v".into(), "justapath".into()]).unwrap_err();
    assert!(err.to_string().contains("missing ':' separator"), "{err}");
}

#[test]
fn reject_mount_non_bind_type() {
    for val in [
        "type=volume,src=/nix/store/x,dst=/dest,ro=true",
        "type=tmpfs,dst=/dest",
    ] {
        let err =
            validate_podman_passthrough_args(&["--mount".into(), val.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("only type=bind"),
            "expected rejection for {val}: {err}"
        );
    }
}

#[test]
fn reject_mount_non_nix_store_source() {
    let err = validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,src=/data,dst=/dest,ro=true".into(),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("not under /nix/store/"), "{err}");
}

#[test]
fn reject_mount_missing_readonly() {
    let err = validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,src=/nix/store/x,dst=/dest".into(),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("not read-only"), "{err}");
}

#[test]
fn accept_mount_nix_store_with_readonly() {
    // ro=true form
    validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,src=/nix/store/x,dst=/dest,ro=true".into(),
    ])
    .unwrap();
    // readonly (bare) form
    validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,source=/nix/store/x,dst=/dest,readonly".into(),
    ])
    .unwrap();
    // ro=1 form
    validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,src=/nix/store/x,dst=/dest,ro=1".into(),
    ])
    .unwrap();
}

#[test]
fn accept_mount_compact_form_valid() {
    validate_podman_passthrough_args(&[
        "--mount=type=bind,src=/nix/store/x,dst=/dest,ro=true".into()
    ])
    .unwrap();
}

#[test]
fn reject_mount_missing_source() {
    let err =
        validate_podman_passthrough_args(&["--mount".into(), "type=bind,dst=/dest,ro=true".into()])
            .unwrap_err();
    assert!(err.to_string().contains("missing source"), "{err}");
}

// ── Path-traversal / allowlist hardening (#192) ───────────────────────

#[test]
fn reject_volume_path_traversal_past_nix_store() {
    for val in [
        "/nix/store/../etc/shadow:/dest:ro",
        "/nix/store/../../home/x/.ssh:/dest:ro",
        "/nix/store/foo/../../../etc/passwd:/dest:ro",
        "/nix/store/abc/../def/../../etc/shadow:/dest:ro",
        "/nix/store/hash-pkg/bin/../../../../etc/shadow:/dest:ro",
    ] {
        let err = validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("..") || msg.contains("not under /nix/store/"),
            "expected traversal rejection for -v {val}: {err}"
        );
    }
}

#[test]
fn reject_mount_path_traversal_past_nix_store() {
    for src in [
        "/nix/store/../etc/shadow",
        "/nix/store/../../home/x/.ssh",
        "/nix/store/foo/../bar/../../etc/passwd",
        "/nix/store/hash/bin/../../../etc/shadow",
    ] {
        let val = format!("type=bind,src={src},dst=/dest,ro=true");
        let err = validate_podman_passthrough_args(&["--mount".into(), val.clone()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("..") || msg.contains("not under /nix/store/"),
            "expected traversal rejection for --mount src={src}: {err}"
        );
    }
}

#[test]
fn accept_legitimate_nix_store_volume_paths() {
    // Realistic nix store-style paths (hash-like prefix + package name).
    validate_podman_passthrough_args(&[
        "-v".into(),
        "/nix/store/abc123def456ghi789jkl0mnopqrstuv-hello-2.12/bin/hello:/app/bin:ro".into(),
    ])
    .unwrap();
    validate_podman_passthrough_args(&[
        "-v".into(),
        "/nix/store/4s514kmhnmncvcsvjh3d17y7y0psbyc1-busybox-1.37.0:/busybox:ro".into(),
    ])
    .unwrap();
    // Dot segments under the store are fine (collapsed lexically).
    validate_podman_passthrough_args(&["-v".into(), "/nix/store/./abc123/bin/foo:/dest:ro".into()])
        .unwrap();
}

#[test]
fn accept_legitimate_nix_store_mount_paths() {
    validate_podman_passthrough_args(&[
        "--mount".into(),
        "type=bind,src=/nix/store/abc123def456ghi789jkl0mnopqrstuv-pkg/bin/foo,dst=/dest,ro=true"
            .into(),
    ])
    .unwrap();
    validate_podman_passthrough_args(&[
        "--mount=type=bind,source=/nix/store/xyz-pkg/lib,dst=/lib,readonly".into(),
    ])
    .unwrap();
}

#[test]
fn reject_volume_explicit_rw() {
    for val in [
        "/nix/store/abc123:/dest:rw",
        "/nix/store/abc123:/dest:ro,rw",
        "/nix/store/abc123:/dest:rw,ro",
        "/nix/store/abc123:/dest:z,rw",
    ] {
        let err = validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rw") || msg.contains("not read-only") || msg.contains("read-write"),
            "expected rw rejection for -v {val}: {err}"
        );
    }
}

#[test]
fn reject_mount_explicit_rw() {
    for val in [
        "type=bind,src=/nix/store/x,dst=/dest,rw=true",
        "type=bind,src=/nix/store/x,dst=/dest,rw",
        "type=bind,src=/nix/store/x,dst=/dest,ro=true,rw=true",
        "type=bind,src=/nix/store/x,dst=/dest,rw=1",
    ] {
        let err =
            validate_podman_passthrough_args(&["--mount".into(), val.to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rw") || msg.contains("read-write") || msg.contains("not read-only"),
            "expected rw rejection for --mount {val}: {err}"
        );
    }
}

#[test]
fn validate_nix_store_source_direct() {
    // Unit-level checks on the shared helper.
    assert!(validate_nix_store_source("/nix/store/abc123/bin/foo").is_ok());
    assert!(validate_nix_store_source("/nix/store/./abc/bin").is_ok());

    assert!(validate_nix_store_source("").is_err());
    let rel_err = validate_nix_store_source("relative/path")
        .unwrap_err()
        .to_string();
    assert!(
        rel_err.contains("not under /nix/store/"),
        "relative path error: {rel_err}"
    );
    assert!(validate_nix_store_source("/etc/shadow").is_err());
    assert!(validate_nix_store_source("/nix/store").is_err());
    assert!(validate_nix_store_source("/nix/store/../etc/shadow").is_err());
    assert!(validate_nix_store_source("/nix/store/../../home/x/.ssh").is_err());
    assert!(validate_nix_store_source("/nix/store/foo/../../../etc/passwd").is_err());
}

// ── --env-file denial ─────────────────────────────────────────────────

#[test]
fn reject_env_file() {
    for arg in ["--env-file", "--env-file=/etc/host-env"] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("env-file"),
            "expected rejection for {arg}: {err}"
        );
    }
}

// ── --entrypoint denial ───────────────────────────────────────────────

#[test]
fn reject_entrypoint() {
    for arg in ["--entrypoint", "--entrypoint=/bin/sh"] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("entrypoint"),
            "expected rejection for {arg}: {err}"
        );
    }
}

// ── --user / -u denial ────────────────────────────────────────────────

#[test]
fn reject_user() {
    for (arg, next) in [
        ("--user", Some("root")),
        ("-u", Some("0")),
        ("--user=root", None),
        ("-u=0", None),
    ] {
        let mut args = vec![arg.to_string()];
        if let Some(v) = next {
            args.push(v.to_string());
        }
        let err = validate_podman_passthrough_args(&args).unwrap_err();
        assert!(
            err.to_string().contains("user"),
            "expected rejection for {arg}: {err}"
        );
    }
}

// ── --network allowlist ───────────────────────────────────────────────

#[test]
fn reject_network_disallowed() {
    for val in ["ns:/proc/1/ns/net", "container:foo", "private"] {
        let err =
            validate_podman_passthrough_args(&["--network".into(), val.to_string()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("only bridge, none, slirp4netns, pasta"),
            "expected rejection for --network {val}: {err}"
        );
    }
    for eq in ["--network=ns:/proc/1/ns/net", "--net=container:foo"] {
        let err = validate_podman_passthrough_args(&[eq.to_string()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("only bridge, none, slirp4netns, pasta"),
            "expected rejection for {eq}: {err}"
        );
    }
}

#[test]
fn accept_network_allowed_modes() {
    for mode in ["bridge", "none", "slirp4netns", "pasta"] {
        validate_podman_passthrough_args(&["--network".into(), mode.to_string()]).unwrap();
        validate_podman_passthrough_args(&[format!("--network={mode}")]).unwrap();
    }
}

// ── --userns allowlist ────────────────────────────────────────────────

#[test]
fn reject_userns_disallowed() {
    for val in ["host", "auto", "nomap"] {
        let err =
            validate_podman_passthrough_args(&["--userns".into(), val.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("only keep-id"),
            "expected rejection for --userns {val}: {err}"
        );
    }
    for eq in ["--userns=host", "--userns=auto"] {
        let err = validate_podman_passthrough_args(&[eq.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("only keep-id"),
            "expected rejection for {eq}: {err}"
        );
    }
}

#[test]
fn accept_userns_keep_id() {
    validate_podman_passthrough_args(&["--userns".into(), "keep-id".into()]).unwrap();
    validate_podman_passthrough_args(&["--userns=keep-id".into()]).unwrap();
}

#[test]
fn reject_userns_no_value() {
    let err = validate_podman_passthrough_args(&["--userns".into()]).unwrap_err();
    assert!(err.to_string().contains("requires a value"), "{err}");
}

// ── --secret is allowed ───────────────────────────────────────────────

#[test]
fn accept_secret() {
    validate_podman_passthrough_args(&["--secret".into(), "mysecret".into()]).unwrap();
    validate_podman_passthrough_args(&["--secret=mysecret".into()]).unwrap();
}

// ── Issue #191: allowlist default-deny + hardening re-assert ──────────

#[test]
fn reject_unknown_passthrough_flags() {
    for arg in [
        "--hooks-dir=/tmp/hooks",
        "--runtime=runc",
        "--sysctl=net.ipv4.ip_forward=1",
        "--pid=host",
        "--ipc=host",
        "--uts=host",
        "--cgroupns=host",
        "--systemd=always",
        "--pull=always",
        "--gidmap=0:0:1",
        "--uidmap=0:0:1",
        "--executable=/bin/sh",
        "--conmon-pidfile=/tmp/x",
        "--cidfile=/tmp/x",
    ] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not on the allowlist")
                || msg.contains("denied for security")
                || msg.contains("hooks-dir")
                || msg.contains("runtime")
                || msg.contains("pid"),
            "expected allowlist denial for {arg}: {err}"
        );
    }
}

#[test]
fn reject_privileged_all_variants() {
    for arg in [
        "--privileged",
        "--privileged=true",
        "--privileged=false",
        "--privileged=0",
        "--privileged=1",
        "--privileged=no",
    ] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("privileged"),
            "expected rejection for {arg}: {err}"
        );
    }
}

#[test]
fn reject_read_only_false_and_passthrough_read_only() {
    for arg in [
        "--read-only=false",
        "--read-only=0",
        "--read-only",
        "--read-only=true",
    ] {
        let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("read-only"),
            "expected rejection for {arg}: {err}"
        );
    }
}

#[test]
fn reject_denied_flags_as_option_values() {
    let err =
        validate_podman_passthrough_args(&["--label".into(), "--privileged".into()]).unwrap_err();
    assert!(
        err.to_string().contains("privileged"),
        "expected rejection of --privileged passed as value token: {err}"
    );
}

#[test]
fn reject_log_driver_and_hooks_runtime() {
    for arg in [
        "--log-driver=json-file",
        "--log-opt",
        "--hooks-dir",
        "--runtime=crun",
    ] {
        let mut args = vec![arg.to_string()];
        if arg == "--log-opt" || arg == "--hooks-dir" {
            args.push("/tmp/evil".into());
        }
        let err = validate_podman_passthrough_args(&args).unwrap_err();
        assert!(
            err.to_string().contains("denied for security"),
            "expected rejection for {arg}: {err}"
        );
    }
}

#[test]
fn accept_allowlisted_resource_and_metadata_flags() {
    validate_podman_passthrough_args(&[
        "--label".into(),
        "app=api".into(),
        "--annotation".into(),
        "io.russel/x=1".into(),
        "--memory".into(),
        "256m".into(),
        "--cpus".into(),
        "1.5".into(),
        "--tmpfs".into(),
        "/var/cache:size=64m".into(),
        "--shm-size".into(),
        "64m".into(),
        "--hostname".into(),
        "svc".into(),
        "-l".into(),
        "env=prod".into(),
    ])
    .unwrap();
    validate_podman_passthrough_args(&["--memory=512m".into(), "--cpus=2".into()]).unwrap();
    validate_podman_passthrough_args(&["--tmpfs=/data".into()]).unwrap();
}

#[test]
fn reject_tmpfs_non_absolute_destination() {
    let err = validate_podman_passthrough_args(&["--tmpfs".into(), "relative".into()]).unwrap_err();
    assert!(
        err.to_string().contains("absolute"),
        "expected absolute-path rejection: {err}"
    );
}

static ALLOW_PODMAN_ARGS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_allow_podman_args_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = ALLOW_PODMAN_ARGS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var("RUSSEL_ALLOW_PODMAN_ARGS").ok();
    // SAFETY: exclusive lock held for the duration of the mutation + assertion.
    unsafe {
        match value {
            Some(v) => std::env::set_var("RUSSEL_ALLOW_PODMAN_ARGS", v),
            None => std::env::remove_var("RUSSEL_ALLOW_PODMAN_ARGS"),
        }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    unsafe {
        match previous {
            Some(v) => std::env::set_var("RUSSEL_ALLOW_PODMAN_ARGS", v),
            None => std::env::remove_var("RUSSEL_ALLOW_PODMAN_ARGS"),
        }
    }
    match result {
        Ok(v) => v,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn russel_allow_podman_args_zero_rejects_extras() {
    for falsy in ["0", "false", "no", "off", "disabled"] {
        with_allow_podman_args_env(Some(falsy), || {
            let err = validate_podman_passthrough_args(&["--network".into(), "bridge".into()])
                .unwrap_err();
            assert!(
                err.to_string().contains("RUSSEL_ALLOW_PODMAN_ARGS"),
                "expected disable message for {falsy:?}: {err}"
            );
            // Empty extras still ok.
            validate_podman_passthrough_args(&[]).unwrap();
        });
    }
}

#[test]
fn russel_allow_podman_args_truthy_allows_allowlisted() {
    for truthy in ["1", "true", "yes", "on"] {
        with_allow_podman_args_env(Some(truthy), || {
            validate_podman_passthrough_args(&["--network".into(), "bridge".into()]).unwrap();
        });
    }
}

#[test]
fn russel_allow_podman_args_unset_allows_allowlisted() {
    with_allow_podman_args_env(None, || {
        validate_podman_passthrough_args(&["--network".into(), "bridge".into()]).unwrap();
    });
}

#[test]
fn build_run_args_reasserts_hardening_after_passthrough() {
    let spec = ContainerStartSpec {
        service_id: "api-1".into(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from("/var/lib/russel/api-1/rootfs"),
            entrypoint: PathBuf::from("/bin/api"),
        },
        host_port: 8080,
        guest_port: 3000,
        memory_mb: 512,
        env: vec![("PORT".into(), "3000".into())],
        extra_args: vec![
            "--cap-drop".into(),
            "NET_RAW".into(),
            "--network".into(),
            "bridge".into(),
        ],
    };
    let log_path = PathBuf::from("/var/lib/russel/api-1/container.log");
    let args = build_run_args(&spec, &log_path).unwrap();

    let network_pos = args.iter().position(|a| a == "--network").unwrap();
    let passthrough_cap_pos = args
        .windows(2)
        .position(|w| w[0] == "--cap-drop" && w[1] == "NET_RAW")
        .expect("passthrough --cap-drop NET_RAW present");
    let last_cap_drop_all = args
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == "--cap-drop" && w[1] == "ALL")
        .map(|(i, _)| i)
        .next_back()
        .expect("--cap-drop ALL re-assert present");
    let last_no_new_privs = args
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == "--security-opt" && w[1] == "no-new-privileges")
        .map(|(i, _)| i)
        .next_back()
        .expect("no-new-privileges re-assert present");
    let last_read_only = args
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--read-only")
        .map(|(i, _)| i)
        .next_back()
        .expect("--read-only re-assert present");
    let rootfs_pos = args.iter().position(|a| a == "--rootfs").unwrap();

    assert!(
        last_cap_drop_all > passthrough_cap_pos,
        "re-asserted --cap-drop ALL must follow passthrough cap-drop; args={args:?}"
    );
    assert!(
        last_cap_drop_all > network_pos,
        "re-asserted hardening must follow passthrough network; args={args:?}"
    );
    assert!(
        last_no_new_privs > network_pos,
        "re-asserted no-new-privileges must follow passthrough; args={args:?}"
    );
    assert!(
        last_read_only > network_pos,
        "re-asserted --read-only must follow passthrough; args={args:?}"
    );
    assert!(
        last_cap_drop_all < rootfs_pos
            && last_no_new_privs < rootfs_pos
            && last_read_only < rootfs_pos,
        "hardening must still appear before --rootfs; args={args:?}"
    );
}

// ── Issue #4: env wrapper in rootfs ─────────────────────────────────────

#[tokio::test]
async fn prepare_rootfs_creates_env_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let store = fake_store(
        tmp.path(),
        "fake-env-app",
        "app",
        b"#!/usr/bin/env bash\necho hi\n",
    );
    let bash = fake_nix_tool(tmp.path(), "bash");
    let curl = fake_nix_tool(tmp.path(), "curl");

    let spec = RootfsSpec {
        service_id: "test-env".into(),
        store_path: store,
        bin_name: "app".into(),
        base_dir: tmp.path().to_path_buf(),
        bash_store: Some(bash),
        curl_store: Some(curl),
        debug: true,
    };

    let prepared = prepare_rootfs(&spec).await.unwrap();
    let env_path = prepared.rootfs_path.join("usr/bin/env");
    assert!(env_path.is_file(), "usr/bin/env should exist");
    let mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "env should be executable");
    let contents = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        contents.contains("#!/bin/bash"),
        "env should delegate to bash"
    );

    // /bin/env symlink should also exist
    let bin_env = prepared.rootfs_path.join("bin/env");
    assert!(
        bin_env.is_symlink() || bin_env.is_file(),
        "bin/env should exist"
    );
}

#[test]
fn validate_entrypoint_allows_env_bash_shebang() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("env-app-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("runner"), "#!/usr/bin/env bash\necho hi\n").unwrap();
    validate_entrypoint(&store, "runner", true).unwrap();
}

#[test]
fn validate_entrypoint_rejects_empty_env_shebang() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("env-empty-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("runner"), "#!/usr/bin/env\necho hi\n").unwrap();
    let err = validate_entrypoint(&store, "runner", true).unwrap_err();
    assert!(err.to_string().contains("program name"), "{err}");
}

#[test]
fn validate_entrypoint_rejects_env_with_path_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("env-bad-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(
        bin_dir.join("runner"),
        "#!/usr/bin/env ../bin/bash\necho hi\n",
    )
    .unwrap();
    let err = validate_entrypoint(&store, "runner", true).unwrap_err();
    assert!(err.to_string().contains("path"), "{err}");
}

#[test]
fn validate_entrypoint_rejects_env_without_debug() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("env-no-debug-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("runner"), "#!/usr/bin/env bash\necho hi\n").unwrap();
    let err = validate_entrypoint(&store, "runner", false).unwrap_err();
    assert!(err.to_string().contains("debug mode is disabled"), "{err}");
}

#[test]
fn validate_entrypoint_rejects_bin_env_without_debug() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("bin-env-no-debug-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("runner"), "#!/bin/env bash\necho hi\n").unwrap();
    let err = validate_entrypoint(&store, "runner", false).unwrap_err();
    assert!(err.to_string().contains("debug mode is disabled"), "{err}");
}

#[test]
fn validate_entrypoint_rejects_non_nix_store_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("bad-interp-store");
    let bin_dir = store.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("runner"), "#!/usr/bin/python3\necho hi\n").unwrap();
    let err = validate_entrypoint(&store, "runner", false).unwrap_err();
    assert!(err.to_string().contains("not under /nix/store/"), "{err}");
}

#[test]
fn reject_port_bare_via_e_flag() {
    let err = validate_podman_passthrough_args(&["-e".into(), "PORT".into()]).unwrap_err();
    assert!(
        err.to_string().contains("PORT"),
        "expected PORT rejection for -e PORT"
    );
}

#[test]
fn reject_port_compact_eport_form() {
    let err = validate_podman_passthrough_args(&["-ePORT".into()]).unwrap_err();
    assert!(
        err.to_string().contains("PORT"),
        "expected PORT rejection for -ePORT"
    );
}

#[test]
fn reject_port_publish_compact() {
    let err = validate_podman_passthrough_args(&["-p8080:80".into()]).unwrap_err();
    assert!(
        err.to_string().contains("publish") || err.to_string().contains("port"),
        "expected rejection for -p8080:80"
    );
}

#[test]
fn validate_podman_args_for_runtime_rejects_microvm() {
    let err = validate_podman_args_for_runtime(
        russel_core::config::RuntimeKind::Microvm,
        &["-v".into(), "/a:/b".into()],
    )
    .unwrap_err();
    assert!(err.to_string().contains("microvm"));
}

#[test]
fn validate_podman_args_for_runtime_validates_reserved_on_container() {
    let err = validate_podman_args_for_runtime(
        russel_core::config::RuntimeKind::Container,
        &["--rootfs".into(), "/tmp".into()],
    )
    .unwrap_err();
    assert!(err.to_string().contains("--rootfs"));
}

#[test]
fn build_run_args_inserts_passthrough_before_entrypoint() {
    let spec = ContainerStartSpec {
        service_id: "api-1".into(),
        rootfs: PreparedRootfs {
            rootfs_path: PathBuf::from("/var/lib/russel/api-1/rootfs"),
            entrypoint: PathBuf::from("/bin/api"),
        },
        host_port: 8080,
        guest_port: 3000,
        memory_mb: 512,
        env: vec![("PORT".into(), "3000".into())],
        extra_args: vec![
            "-v".into(),
            "/nix/store/abc123:/dest:ro".into(),
            "--network".into(),
            "bridge".into(),
        ],
    };
    let log_path = PathBuf::from("/var/lib/russel/api-1/container.log");
    let args = build_run_args(&spec, &log_path).unwrap();
    let entry_idx = args.iter().position(|a| a == "/bin/api").unwrap();
    let vol_idx = args.iter().position(|a| a == "-v").unwrap();
    let network_idx = args.iter().position(|a| a == "--network").unwrap();
    assert!(vol_idx < entry_idx);
    assert!(network_idx < entry_idx);
    assert_eq!(args.last().unwrap(), "/bin/api");
}

#[test]
fn container_log_path_is_under_service_base_dir() {
    assert_eq!(
        container_log_path("demo"),
        PathBuf::from("/var/lib/russel/demo/container.log")
    );
}

#[tokio::test]
#[ignore = "requires rootless podman"]
async fn e2e_podman_container_lifecycle() {
    let runner = ContainerRunner::new();
    ContainerRunner::ensure_rootless()
        .await
        .expect("rootless podman required");

    let tmp = tempfile::tempdir().unwrap();
    let store = fake_store(tmp.path(), "fake-e2e-app", "app", b"\x7fELF");
    let bash = fake_nix_tool(tmp.path(), "bash");
    let curl = fake_nix_tool(tmp.path(), "curl");
    let service_id = "e2e-podman-test";

    let _ = runner.destroy(service_id).await;

    let rootfs_spec = RootfsSpec {
        service_id: service_id.into(),
        store_path: store,
        bin_name: "app".into(),
        base_dir: tmp.path().to_path_buf(),
        bash_store: Some(bash),
        curl_store: Some(curl),
        debug: false,
    };
    let prepared = runner.prepare(&rootfs_spec).await.unwrap();

    let start_spec = ContainerStartSpec {
        service_id: service_id.into(),
        rootfs: prepared,
        host_port: 18080,
        guest_port: 8080,
        memory_mb: 128,
        env: vec![("PORT".into(), "8080".into())],
        extra_args: vec![],
    };

    let running = runner.start(&start_spec).await.unwrap();
    assert_eq!(running.container_name, "russel-e2e-podman-test");
    assert!(!running.container_id.is_empty());

    let inspected = runner.inspect(service_id).await.unwrap();
    assert!(inspected.is_some());

    runner.stop(service_id).await.unwrap();
    runner.destroy(service_id).await.unwrap();
    assert!(runner.inspect(service_id).await.unwrap().is_none());
}

// ── RUSSEL_PODMAN_USER tests (Issue #278598) ────────────────────────
// Serialize env mutations: cargo runs tests in parallel by default.

static PODMAN_USER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_russel_podman_user_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = PODMAN_USER_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var("RUSSEL_PODMAN_USER").ok();
    // SAFETY: exclusive lock held for the duration of the mutation + assertion.
    unsafe {
        match value {
            Some(v) => std::env::set_var("RUSSEL_PODMAN_USER", v),
            None => std::env::remove_var("RUSSEL_PODMAN_USER"),
        }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    unsafe {
        match previous {
            Some(v) => std::env::set_var("RUSSEL_PODMAN_USER", v),
            None => std::env::remove_var("RUSSEL_PODMAN_USER"),
        }
    }
    match result {
        Ok(v) => v,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn configured_podman_user_empty_when_not_set() {
    with_russel_podman_user_env(None, || {
        assert_eq!(configured_podman_user(), None);
    });
}

#[test]
fn configured_podman_user_filters_empty() {
    with_russel_podman_user_env(Some(""), || {
        assert_eq!(configured_podman_user(), None);
    });
}

#[test]
fn configured_podman_user_filters_root() {
    with_russel_podman_user_env(Some("root"), || {
        assert_eq!(configured_podman_user(), None);
    });
}

#[test]
fn configured_podman_user_accepts_valid() {
    with_russel_podman_user_env(Some("myuser"), || {
        assert_eq!(configured_podman_user(), Some("myuser".to_string()));
    });
}

#[test]
fn configured_podman_user_trims_whitespace() {
    with_russel_podman_user_env(Some("  myuser  "), || {
        assert_eq!(configured_podman_user(), Some("myuser".to_string()));
    });
}

#[test]
fn resolve_podman_user_explicit_wins() {
    assert_eq!(
        resolve_podman_user(Some("alice"), Some("bob"), 0),
        Some("alice".to_string())
    );
    // Even when not root, explicit wins.
    assert_eq!(
        resolve_podman_user(Some("alice"), Some("bob"), 1000),
        Some("alice".to_string())
    );
}

#[test]
fn resolve_podman_user_sudo_fallback_when_root() {
    assert_eq!(
        resolve_podman_user(None, Some("bob"), 0),
        Some("bob".to_string())
    );
}

#[test]
fn resolve_podman_user_no_sudo_when_not_root() {
    assert_eq!(resolve_podman_user(None, Some("bob"), 1000), None);
}

#[test]
fn resolve_podman_user_rejects_root_and_empty() {
    assert_eq!(resolve_podman_user(Some("root"), None, 0), None);
    assert_eq!(resolve_podman_user(Some(""), None, 0), None);
    assert_eq!(resolve_podman_user(Some("  "), None, 0), None);
}

// ── container_name trust / resolve (Issue #193) ─────────────────────

#[test]
fn trusted_container_name_accepts_canonical() {
    assert!(is_trusted_container_name("api", "russel-api"));
    assert!(is_trusted_container_name("my-service", "russel-my-service"));
    assert!(is_trusted_container_name(
        "api_gdeadbeef",
        "russel-api_gdeadbeef"
    ));
}

#[test]
fn trusted_container_name_accepts_generation_scoped() {
    // After dual-live promote, metadata may keep the runtime-key name.
    assert!(is_trusted_container_name("api", "russel-api_gdeadbeef"));
    assert!(is_trusted_container_name("api", "russel-api_gABCDEF12"));
    assert!(is_trusted_container_name(
        "my-svc",
        "russel-my-svc_g0123456789abcdef"
    ));
}

#[test]
fn trusted_container_name_rejects_arbitrary_and_malformed() {
    assert!(!is_trusted_container_name("api", ""));
    assert!(!is_trusted_container_name("api", "russel-other"));
    assert!(!is_trusted_container_name("api", "other-api"));
    assert!(!is_trusted_container_name("api", "russel-api_evil"));
    assert!(!is_trusted_container_name("api", "russel-api_g")); // empty gen
    assert!(!is_trusted_container_name("api", "russel-api_gnotahex!"));
    assert!(!is_trusted_container_name("api", "russel-api/../evil"));
    assert!(!is_trusted_container_name("api", "russel-api;rm -rf /"));
    // Wrong service prefix under russel-
    assert!(!is_trusted_container_name("api", "russel-apix_gdeadbeef"));
    // Gen too long
    assert!(!is_trusted_container_name(
        "api",
        &format!("russel-api_g{}", "a".repeat(33))
    ));
}

#[test]
fn resolve_container_name_falls_back_when_metadata_missing() {
    // No metadata under a non-existent service path → canonical.
    assert_eq!(
        resolve_container_name("no-such-service-193-unit"),
        "russel-no-such-service-193-unit"
    );
}
