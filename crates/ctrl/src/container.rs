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
    #[allow(dead_code)] // optional debug helper for container shells; not on deploy path
    pub async fn ensure_debug_tools(&self) -> anyhow::Result<(PathBuf, PathBuf)> {
        let bash = self.ensure_bash(None).await?;
        let curl = self.ensure_curl(None).await?;
        Ok((bash, curl))
    }

    async fn ensure_bash(&self, override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
        if let Some(path) = override_path {
            return Ok(path.to_path_buf());
        }
        self.ensure_nix_package(&self.bash_cache, "bash", "bash")
            .await
    }

    async fn ensure_curl(&self, override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
        if let Some(path) = override_path {
            return Ok(path.to_path_buf());
        }
        self.ensure_nix_package(&self.curl_cache, "curl", "curl")
            .await
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
        // Build the package attr (not `.out`): multi-output packages differ —
        // bash puts the binary in `out`, curl in `bin`. `--print-out-paths` may
        // list several paths; `select_nix_tool_store_path` picks the one with
        // `bin/<tool>`.
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

        let path = select_nix_tool_store_path(&output.stdout, label)?;

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

/// Pick the store path that contains `bin/<tool>` from `nix build --print-out-paths`.
///
/// Multi-output packages (e.g. `bash`) can print several paths (man, doc, out).
/// Using the whole stdout as a single path breaks `link_debug_tool`.
fn select_nix_tool_store_path(stdout: &[u8], tool: &str) -> anyhow::Result<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    let candidates: Vec<PathBuf> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();

    if candidates.is_empty() {
        anyhow::bail!("nix build produced no store paths for {tool}");
    }

    for path in &candidates {
        let bin = path.join("bin").join(tool);
        if bin.is_file() || bin.is_symlink() {
            return Ok(path.clone());
        }
    }

    anyhow::bail!(
        "none of the nix build outputs for {tool} contain bin/{tool}: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
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
    let bash_store = cache.ensure_bash(spec.bash_store.as_deref()).await?;
    let curl_store = cache.ensure_curl(spec.curl_store.as_deref()).await?;

    let rootfs_path = spec.base_dir.join("rootfs");
    if rootfs_path.exists() {
        tokio::fs::remove_dir_all(&rootfs_path).await?;
    }
    create_layout(&rootfs_path)?;
    write_etc_files(&rootfs_path)?;
    link_store_binary(&rootfs_path, &entrypoint, &spec.bin_name)?;
    link_debug_tool(&rootfs_path, &bash_store, "bash")?;
    link_debug_tool(&rootfs_path, &curl_store, "curl")?;
    install_env_wrapper(&rootfs_path)?;

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

    if interpreter == "/usr/bin/env" || interpreter == "/bin/env" {
        // Rootfs installs a minimal `env` wrapper that is `exec "$@"`.
        // Only the staged form `#!/usr/bin/env <program>` is supported —
        // reject options (`env -S …`), extra args, and pathy program names.
        let parts: Vec<&str> = shebang.split_whitespace().collect();
        if parts.len() < 2 {
            anyhow::bail!(
                "shebang uses {interpreter} without a program name in {}",
                bin_path.display()
            );
        }
        if parts.len() != 2 {
            anyhow::bail!(
                "shebang env form must be exactly `#!{interpreter} <program>` \
                 (no options or extra arguments) in {}",
                bin_path.display()
            );
        }
        let prog = parts[1];
        if prog.is_empty() {
            anyhow::bail!(
                "shebang uses {interpreter} without a program name in {}",
                bin_path.display()
            );
        }
        if prog.starts_with('-') || prog.contains('/') || prog.contains("..") {
            anyhow::bail!(
                "shebang program name must be a simple basename without options, \
                 path separators, or traversal: {prog}"
            );
        }
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
        anyhow::bail!("{context} does not exist: {}", interpreter.display());
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
        "tmp", "var", "var/tmp", "etc", "bin", "usr", "usr/bin", "dev", "proc", "sys", "run",
        "home", "root",
    ];

    for dir in dirs {
        std::fs::create_dir_all(rootfs.join(dir))?;
    }

    std::fs::set_permissions(rootfs.join("tmp"), std::fs::Permissions::from_mode(0o1777))?;
    std::fs::set_permissions(rootfs.join("root"), std::fs::Permissions::from_mode(0o700))?;

    Ok(())
}

fn write_etc_files(rootfs: &Path) -> anyhow::Result<()> {
    let etc = rootfs.join("etc");
    std::fs::write(
        etc.join("passwd"),
        "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
    )?;
    std::fs::write(etc.join("group"), "root:x:0:\nnogroup:x:65534:\n")?;
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
    let canonical = host_path
        .canonicalize()
        .unwrap_or_else(|_| host_path.to_path_buf());
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

/// Install /usr/bin/env as a small executable that delegates to the rootfs bash
/// (Issue #4). Needed when entrypoint scripts use `#!/usr/bin/env bash` and
/// the host's /usr/bin/env is not available inside the container.
fn install_env_wrapper(rootfs: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let env_path = rootfs.join("usr/bin/env");
    std::fs::write(&env_path, "#!/bin/bash\nexec \"$@\"\n")?;
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o755))?;
    // Also symlink at /bin/env for `#!/bin/env` shebang compatibility.
    let bin_env = rootfs.join("bin/env");
    if !bin_env.exists() {
        std::os::unix::fs::symlink("../usr/bin/env", &bin_env)?;
    }
    Ok(())
}

const CONTAINER_NAME_PREFIX: &str = "russel-";
const LABEL_SERVICE: &str = "russel.service";
const LABEL_RUNTIME: &str = "russel.runtime";
const RUNTIME_CONTAINER: &str = "container";
const NIX_STORE_MOUNT: &str = "type=bind,source=/nix/store,destination=/nix/store,ro=true";
const PODMAN_STOP_TIMEOUT_SECS: &str = "10";

// ── RUSSEL_PODMAN_USER env support (Issue #278598) ───────────────────────────

fn configured_podman_user() -> Option<String> {
    std::env::var("RUSSEL_PODMAN_USER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "root")
}

struct PodmanUserEnv {
    home: String,
    xdg_runtime: String,
}

fn podman_user_env() -> Option<&'static PodmanUserEnv> {
    use std::sync::OnceLock;
    static ENV: OnceLock<Option<PodmanUserEnv>> = OnceLock::new();
    ENV.get_or_init(|| {
        let user = configured_podman_user()?;
        let uid = std::process::Command::new("id")
            .args(["-u", &user])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
        let home = std::process::Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.split(':')
                    .nth(5)
                    .unwrap_or(&format!("/home/{user}"))
                    .to_string()
            })?;
        Some(PodmanUserEnv {
            home,
            xdg_runtime: format!("/run/user/{uid}"),
        })
    })
    .as_ref()
}

/// Build a `Command` that runs `podman <args>` as the configured user when
/// `RUSSEL_PODMAN_USER` is set (sudo wrapper with rootless env vars).
fn podman_command() -> Command {
    if let Some(env) = podman_user_env() {
        let user = configured_podman_user().unwrap(); // safe: podman_user_env already checked
        let mut cmd = Command::new("sudo");
        cmd.args([
            "-u",
            &user,
            "-H",
            "env",
            &format!("HOME={}", env.home),
            &format!("XDG_RUNTIME_DIR={}", env.xdg_runtime),
            "podman",
        ]);
        cmd
    } else {
        Command::new("podman")
    }
}

/// Make the service dir + rootfs usable by a non-root podman user when
/// `RUSSEL_PODMAN_USER` is set (ctrl runs as root via sudo for microVMs).
///
/// Rootless podman needs:
/// - traverse every path component to the rootfs (`faccessat` fails on 0700
///   parents such as `mktemp -d /tmp/...` created by root)
/// - own/write the service dir for the k8s-file log under `/var/lib/russel/<id>/`
fn ensure_rootfs_readable_for_podman_user(rootfs: &Path) -> anyhow::Result<()> {
    let Some(user) = configured_podman_user() else {
        return Ok(());
    };

    // Open path components for traversal (resolve symlinks first).
    let resolved = rootfs
        .canonicalize()
        .unwrap_or_else(|_| rootfs.to_path_buf());
    let mut walk = resolved.as_path();
    loop {
        let output = std::process::Command::new("chmod")
            .args(["a+rx", &walk.display().to_string()])
            .output()?;
        if !output.status.success() {
            // Best-effort on parents we may not own (e.g. /); rootfs/base are critical.
            tracing::debug!(
                path = %walk.display(),
                err = %String::from_utf8_lossy(&output.stderr).trim(),
                "chmod a+rx parent skipped"
            );
        }
        match walk.parent() {
            Some(parent) if parent != walk => walk = parent,
            _ => break,
        }
    }

    // Hand the service dir to the podman user so rootless can write logs + mount.
    // Use numeric uid:gid — NixOS (and others) often have no group named like the user
    // (e.g. user crimxnhaze, group users), so `chown user:user` fails with "invalid group".
    let base = rootfs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| rootfs.to_path_buf());
    let uid = std::process::Command::new("id")
        .args(["-u", &user])
        .output()
        .map_err(|e| anyhow::anyhow!("id -u {user}: {e}"))?;
    if !uid.status.success() {
        anyhow::bail!(
            "id -u {user} failed: {}",
            String::from_utf8_lossy(&uid.stderr).trim()
        );
    }
    let gid = std::process::Command::new("id")
        .args(["-g", &user])
        .output()
        .map_err(|e| anyhow::anyhow!("id -g {user}: {e}"))?;
    if !gid.status.success() {
        anyhow::bail!(
            "id -g {user} failed: {}",
            String::from_utf8_lossy(&gid.stderr).trim()
        );
    }
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let gid = String::from_utf8_lossy(&gid.stdout).trim().to_string();
    let output = std::process::Command::new("chown")
        .args(["-R", &format!("{uid}:{gid}"), &base.display().to_string()])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "chown service dir to {user} ({uid}:{gid}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

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
        let output = podman_command()
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
        // ponytail: make rootfs readable for configured podman user when running via sudo
        ensure_rootfs_readable_for_podman_user(&spec.rootfs.rootfs_path)?;
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

    /// Inspect a running Russel container (used by e2e tests and future status API).
    #[allow(dead_code)]
    pub async fn inspect(&self, service_id: &str) -> anyhow::Result<Option<RunningContainer>> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = Self::container_name(service_id);
        let output = podman_command()
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

/// Reject podman passthrough args that Russel owns (name, detach, rootfs, etc.)
/// or that weaken container isolation (Issue #3).
pub fn validate_podman_passthrough_args(args: &[String]) -> anyhow::Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let next = args.get(i + 1);

        // --rootfs, --name/-n, --replace, -d/--detach (owned by Russel)
        if arg == "--rootfs" || arg.starts_with("--rootfs=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --rootfs");
        }
        if arg == "--name" || arg == "-n" || arg.starts_with("--name=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --name/-n");
        }
        if arg == "--replace" || arg.starts_with("--replace=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: --replace");
        }
        if arg == "-d" || arg == "--detach" || arg.starts_with("--detach=") {
            anyhow::bail!("podman passthrough arg reserved by Russel: detach (-d/--detach)");
        }

        // PORT env override (Issue #2): deny any arg that sets PORT variable
        if (arg == "-e" || arg == "--env")
            && let Some(val) = next
        {
            if val.starts_with("PORT=") {
                anyhow::bail!("podman passthrough arg must not override PORT ({arg} PORT=...)");
            }
            if val == "PORT" {
                anyhow::bail!("podman passthrough arg must not override PORT ({arg} PORT)");
            }
        }
        if arg.starts_with("-ePORT=") || arg.starts_with("--env=PORT=") {
            anyhow::bail!("podman passthrough arg must not override PORT ({arg})");
        }
        if arg == "-ePORT" {
            anyhow::bail!("podman passthrough arg must not override PORT ({arg})");
        }

        // Isolation-weakening flags (Issue #3)
        if arg == "--privileged" || arg == "--privileged=true" {
            anyhow::bail!("podman passthrough arg denied for security: --privileged");
        }

        // --pid: any value denied (host sharing)
        if arg == "--pid" {
            anyhow::bail!("podman passthrough arg denied for security: --pid");
        }
        if arg.starts_with("--pid=") {
            anyhow::bail!("podman passthrough arg denied for security: --pid=...");
        }

        // --network/--net/-net with host
        if (arg == "--network" || arg == "--net" || arg == "-net")
            && let Some(val) = next
            && val == "host"
        {
            anyhow::bail!("podman passthrough arg denied for security: {arg} host");
        }
        if arg == "--network=host" || arg == "--net=host" {
            anyhow::bail!("podman passthrough arg denied for security: {arg}");
        }

        // --ipc/--uts/--cgroupns/--userns with host
        for flag in &["--ipc", "--uts", "--cgroupns", "--userns"] {
            if arg == *flag
                && let Some(val) = next
                && val == "host"
            {
                anyhow::bail!("podman passthrough arg denied for security: {flag} host");
            }
            if arg.starts_with(&format!("{}={}", flag, "host")) {
                anyhow::bail!("podman passthrough arg denied for security: {arg}");
            }
        }

        // --security-opt (any form)
        if arg == "--security-opt" || arg.starts_with("--security-opt=") {
            anyhow::bail!("podman passthrough arg denied for security: --security-opt");
        }

        // --cap-add (deny all), --cap-drop (allow)
        if arg == "--cap-add" || arg.starts_with("--cap-add=") {
            anyhow::bail!("podman passthrough arg denied for security: --cap-add");
        }

        // --device / --add-device
        if arg == "--device" || arg.starts_with("--device=") {
            anyhow::bail!("podman passthrough arg denied for security: --device");
        }
        if arg == "--add-device" || arg.starts_with("--add-device=") {
            anyhow::bail!("podman passthrough arg denied for security: --add-device");
        }

        // -p / --publish / --publish-all / -P (port mapping owned by Russel)
        if arg == "-p" || arg == "--publish" || arg == "--publish-all" || arg == "-P" {
            anyhow::bail!("podman passthrough arg denied for security: port publish ({arg})");
        }
        if arg.starts_with("--publish=") {
            anyhow::bail!("podman passthrough arg denied for security: --publish=...");
        }
        // ponytail: catch compact -p forms (-p8080:80, -p8080, etc.)
        if arg.starts_with("-p") && arg != "-p" {
            anyhow::bail!("podman passthrough arg denied for security: port publish ({arg})");
        }

        i += 1;
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

    // Option order matters for `podman run --rootfs`: after `--rootfs PATH`,
    // remaining args are the container COMMAND (not more podman flags). So all
    // flags (mounts, ports, env, passthrough) must come *before* `--rootfs`.
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name,
        "--label".to_string(),
        format!("{LABEL_SERVICE}={}", spec.service_id),
        "--label".to_string(),
        format!("{LABEL_RUNTIME}={RUNTIME_CONTAINER}"),
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

    if !spec.extra_args.is_empty() {
        validate_podman_passthrough_args(&spec.extra_args)?;
        args.extend(spec.extra_args.clone());
    }

    for (key, value) in &spec.env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }

    args.push("--rootfs".to_string());
    args.push(spec.rootfs.rootfs_path.display().to_string());
    args.push(spec.rootfs.entrypoint.display().to_string());
    Ok(args)
}

async fn run_podman(args: &[String]) -> anyhow::Result<std::process::Output> {
    podman_command()
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
    let output = podman_command()
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
    let output = podman_command()
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

    #[test]
    fn select_nix_tool_store_path_picks_bin_output_from_multi_line() {
        let tmp = tempfile::tempdir().unwrap();
        let man = tmp.path().join("bash-man");
        let out = tmp.path().join("bash-out");
        std::fs::create_dir_all(man.join("share/man")).unwrap();
        std::fs::create_dir_all(out.join("bin")).unwrap();
        std::fs::write(out.join("bin/bash"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(out.join("bin/bash"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        // man listed first (as real `nix build bash` often does), then out.
        let stdout = format!("{}\n{}\n", man.display(), out.display());
        let path = select_nix_tool_store_path(stdout.as_bytes(), "bash").unwrap();
        assert_eq!(path, out);

        let err = select_nix_tool_store_path(format!("{}\n", man.display()).as_bytes(), "bash")
            .unwrap_err();
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
        let script = format!("#!{}/bin/bash\necho hi\n", interpreter.display());
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
            "/data:/data:ro".into(),
            "--mount".into(),
            "type=bind,source=/tmp/x,destination=/data".into(),
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
            let err =
                validate_podman_passthrough_args(&[arg.to_string(), next.unwrap().to_string()])
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
            let err = validate_podman_passthrough_args(&[flag.to_string(), value.to_string()])
                .unwrap_err();
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
        validate_entrypoint(&store, "runner").unwrap();
    }

    #[test]
    fn validate_entrypoint_rejects_empty_env_shebang() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("env-empty-store");
        let bin_dir = store.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("runner"), "#!/usr/bin/env\necho hi\n").unwrap();
        let err = validate_entrypoint(&store, "runner").unwrap_err();
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
        let err = validate_entrypoint(&store, "runner").unwrap_err();
        assert!(err.to_string().contains("path"), "{err}");
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
}
