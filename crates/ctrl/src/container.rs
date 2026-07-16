use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::process::Command;

/// Default service state directory: `/var/lib/russel/<service_id>`.
pub fn default_base_dir(service_id: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/russel/{service_id}"))
}

#[derive(Debug, Clone)]
pub struct RootfsSpec {
    pub service_id: String,
    /// Nix store path of the application package.
    pub store_path: PathBuf,
    pub bin_name: String,
    /// Parent of the `rootfs/` tree (e.g. `/var/lib/russel/<service_id>`).
    pub base_dir: PathBuf,
    /// Test injection: skip `nix build` for bash when set.
    pub bash_store: Option<PathBuf>,
    /// Test injection: skip `nix build` for curl when set.
    pub curl_store: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRootfs {
    pub rootfs_path: PathBuf,
    /// Absolute path as seen inside the container (e.g. `/bin/<bin_name>`).
    pub entrypoint: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct DebugToolsCache {
    bash_cache: Arc<Mutex<Option<PathBuf>>>,
    curl_cache: Arc<Mutex<Option<PathBuf>>>,
}

impl DebugToolsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve or build bash and curl from nixpkgs; results are cached in-memory.
    pub async fn ensure_debug_tools(&self) -> anyhow::Result<(PathBuf, PathBuf)> {
        let bash = self.ensure_bash(None).await?;
        let curl = self.ensure_curl(None).await?;
        Ok((bash, curl))
    }

    async fn ensure_bash(&self, override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
        if let Some(path) = override_path {
            return Ok(path.to_path_buf());
        }
        self.ensure_nix_package(&self.bash_cache, "bash", "bash").await
    }

    async fn ensure_curl(&self, override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
        if let Some(path) = override_path {
            return Ok(path.to_path_buf());
        }
        self.ensure_nix_package(&self.curl_cache, "curl", "curl").await
    }

    async fn ensure_nix_package(
        &self,
        cache: &Mutex<Option<PathBuf>>,
        attr: &str,
        label: &str,
    ) -> anyhow::Result<PathBuf> {
        if let Some(path) = Self::check_cache(cache) {
            return Ok(path);
        }

        let system = crate::build::current_system().await;
        tracing::info!(system = %system, package = %attr, "building {label} from nixpkgs (cached after first run)");
        let output = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "-f",
                "<nixpkgs>",
                "--argstr",
                "system",
                &system,
                attr,
            ])
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("failed to build {label} from nixpkgs");
        }

        let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let path = PathBuf::from(&store_path);

        if let Ok(mut cache) = cache.lock() {
            *cache = Some(path.clone());
        } else {
            tracing::warn!("{label} cache lock poisoned, skipping cache update");
        }
        tracing::info!(package = %label, store_path = %path.display(), "{label} cached");
        Ok(path)
    }

    fn check_cache(cache: &Mutex<Option<PathBuf>>) -> Option<PathBuf> {
        if let Ok(cache) = cache.lock() {
            if let Some(ref path) = *cache
                && path.exists()
            {
                return Some(path.clone());
            }
        } else {
            tracing::warn!("cache lock poisoned, re-building from scratch");
        }
        None
    }
}

static DEBUG_TOOLS: std::sync::OnceLock<DebugToolsCache> = std::sync::OnceLock::new();

fn shared_debug_tools() -> &'static DebugToolsCache {
    DEBUG_TOOLS.get_or_init(DebugToolsCache::new)
}

/// Prepare a Docker-like root filesystem for future `podman --rootfs` deploys.
///
/// The tree is written under `spec.base_dir/rootfs/`. Symlinks point at
/// `/nix/store/...` paths; at runtime the host store should be bind-mounted
/// read-only at `/nix/store` inside the container (Podman `--mount
/// type=bind,source=/nix/store,target=/nix/store,readonly`) so closure paths
/// resolve without copying store objects into the rootfs.
pub async fn prepare_rootfs(spec: &RootfsSpec) -> anyhow::Result<PreparedRootfs> {
    let entrypoint = validate_entrypoint(&spec.store_path, &spec.bin_name)?;
    let cache = shared_debug_tools();
    let bash_store = cache
        .ensure_bash(spec.bash_store.as_deref())
        .await?;
    let curl_store = cache
        .ensure_curl(spec.curl_store.as_deref())
        .await?;

    let rootfs_path = spec.base_dir.join("rootfs");
    if rootfs_path.exists() {
        tokio::fs::remove_dir_all(&rootfs_path).await?;
    }
    create_layout(&rootfs_path)?;
    write_etc_files(&rootfs_path)?;
    link_store_binary(&rootfs_path, &entrypoint, &spec.bin_name)?;
    link_debug_tool(&rootfs_path, &bash_store, "bash")?;
    link_debug_tool(&rootfs_path, &curl_store, "curl")?;

    let container_entrypoint = PathBuf::from(format!("/bin/{}", spec.bin_name));
    tracing::info!(
        service_id = %spec.service_id,
        rootfs = %rootfs_path.display(),
        entrypoint = %container_entrypoint.display(),
        "container rootfs prepared"
    );

    Ok(PreparedRootfs {
        rootfs_path,
        entrypoint: container_entrypoint,
    })
}

/// Require `store_path/bin/<bin_name>` and validate script shebangs when present.
pub fn validate_entrypoint(store_path: &Path, bin_name: &str) -> anyhow::Result<PathBuf> {
    let bin_path = store_path.join("bin").join(bin_name);
    if !bin_path.exists() {
        anyhow::bail!(
            "entrypoint missing: {} (expected {}/bin/{})",
            bin_path.display(),
            store_path.display(),
            bin_name
        );
    }

    let meta = std::fs::symlink_metadata(&bin_path)?;
    if meta.is_symlink() {
        let target = std::fs::read_link(&bin_path)?;
        validate_interpreter_path(store_path, &target, "symlink target")?;
        return Ok(bin_path);
    }

    if meta.is_file() {
        validate_shebang(store_path, &bin_path)?;
    }

    Ok(bin_path)
}

fn validate_shebang(store_path: &Path, bin_path: &Path) -> anyhow::Result<()> {
    let contents = std::fs::read(bin_path)?;
    let prefix = b"#!";
    if contents.len() < prefix.len() || &contents[..prefix.len()] != prefix {
        return Ok(());
    }

    let line_end = contents
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(contents.len());
    let shebang = std::str::from_utf8(&contents[prefix.len()..line_end])
        .map_err(|_| anyhow::anyhow!("invalid UTF-8 in shebang for {}", bin_path.display()))?
        .trim();
    let interpreter = shebang
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty shebang in {}", bin_path.display()))?;

    if interpreter == "/usr/bin/env" {
        return Ok(());
    }

    validate_interpreter_path(store_path, Path::new(interpreter), "shebang interpreter")
}

fn validate_interpreter_path(
    store_path: &Path,
    interpreter: &Path,
    context: &str,
) -> anyhow::Result<()> {
    if interpreter.is_absolute() {
        if interpreter.exists() {
            return Ok(());
        }
        if interpreter.starts_with("/nix/store/") {
            anyhow::bail!(
                "{context} does not exist on host: {} (required for bind-mounted /nix/store)",
                interpreter.display()
            );
        }
        if interpreter.starts_with(store_path) {
            return Ok(());
        }
        anyhow::bail!(
            "{context} does not exist: {}",
            interpreter.display()
        );
    }

    let resolved = store_path.join(interpreter);
    if resolved.exists() {
        return Ok(());
    }
    anyhow::bail!(
        "{context} not found under store path: {} (resolved to {})",
        interpreter.display(),
        resolved.display()
    )
}

fn create_layout(rootfs: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dirs = [
        "tmp",
        "var",
        "var/tmp",
        "etc",
        "bin",
        "usr",
        "usr/bin",
        "dev",
        "proc",
        "sys",
        "run",
        "home",
        "root",
    ];

    for dir in dirs {
        std::fs::create_dir_all(rootfs.join(dir))?;
    }

    std::fs::set_permissions(
        rootfs.join("tmp"),
        std::fs::Permissions::from_mode(0o1777),
    )?;
    std::fs::set_permissions(
        rootfs.join("root"),
        std::fs::Permissions::from_mode(0o700),
    )?;

    Ok(())
}

fn write_etc_files(rootfs: &Path) -> anyhow::Result<()> {
    let etc = rootfs.join("etc");
    std::fs::write(
        etc.join("passwd"),
        "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
    )?;
    std::fs::write(
        etc.join("group"),
        "root:x:0:\nnogroup:x:65534:\n",
    )?;
    std::fs::write(
        etc.join("hosts"),
        "127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n",
    )?;

    let resolv = etc.join("resolv.conf");
    if Path::new("/etc/resolv.conf").exists() {
        std::fs::copy("/etc/resolv.conf", &resolv)?;
    } else {
        std::fs::write(&resolv, "nameserver 8.8.8.8\nnameserver 8.8.4.4\n")?;
    }

    Ok(())
}

fn link_store_binary(rootfs: &Path, store_bin: &Path, bin_name: &str) -> anyhow::Result<()> {
    let target = store_bin_to_container_path(store_bin)?;
    symlink_in_bin_dirs(rootfs, bin_name, &target)
}

fn link_debug_tool(rootfs: &Path, store_path: &Path, tool: &str) -> anyhow::Result<()> {
    let host_bin = store_path.join("bin").join(tool);
    if !host_bin.exists() {
        anyhow::bail!(
            "debug tool binary missing: {} (store path {})",
            host_bin.display(),
            store_path.display()
        );
    }
    let target = store_bin_to_container_path(&host_bin)?;
    symlink_in_bin_dirs(rootfs, tool, &target)
}

fn store_bin_to_container_path(host_path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = host_path.canonicalize().unwrap_or_else(|_| host_path.to_path_buf());
    let path_str = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 store path: {}", canonical.display()))?;
    let Some(idx) = path_str.find("/nix/store/") else {
        anyhow::bail!("expected path containing /nix/store/, got {path_str}");
    };
    Ok(PathBuf::from(&path_str[idx..]))
}

fn symlink_in_bin_dirs(rootfs: &Path, name: &str, target: &Path) -> anyhow::Result<()> {
    for dir in ["bin", "usr/bin"] {
        let link = rootfs.join(dir).join(name);
        if link.exists() {
            std::fs::remove_file(&link)?;
        }
        std::os::unix::fs::symlink(target, &link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
        };

        let prepared = prepare_rootfs(&spec).await.unwrap();
        let rootfs = &prepared.rootfs_path;

        for dir in [
            "tmp", "var", "var/tmp", "etc", "bin", "usr", "usr/bin",
            "dev", "proc", "sys", "run", "home", "root",
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
        };

        let prepared = prepare_rootfs(&spec).await.unwrap();
        let link = prepared.rootfs_path.join("bin/myapp");
        assert!(link.is_symlink());
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, PathBuf::from("/nix/store/fake-myapp-package/bin/myapp"));
    }

    #[test]
    fn validate_entrypoint_fails_when_bin_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("empty-store");
        std::fs::create_dir_all(store.join("bin")).unwrap();

        let err = validate_entrypoint(&store, "missing").unwrap_err();
        assert!(err.to_string().contains("entrypoint missing"));
    }

    #[test]
    fn validate_entrypoint_succeeds_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let store = fake_store(tmp.path(), "fake-app-package", "app", b"\x7fELF");

        let path = validate_entrypoint(&store, "app").unwrap();
        assert_eq!(path, store.join("bin/app"));
    }

    #[test]
    fn validate_entrypoint_checks_shebang_interpreter() {
        let tmp = tempfile::tempdir().unwrap();
        let interpreter = fake_nix_tool(tmp.path(), "bash");
        let store = tmp.path().join("app-store");
        let bin_dir = store.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = format!(
            "#!{}/bin/bash\necho hi\n",
            interpreter.display()
        );
        let bin_path = bin_dir.join("runner");
        std::fs::write(&bin_path, script).unwrap();

        validate_entrypoint(&store, "runner").unwrap();
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

        let err = validate_entrypoint(&store, "runner").unwrap_err();
        assert!(err.to_string().contains("shebang interpreter"));
    }
}