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

const CONTAINER_NAME_PREFIX: &str = "russel-";
const LABEL_SERVICE: &str = "russel.service";
const LABEL_RUNTIME: &str = "russel.runtime";
const RUNTIME_CONTAINER: &str = "container";
const NIX_STORE_MOUNT: &str =
    "type=bind,source=/nix/store,destination=/nix/store,ro=true";
const PODMAN_STOP_TIMEOUT_SECS: &str = "10";

/// Console output is written here when a container starts; also available via `podman logs`.
pub fn container_log_path(service_id: &str) -> PathBuf {
    default_base_dir(service_id).join("container.log")
}

#[derive(Debug, Default, Clone)]
pub struct ContainerRunner;

#[derive(Debug, Clone)]
pub struct ContainerStartSpec {
    pub service_id: String,
    pub rootfs: PreparedRootfs,
    pub host_port: u16,
    pub guest_port: u16,
    pub memory_mb: u16,
    pub env: Vec<(String, String)>,
    /// Validated extra `podman run` arguments (inserted before entrypoint).
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub service_id: String,
    pub container_name: String,
    pub container_id: String,
    pub rootfs_path: PathBuf,
}

impl ContainerRunner {
    pub fn new() -> Self {
        Self
    }

    pub fn container_name(service_id: &str) -> String {
        format!("{CONTAINER_NAME_PREFIX}{service_id}")
    }

    /// Fail if `podman info` does not indicate rootless.
    pub async fn ensure_rootless() -> anyhow::Result<()> {
        let output = Command::new("podman")
            .args(["info", "--format", "json"])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run podman info: {e}"))?;

        if !output.status.success() {
            anyhow::bail!(
                "podman info failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let info = String::from_utf8_lossy(&output.stdout);
        match parse_podman_rootless(&info) {
            Ok(true) => Ok(()),
            Ok(false) => anyhow::bail!(rootless_required_error("false")),
            Err(_) => anyhow::bail!(rootless_required_error("not found")),
        }
    }

    pub async fn prepare(&self, spec: &RootfsSpec) -> anyhow::Result<PreparedRootfs> {
        prepare_rootfs(spec).await
    }

    pub async fn start(&self, spec: &ContainerStartSpec) -> anyhow::Result<RunningContainer> {
        crate::microvm::MicrovmRunner::validate_service_id(&spec.service_id)?;
        Self::ensure_rootless().await?;

        let name = Self::container_name(&spec.service_id);
        stop_and_remove_container(&name).await?;

        if let Some(parent) = spec.rootfs.rootfs_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let log_path = container_log_path(&spec.service_id);
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let args = build_run_args(spec, &log_path)?;
        let output = run_podman(&args).await?;
        if !output.status.success() {
            anyhow::bail!(
                "podman run failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if container_id.is_empty() {
            anyhow::bail!("podman run returned empty container id");
        }

        tracing::info!(
            service_id = %spec.service_id,
            container_name = %name,
            container_id = %container_id,
            rootfs = %spec.rootfs.rootfs_path.display(),
            log_path = %log_path.display(),
            "container started (console also via podman logs {name})"
        );

        Ok(RunningContainer {
            service_id: spec.service_id.clone(),
            container_name: name,
            container_id,
            rootfs_path: spec.rootfs.rootfs_path.clone(),
        })
    }

    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = Self::container_name(service_id);
        stop_container(&name).await
    }

    /// Stop the container, remove it, and delete the prepared rootfs tree when present.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = Self::container_name(service_id);
        stop_container(&name).await?;
        remove_container(&name).await?;

        let rootfs_path = default_base_dir(service_id).join("rootfs");
        if rootfs_path.exists() {
            tokio::fs::remove_dir_all(&rootfs_path).await?;
        }
        Ok(())
    }

    pub async fn inspect(&self, service_id: &str) -> anyhow::Result<Option<RunningContainer>> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = Self::container_name(service_id);
        let output = Command::new("podman")
            .args([
                "inspect",
                &name,
                "--format",
                "{{.Id}} {{index .Config.Labels \"russel.service\"}} {{.Rootfs}}",
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run podman inspect: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no such object")
                || stderr.contains("No such container")
                || output.status.code() == Some(125)
            {
                return Ok(None);
            }
            anyhow::bail!("podman inspect failed: {}", stderr.trim());
        }

        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let mut parts = line.splitn(3, ' ');
        let container_id = parts
            .next()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("podman inspect returned empty container id"))?;
        let label_service = parts.next().unwrap_or_default();
        let rootfs_from_inspect = parts.next().unwrap_or_default();

        let rootfs_path = if rootfs_from_inspect.is_empty() {
            default_base_dir(service_id).join("rootfs")
        } else {
            PathBuf::from(rootfs_from_inspect)
        };

        let resolved_service_id = if label_service.is_empty() {
            service_id.to_string()
        } else {
            label_service.to_string()
        };

        Ok(Some(RunningContainer {
            service_id: resolved_service_id,
            container_name: name,
            container_id,
            rootfs_path,
        }))
    }
}

fn rootless_required_error(detail: &str) -> String {
    format!(
        "Russel containers require rootless Podman (podman info .host.security.rootless detected {detail})"
    )
}

/// Parse `podman info --format json` and read `.host.security.rootless`.
pub fn parse_podman_rootless(info_json: &str) -> anyhow::Result<bool> {
    let value: serde_json::Value = serde_json::from_str(info_json)?;
    value
        .pointer("/host/security/rootless")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("podman info missing .host.security.rootless"))
}

/// Reject podman passthrough args that Russel owns (name, detach, rootfs, etc.).
pub fn validate_podman_passthrough_args(args: &[String]) -> anyhow::Result<()> {
    for arg in args {
        if arg == "--rootfs" || arg.starts_with("--rootfs=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --rootfs");
        }
        if arg == "--name" || arg.starts_with("--name=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --name");
        }
        if arg == "--replace" || arg.starts_with("--replace=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --replace");
        }
        if arg == "-d" || arg == "--detach" || arg.starts_with("--detach=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: detach (-d/--detach)");
        }
        if arg == "-n" {
            anyhow::bail!("podman passthrough arg reserved by Russel: -n");
        }
    }
    Ok(())
}

/// Ensure podman passthrough args are only used with the container runtime.
pub fn validate_podman_args_for_runtime(
    runtime: russel_core::config::RuntimeKind,
    args: &[String],
) -> anyhow::Result<()> {
    if runtime == russel_core::config::RuntimeKind::Microvm && !args.is_empty() {
        anyhow::bail!(
            "podman passthrough args require container runtime (effective runtime is microvm)"
        );
    }
    if !args.is_empty() {
        validate_podman_passthrough_args(args)?;
    }
    Ok(())
}

/// Build `podman run` arguments for unit testing and runtime use.
pub fn build_run_args(spec: &ContainerStartSpec, log_path: &Path) -> anyhow::Result<Vec<String>> {
    crate::microvm::MicrovmRunner::validate_service_id(&spec.service_id)?;

    let name = ContainerRunner::container_name(&spec.service_id);
    let port_mapping = format!("{}:{}", spec.host_port, spec.guest_port);
    let log_path = log_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 log path: {}", log_path.display()))?;

    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name,
        "--label".to_string(),
        format!("{LABEL_SERVICE}={}", spec.service_id),
        "--label".to_string(),
        format!("{LABEL_RUNTIME}={RUNTIME_CONTAINER}"),
        "--rootfs".to_string(),
        spec.rootfs.rootfs_path.display().to_string(),
        "--mount".to_string(),
        NIX_STORE_MOUNT.to_string(),
        "-p".to_string(),
        port_mapping,
        "--memory".to_string(),
        format!("{}m", spec.memory_mb),
        "--workdir".to_string(),
        "/".to_string(),
        "--log-driver".to_string(),
        "k8s-file".to_string(),
        "--log-opt".to_string(),
        format!("path={log_path}"),
    ];

    for (key, value) in &spec.env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }

    if !spec.extra_args.is_empty() {
        validate_podman_passthrough_args(&spec.extra_args)?;
        args.extend(spec.extra_args.clone());
    }

    args.push(spec.rootfs.entrypoint.display().to_string());
    Ok(args)
}

async fn run_podman(args: &[String]) -> anyhow::Result<std::process::Output> {
    Command::new("podman")
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run podman: {e}"))
}

async fn stop_and_remove_container(name: &str) -> anyhow::Result<()> {
    stop_container(name).await?;
    remove_container(name).await
}

async fn stop_container(name: &str) -> anyhow::Result<()> {
    let output = Command::new("podman")
        .args(["stop", "-t", PODMAN_STOP_TIMEOUT_SECS, name])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run podman stop: {e}"))?;

    if output.status.success() || is_missing_container(&output) {
        return Ok(());
    }

    anyhow::bail!(
        "podman stop failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

async fn remove_container(name: &str) -> anyhow::Result<()> {
    let output = Command::new("podman")
        .args(["rm", "-f", name])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run podman rm: {e}"))?;

    if output.status.success() || is_missing_container(&output) {
        return Ok(());
    }

    anyhow::bail!(
        "podman rm failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn is_missing_container(output: &std::process::Output) -> bool {
    if output.status.code() == Some(125) {
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("no such object") || stderr.contains("No such container")
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
            env: vec![("PORT".into(), "3000".into()), ("RUSSEL".into(), "1".into())],
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
        assert!(args.contains(&"8080:3000".to_string()));
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
            "/data:/data:ro".into(),
            "--mount".into(),
            "type=bind,source=/tmp/x,destination=/data".into(),
            "--network".into(),
            "bridge".into(),
            "--device".into(),
            "/dev/fuse".into(),
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--env".into(),
            "FOO=bar".into(),
        ];
        validate_podman_passthrough_args(&args).unwrap();
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
                "/data:/data:ro".into(),
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
}