//! Unit tests for deploy helpers (generation ids, config path containment, podman args).

use super::config::{MAX_CONFIG_BYTES, load_russelfile_under_repo, resolve_build_dir};
use super::pipeline::{DesiredExtras, build_desired_state, new_generation_id};
use crate::metadata::build_microvm_metadata;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn generation_id_is_short_hex() {
    let id = new_generation_id();
    assert_eq!(id.len(), 8);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn generation_runtime_key_is_valid_service_id() {
    let id = new_generation_id();
    let key = format!("api_g{id}");
    russel_core::ids::validate_service_id(&key).unwrap();
}

#[test]
fn metadata_records_generation_and_tap() {
    let meta = build_microvm_metadata(
        "api_gdeadbeef",
        3100,
        3000,
        "10.0.1.2",
        "10.0.1.1",
        Some(1),
        &[],
        None,
        "/k",
        "/s",
        512,
        1,
        None,
        None,
        None,
        Some("deadbeef"),
        Some("rsl-abcd1234"),
    );
    assert_eq!(meta["generation_id"], "deadbeef");
    assert_eq!(meta["tap_id"], "rsl-abcd1234");
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "russel-config-path-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_config(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

const MINIMAL: &str = r#"[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;

#[test]
fn load_russelfile_accepts_relative_file() {
    let repo = TempRepo::new();
    write_config(repo.path(), "Russelfile.toml", MINIMAL);
    let cfg = load_russelfile_under_repo(repo.path(), "Russelfile.toml").unwrap();
    assert_eq!(cfg.service.name, "app");
    assert_eq!(cfg.service.port, 3000);
}

#[test]
fn load_russelfile_accepts_nested_relative() {
    let repo = TempRepo::new();
    write_config(repo.path(), "deploy/Russelfile.toml", MINIMAL);
    let cfg = load_russelfile_under_repo(repo.path(), "deploy/Russelfile.toml").unwrap();
    assert_eq!(cfg.service.name, "app");
}

#[test]
fn build_dir_is_the_repo_root_for_a_root_russelfile() {
    let repo = TempRepo::new();
    assert_eq!(
        resolve_build_dir(repo.path(), "Russelfile.toml", ".").unwrap(),
        repo.path()
    );
}

#[test]
fn build_dir_follows_the_russelfile_folder_and_source() {
    let repo = TempRepo::new();
    std::fs::create_dir_all(repo.path().join("examples/basic-http/src")).unwrap();
    // `--config examples/basic-http/Russelfile.toml` builds that folder (#525).
    assert_eq!(
        resolve_build_dir(repo.path(), "examples/basic-http/Russelfile.toml", ".").unwrap(),
        repo.path().join("examples/basic-http")
    );
    assert_eq!(
        resolve_build_dir(repo.path(), "examples/basic-http/Russelfile.toml", "./src").unwrap(),
        repo.path().join("examples/basic-http/src")
    );
    std::fs::create_dir_all(repo.path().join("apps/api")).unwrap();
    assert_eq!(
        resolve_build_dir(repo.path(), "Russelfile.toml", "apps/api").unwrap(),
        repo.path().join("apps/api")
    );
}

#[test]
fn build_dir_must_exist() {
    let repo = TempRepo::new();
    let err = resolve_build_dir(repo.path(), "Russelfile.toml", "missing")
        .unwrap_err()
        .to_string();
    assert!(err.contains("service.source 'missing'"), "{err}");
}

#[cfg(unix)]
#[test]
fn build_dir_rejects_a_symlink_out_of_the_repo() {
    let repo = TempRepo::new();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join("app")).unwrap();
    let err = resolve_build_dir(repo.path(), "Russelfile.toml", "app")
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a directory in the repository"), "{err}");
}

#[test]
fn load_russelfile_rejects_absolute() {
    let repo = TempRepo::new();
    let err = load_russelfile_under_repo(repo.path(), "/etc/passwd")
        .unwrap_err()
        .to_string();
    assert!(err.contains("relative"), "{err}");
}

#[test]
fn load_russelfile_rejects_parent_dir() {
    let repo = TempRepo::new();
    let err = load_russelfile_under_repo(repo.path(), "../outside.toml")
        .unwrap_err()
        .to_string();
    assert!(err.contains(".."), "{err}");
}

#[test]
fn load_russelfile_rejects_missing() {
    let repo = TempRepo::new();
    let err = load_russelfile_under_repo(repo.path(), "missing.toml")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not found") || err.contains("cannot open") || err.contains("No such file"),
        "{err}"
    );
}

#[test]
fn load_russelfile_rejects_directory() {
    let repo = TempRepo::new();
    std::fs::create_dir(repo.path().join("subdir")).unwrap();
    let err = load_russelfile_under_repo(repo.path(), "subdir")
        .unwrap_err()
        .to_string();
    // Directory has no file name open as file → open or "regular file" error
    assert!(
        err.contains("regular file")
            || err.contains("cannot open")
            || err.contains("Is a directory"),
        "{err}"
    );
}

#[cfg(target_family = "unix")]
#[test]
fn load_russelfile_rejects_symlink_leaf() {
    let repo = TempRepo::new();
    let outside = std::env::temp_dir().join(format!(
        "russel-config-path-outside-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("secret.toml");
    std::fs::write(
        &outside_file,
        b"[service]\nname=\"x\"\nsource=\".\"\nport=1\nmemory=\"1mb\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside_file, repo.path().join("escape.toml")).unwrap();
    // O_NOFOLLOW rejects the leaf symlink (does not follow out of the repo).
    let err = load_russelfile_under_repo(repo.path(), "escape.toml")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot open")
            || err.contains("symbolic link")
            || err.contains("Too many levels")
            || err.contains("os error"),
        "{err}"
    );
    std::fs::remove_dir_all(&outside).ok();
}

#[cfg(target_family = "unix")]
#[test]
fn load_russelfile_rejects_symlink_intermediate_dir() {
    let repo = TempRepo::new();
    let outside = std::env::temp_dir().join(format!(
        "russel-config-path-outside-dir-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.toml"), MINIMAL.as_bytes()).unwrap();
    // Intermediate directory component is a symlink → openat O_NOFOLLOW must fail.
    std::os::unix::fs::symlink(&outside, repo.path().join("linkdir")).unwrap();
    let err = load_russelfile_under_repo(repo.path(), "linkdir/secret.toml")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not found")
            || err.contains("cannot open")
            || err.contains("symbolic link")
            || err.contains("Too many levels")
            || err.contains("os error"),
        "{err}"
    );
    std::fs::remove_dir_all(&outside).ok();
}

#[test]
fn load_russelfile_rejects_oversized() {
    let repo = TempRepo::new();
    let path = repo.path().join("huge.toml");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"[service]\n").unwrap();
        let pad = vec![b'#'; MAX_CONFIG_BYTES as usize + 1];
        f.write_all(&pad).unwrap();
    }
    let err = load_russelfile_under_repo(repo.path(), "huge.toml")
        .unwrap_err()
        .to_string();
    assert!(err.contains("exceeds maximum size"), "{err}");
}

use crate::container::validate_podman_args_for_runtime;
use russel_core::config::{GuestKind, RuntimeKind};

fn desired_state_for_toml(body: &str) -> serde_json::Value {
    let repo = TempRepo::new();
    write_config(repo.path(), "Russelfile.toml", body);
    let cfg = load_russelfile_under_repo(repo.path(), "Russelfile.toml").unwrap();
    build_desired_state(
        "https://example.com/app.git",
        "Russelfile.toml",
        RuntimeKind::Microvm,
        cfg.service.guest,
        &std::collections::HashMap::new(),
        &[],
        None,
        None,
        DesiredExtras::default(),
    )
}

#[test]
fn desired_state_records_omitted_guest_as_busybox() {
    let ds = desired_state_for_toml(MINIMAL);
    assert_eq!(ds["runtime"], "microvm");
    assert_eq!(ds["guest"], "busybox");
}

#[test]
fn desired_state_records_explicit_busybox() {
    let toml = r#"[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
guest = "busybox"
"#;
    let ds = desired_state_for_toml(toml);
    assert_eq!(ds["guest"], "busybox");
}

#[test]
fn podman_args_rejected_for_microvm_runtime() {
    let err =
        validate_podman_args_for_runtime(RuntimeKind::Microvm, &["-v".into(), "/a:/b".into()])
            .unwrap_err();
    assert!(err.to_string().contains("microvm"));
}

#[test]
fn podman_args_allowed_for_container_runtime() {
    validate_podman_args_for_runtime(
        RuntimeKind::Container,
        &["--network".into(), "bridge".into()],
    )
    .unwrap();
}

#[test]
fn persist_repo_url_drops_http_userinfo() {
    let ds = build_desired_state(
        "https://user:token@github.com/org/app.git",
        "Russelfile.toml",
        RuntimeKind::Microvm,
        GuestKind::Busybox,
        &std::collections::HashMap::new(),
        &[],
        None,
        None,
        DesiredExtras::default(),
    );
    assert_eq!(ds["repo_url"], "https://github.com/org/app.git");
    assert_eq!(ds["guest"], "busybox");
    let plain = build_desired_state(
        "https://github.com/org/app.git",
        "Russelfile.toml",
        RuntimeKind::Microvm,
        GuestKind::Busybox,
        &std::collections::HashMap::new(),
        &[],
        None,
        None,
        DesiredExtras::default(),
    );
    assert_eq!(plain["repo_url"], "https://github.com/org/app.git");
}

#[test]
fn desired_state_records_file_only_ingress_pin_and_host() {
    let pin = russel_core::api::PortMapping {
        host: 4000,
        guest: 3000,
    };
    let ds = build_desired_state(
        "https://example.com/app.git",
        "Russelfile.toml",
        RuntimeKind::Container,
        GuestKind::Busybox,
        &std::collections::HashMap::new(),
        &[],
        Some(&pin),
        Some("abc.com"),
        DesiredExtras::default(),
    );
    assert_eq!(ds["port"]["host"], 4000);
    assert_eq!(ds["port"]["guest"], 3000);
    assert_eq!(ds["ingress_host"], "abc.com");
}

/// `[ingress].port` in the Russelfile becomes the desired_state pin that
/// update, rollback, and health restart replay.
#[test]
fn desired_state_records_the_russelfile_ingress_pin() {
    let repo = TempRepo::new();
    let toml = format!("{MINIMAL}\n[ingress]\nport = 8081\n");
    write_config(repo.path(), "Russelfile.toml", &toml);
    let cfg = load_russelfile_under_repo(repo.path(), "Russelfile.toml").unwrap();
    let pin = russel_core::config::resolve_primary_publish(&cfg).unwrap();
    let ds = build_desired_state(
        "https://example.com/app.git",
        "Russelfile.toml",
        RuntimeKind::Container,
        cfg.service.guest,
        &std::collections::HashMap::new(),
        &[],
        Some(&pin),
        None,
        DesiredExtras::default(),
    );
    assert_eq!(ds["port"]["host"], 8081);
    assert_eq!(ds["port"]["guest"], 3000);
}

#[test]
fn render_argv_one_entry_per_line() {
    let args = vec!["--listen".to_string(), ":8080".to_string(), String::new()];
    assert_eq!(super::render_argv(&args).unwrap(), "--listen\n:8080\n\n");
    assert_eq!(super::render_argv(&[]).unwrap(), "");
    assert!(super::render_argv(&["a\nb".to_string()]).is_err());
}

#[test]
fn write_deploy_env_writes_argv_and_user_and_clears_them() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let cfg = cfg.to_str().unwrap();
    let env = std::collections::HashMap::new();
    let args = vec!["file-server".to_string(), "--listen".to_string()];
    super::write_deploy_env(
        cfg,
        "10.0.0.2",
        "10.0.0.1",
        3000,
        "/nix/store/x/bin/a",
        &env,
        &args,
        &[],
        super::RunAs::App,
    )
    .unwrap();
    let argv = std::fs::read_to_string(format!("{cfg}/argv")).unwrap();
    assert_eq!(argv, "file-server\n--listen\n");
    let (uid, gid) = super::microvm_app_ids();
    assert_eq!(
        std::fs::read_to_string(format!("{cfg}/user")).unwrap(),
        format!("{uid} {gid}\n")
    );
    // A retirement's stop request must not reach the next boot (#562).
    std::fs::write(format!("{cfg}/{}", super::STOP_FILE), "").unwrap();
    super::write_deploy_env(
        cfg,
        "10.0.0.2",
        "10.0.0.1",
        3000,
        "/nix/store/x/bin/a",
        &env,
        &[],
        &[],
        super::RunAs::Root,
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(format!("{cfg}/argv")).unwrap(), "");
    assert!(!std::path::Path::new(&format!("{cfg}/user")).exists());
    assert!(!std::path::Path::new(&format!("{cfg}/{}", super::STOP_FILE)).exists());
}
