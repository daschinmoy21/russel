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
    /// When true, include bash + curl debug tools in the container rootfs.
    /// Defaults to false for production hardening.
    pub debug: bool,
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

        let system = crate::build::current_system();
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
                system,
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
    let entrypoint = validate_entrypoint(&spec.store_path, &spec.bin_name, spec.debug)?;

    let rootfs_path = spec.base_dir.join("rootfs");
    if rootfs_path.exists() {
        tokio::fs::remove_dir_all(&rootfs_path).await?;
    }
    create_layout(&rootfs_path)?;
    write_etc_files(&rootfs_path)?;
    link_store_binary(&rootfs_path, &entrypoint, &spec.bin_name)?;

    if spec.debug {
        let cache = shared_debug_tools();
        let bash_store = cache.ensure_bash(spec.bash_store.as_deref()).await?;
        let curl_store = cache.ensure_curl(spec.curl_store.as_deref()).await?;
        link_debug_tool(&rootfs_path, &bash_store, "bash")?;
        link_debug_tool(&rootfs_path, &curl_store, "curl")?;
        install_env_wrapper(&rootfs_path)?;
        tracing::info!(
            service_id = %spec.service_id,
            "debug mode: bash + curl linked into rootfs"
        );
    }

    let container_entrypoint = PathBuf::from(format!("/bin/{}", spec.bin_name));
    tracing::info!(
        service_id = %spec.service_id,
        rootfs = %rootfs_path.display(),
        entrypoint = %container_entrypoint.display(),
        debug = spec.debug,
        "container rootfs prepared"
    );

    Ok(PreparedRootfs {
        rootfs_path,
        entrypoint: container_entrypoint,
    })
}

/// Require `store_path/bin/<bin_name>` and validate script shebangs when present.
pub fn validate_entrypoint(
    store_path: &Path,
    bin_name: &str,
    debug: bool,
) -> anyhow::Result<PathBuf> {
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
        validate_shebang(store_path, &bin_path, debug)?;
    }

    Ok(bin_path)
}

fn validate_shebang(store_path: &Path, bin_path: &Path, debug: bool) -> anyhow::Result<()> {
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
        // The env wrapper + bash are only installed when debug is enabled.
        if !debug {
            anyhow::bail!(
                "entrypoint uses `{interpreter} …` shebang but debug mode is disabled. \
                 Set `debug = true` in Russelfile [service] to include bash and the \
                 /usr/bin/env wrapper, or use an absolute /nix/store/… interpreter path \
                 in {}",
                bin_path.display()
            );
        }
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
        // /nix/store paths are bind-mounted into the container — require they exist
        // on the host so the mount resolves.
        if interpreter.starts_with("/nix/store/") {
            if interpreter.exists() {
                return Ok(());
            }
            anyhow::bail!(
                "{context} does not exist on host: {} (required for bind-mounted /nix/store)",
                interpreter.display()
            );
        }
        // Absolute paths within the package store_path are trusted (they resolve
        // via the store bind-mount at runtime).
        if interpreter.starts_with(store_path) {
            return Ok(());
        }
        // Any other absolute path (e.g. /usr/bin/python3) does not exist inside
        // the container — only /nix/store is mounted.
        anyhow::bail!(
            "{context} is not under /nix/store/ and will not be available \
             inside the container: {}",
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

/// Resolve Podman container name: prefer metadata (generation promote may leave
/// a gen-scoped name), else the canonical `russel-{service_id}`.
fn resolve_container_name(service_id: &str) -> String {
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
    if let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(name) = value.get("container_name").and_then(|v| v.as_str())
        && !name.is_empty()
    {
        return name.to_string();
    }
    ContainerRunner::container_name(service_id)
}
const LABEL_SERVICE: &str = "russel.service";
const LABEL_RUNTIME: &str = "russel.runtime";
const RUNTIME_CONTAINER: &str = "container";
const NIX_STORE_MOUNT: &str = "type=bind,source=/nix/store,destination=/nix/store,ro=true";
/// Grace period passed to `podman stop -t` before Podman sends SIGKILL.
const PODMAN_STOP_TIMEOUT_SECS: &str = "5";
/// Hard ceiling for the whole stop attempt (stop + kill). Prevents hung HTTP
/// handlers when Podman itself stalls in "Stopping".
const PODMAN_STOP_WALL_SECS: u64 = 20;

// ── RUSSEL_PODMAN_USER env support (Issue #278598) ───────────────────────────

// ── Rootless podman user (Issue #278598) ─────────────────────────────────────
// microVMs need a privileged ctrl (TAP/KVM). Containers must stay rootless.
// When ctrl is root, run podman as RUSSEL_PODMAN_USER or SUDO_USER.

/// Pure resolution: explicit env wins, then SUDO_USER when euid is root.
fn resolve_podman_user(
    explicit: Option<&str>,
    sudo_user: Option<&str>,
    euid: u32,
) -> Option<String> {
    let normalize = |s: &str| {
        let t = s.trim();
        if t.is_empty() || t == "root" {
            None
        } else {
            Some(t.to_string())
        }
    };
    if let Some(u) = explicit.and_then(normalize) {
        return Some(u);
    }
    if euid == 0 {
        return sudo_user.and_then(normalize);
    }
    None
}

fn configured_podman_user() -> Option<String> {
    let explicit = std::env::var("RUSSEL_PODMAN_USER").ok();
    let sudo_user = std::env::var("SUDO_USER").ok();
    let euid = unsafe { libc::geteuid() };
    resolve_podman_user(explicit.as_deref(), sudo_user.as_deref(), euid)
}

/// Where the active podman identity came from (for startup logs / errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodmanUserSource {
    Env,
    SudoUser,
    Ambient,
}

pub fn podman_user_source() -> PodmanUserSource {
    let explicit = std::env::var("RUSSEL_PODMAN_USER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "root");
    if explicit.is_some() {
        return PodmanUserSource::Env;
    }
    let euid = unsafe { libc::geteuid() };
    let sudo = std::env::var("SUDO_USER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "root");
    if euid == 0 && sudo.is_some() {
        PodmanUserSource::SudoUser
    } else {
        PodmanUserSource::Ambient
    }
}

/// Log which identity will run `podman` (call once at ctrl startup).
pub fn log_podman_identity() {
    match (configured_podman_user(), podman_user_source()) {
        (Some(user), PodmanUserSource::Env) => {
            tracing::info!(
                user = %user,
                "container podman identity: {user} (RUSSEL_PODMAN_USER) — microVM stays privileged"
            );
        }
        (Some(user), PodmanUserSource::SudoUser) => {
            tracing::info!(
                user = %user,
                "container podman identity: {user} (SUDO_USER) — microVM stays privileged"
            );
        }
        (_, PodmanUserSource::Ambient) | (None, _) => {
            let euid = unsafe { libc::geteuid() };
            if euid == 0 {
                tracing::warn!(
                    "ctrl is root and no RUSSEL_PODMAN_USER/SUDO_USER — container deploys \
                     will fail rootless check; set RUSSEL_PODMAN_USER=<user> or run via \
                     sudo from a non-root account"
                );
            } else {
                tracing::info!(
                    euid,
                    "container podman identity: ambient uid (rootless podman as this user)"
                );
            }
        }
    }
}

struct PodmanUserEnv {
    user: String,
    home: String,
    xdg_runtime: String,
    dbus: Option<String>,
}

async fn podman_user_env() -> Option<&'static PodmanUserEnv> {
    use tokio::sync::OnceCell;
    // OnceCell memoizes for the process lifetime: a successful resolution is
    // cached as Some(env), and a failure (None) is also cached — subsequent
    // calls return the same outcome without re-running id/getent.
    static ENV: OnceCell<Option<PodmanUserEnv>> = OnceCell::const_new();
    ENV.get_or_init(|| async {
        let user = configured_podman_user()?;
        let uid_output = tokio::process::Command::new("id")
            .args(["-u", &user])
            .output()
            .await
            .ok()?;
        if !uid_output.status.success() {
            return None;
        }
        let uid = String::from_utf8_lossy(&uid_output.stdout)
            .trim()
            .to_string();
        if uid.is_empty() {
            return None;
        }
        let home_output = tokio::process::Command::new("getent")
            .args(["passwd", &user])
            .output()
            .await
            .ok()?;
        if !home_output.status.success() {
            return None;
        }
        let out = String::from_utf8_lossy(&home_output.stdout);
        let home = out
            .split(':')
            .nth(5)
            .unwrap_or(&format!("/home/{user}"))
            .to_string();
        let xdg_runtime = format!("/run/user/{uid}");
        let dbus_path = format!("{xdg_runtime}/bus");
        let dbus = std::path::Path::new(&dbus_path)
            .exists()
            .then(|| format!("unix:path={dbus_path}"));
        if !std::path::Path::new(&xdg_runtime).exists() {
            tracing::warn!(
                user = %user,
                path = %xdg_runtime,
                "XDG_RUNTIME_DIR missing for podman user — enable lingering \
                 (loginctl enable-linger {user}) or log in once"
            );
        }
        Some(PodmanUserEnv {
            user,
            home,
            xdg_runtime,
            dbus,
        })
    })
    .await
    .as_ref()
}

/// Build a `Command` that runs `podman <args>` as the configured user when
/// ctrl is root and a non-root podman user was resolved (env or SUDO_USER).
pub(crate) async fn podman_command() -> Command {
    if let Some(env) = podman_user_env().await {
        let mut cmd = Command::new("sudo");
        cmd.args(["-u", &env.user, "-H", "env"]);
        cmd.arg(format!("HOME={}", env.home));
        cmd.arg(format!("XDG_RUNTIME_DIR={}", env.xdg_runtime));
        if let Some(ref dbus) = env.dbus {
            cmd.arg(format!("DBUS_SESSION_BUS_ADDRESS={dbus}"));
        }
        cmd.arg("podman");
        cmd
    } else {
        // Ambient podman. If ctrl is root (no user wrap), strip session vars
        // that `sudo -E` may have preserved — otherwise rootful podman writes
        // crun state into the invoking user's /run/user/UID as root:root and
        // later rootless runs fail with Permission denied.
        let mut cmd = Command::new("podman");
        if unsafe { libc::geteuid() } == 0 {
            cmd.env_remove("XDG_RUNTIME_DIR");
            cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
        }
        cmd
    }
}

/// Remove root-owned OCI runtime dirs under the podman user's XDG_RUNTIME_DIR.
///
/// A prior rootful `podman` that inherited `XDG_RUNTIME_DIR=/run/user/UID`
/// (common with `sudo -E`) leaves `crun/` owned by root:root mode 0700. Rootless
/// podman then cannot open its own runtime dir.
async fn sanitize_podman_user_runtime_dir() -> anyhow::Result<()> {
    let Some(env) = podman_user_env().await else {
        return Ok(());
    };
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }

    for name in ["crun", "runc"] {
        let path = std::path::Path::new(&env.xdg_runtime).join(name);
        if !path.exists() {
            continue;
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "skip runtime dir sanitize");
                continue;
            }
        };
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != 0 {
            continue;
        }
        tracing::warn!(
            path = %path.display(),
            user = %env.user,
            "removing root-owned podman runtime dir under user XDG_RUNTIME_DIR \
             (leftover from rootful podman; blocks rootless)"
        );
        tokio::fs::remove_dir_all(&path).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to remove root-owned {} (blocks rootless podman for {}): {e}. \
                 Run: sudo rm -rf {}",
                path.display(),
                env.user,
                path.display()
            )
        })?;
    }
    Ok(())
}

/// Make the service dir + rootfs usable by a non-root podman user when
/// `RUSSEL_PODMAN_USER` is set (ctrl runs as root via sudo for microVMs).
async fn ensure_rootfs_readable_for_podman_user(rootfs: &Path) -> anyhow::Result<()> {
    let Some(user) = configured_podman_user() else {
        return Ok(());
    };

    // Open path components for traversal (resolve symlinks first).
    let resolved = tokio::fs::canonicalize(rootfs)
        .await
        .unwrap_or_else(|_| rootfs.to_path_buf());
    let mut walk = resolved.as_path();
    loop {
        let output = tokio::process::Command::new("chmod")
            .args(["a+rx", &walk.display().to_string()])
            .output()
            .await?;
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

    // Only the rootfs + the service directory itself are given to the podman
    // user.  The service dir (non-recursive) lets podman create container.log;
    // rootfs (recursive) provides the container filesystem.  metadata.json and
    // other files under the service dir stay root-owned — container workloads
    // cannot tamper with them.
    let base = rootfs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| rootfs.to_path_buf());
    let uid = tokio::process::Command::new("id")
        .args(["-u", &user])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("id -u {user}: {e}"))?;
    if !uid.status.success() {
        anyhow::bail!(
            "id -u {user} failed: {}",
            String::from_utf8_lossy(&uid.stderr).trim()
        );
    }
    let gid = tokio::process::Command::new("id")
        .args(["-g", &user])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("id -g {user}: {e}"))?;
    if !gid.status.success() {
        anyhow::bail!(
            "id -g {user} failed: {}",
            String::from_utf8_lossy(&gid.stderr).trim()
        );
    }
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let gid = String::from_utf8_lossy(&gid.stdout).trim().to_string();

    // Service dir: non-recursive — podman needs to write container.log here;
    // metadata.json (written later by root) remains root-owned.
    let output = tokio::process::Command::new("chown")
        .args([&format!("{uid}:{gid}"), &base.display().to_string()])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown service dir to {user} ({uid}:{gid}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Rootfs: recursive — the container filesystem must be readable/writable
    // by the podman user.
    let output = tokio::process::Command::new("chown")
        .args(["-R", &format!("{uid}:{gid}"), &rootfs.display().to_string()])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown rootfs to {user} ({uid}:{gid}) failed: {}",
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
        sanitize_podman_user_runtime_dir().await?;
        let output = podman_command()
            .await
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

        // Validate args + rootless BEFORE stopping old container (#115).
        let log_path = container_log_path(&spec.service_id);
        let args = build_run_args(spec, &log_path)?;
        Self::ensure_rootless().await?;

        let name = Self::container_name(&spec.service_id);
        stop_and_remove_container(&name).await?;

        if let Some(parent) = spec.rootfs.rootfs_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // ponytail: make rootfs readable for configured podman user when running via sudo
        ensure_rootfs_readable_for_podman_user(&spec.rootfs.rootfs_path).await?;
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

        // Container env values are visible via `podman inspect`; prefer microvm
        // for secret-heavy workloads (its deploy.env is 0600 in a 0700 dir).
        tracing::info!(
            service_id = %spec.service_id,
            "note: container env values are visible via podman inspect; \
             prefer microvm runtime for secret-heavy workloads"
        );
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
        let name = resolve_container_name(service_id);
        stop_container(&name).await
    }

    /// Stop the container, remove it, and delete the prepared rootfs tree when present.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = resolve_container_name(service_id);
        stop_container(&name).await?;
        remove_container(&name).await?;

        // Remove the entire service base dir (metadata, rootfs, logs) — match
        // MicrovmRunner::destroy cleanup of /var/lib/russel/{service_id}.
        let base = default_base_dir(service_id);
        if base.exists() {
            tokio::fs::remove_dir_all(&base).await?;
        }
        Ok(())
    }

    /// Inspect a running Russel container (used by e2e tests and future status API).
    #[allow(dead_code)]
    pub async fn inspect(&self, service_id: &str) -> anyhow::Result<Option<RunningContainer>> {
        crate::microvm::MicrovmRunner::validate_service_id(service_id)?;
        let name = Self::container_name(service_id);
        let output = podman_command()
            .await
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

/// When `RUSSEL_ALLOW_PODMAN_ARGS=0` (or `false`/`no`/`off`), all passthrough
/// extras are rejected. Unset or any other value leaves the allowlist in effect.
fn podman_passthrough_disabled() -> bool {
    match std::env::var("RUSSEL_ALLOW_PODMAN_ARGS") {
        Ok(v) => {
            let v = v.trim();
            v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no") || v.eq_ignore_ascii_case("off")
        }
        Err(_) => false,
    }
}

/// Require a following value token for a space-separated flag form.
fn require_passthrough_value<'a>(flag: &str, next: Option<&'a String>) -> anyhow::Result<&'a str> {
    match next {
        Some(v) => Ok(v.as_str()),
        None => anyhow::bail!("podman passthrough arg {flag} requires a value"),
    }
}

/// Allowed network modes for passthrough `--network` / `--net`.
const ALLOWED_NETWORKS: &[&str] = &["bridge", "none", "slirp4netns", "pasta"];

fn validate_passthrough_network_mode(flag: &str, val: &str) -> anyhow::Result<()> {
    if val == "host" {
        // Keep the historical "host" wording for tests and operators.
        if flag.contains('=') || flag.starts_with("--network=") || flag.starts_with("--net=") {
            anyhow::bail!("podman passthrough arg denied for security: --network=host");
        }
        anyhow::bail!("podman passthrough arg denied for security: {flag} host");
    }
    if !ALLOWED_NETWORKS.contains(&val) {
        anyhow::bail!(
            "podman passthrough arg denied for security: {flag} {val} \
             (only bridge, none, slirp4netns, pasta are permitted)"
        );
    }
    Ok(())
}

fn validate_passthrough_userns(val: &str) -> anyhow::Result<()> {
    if val != "keep-id" {
        anyhow::bail!(
            "podman passthrough arg denied for security: --userns {val} \
             (only keep-id is permitted)"
        );
    }
    Ok(())
}

/// Reject env assignments that would override Russel-managed `PORT`.
fn validate_passthrough_env_assignment(flag: &str, val: &str) -> anyhow::Result<()> {
    if val == "PORT" || val.starts_with("PORT=") {
        if val == "PORT" {
            anyhow::bail!("podman passthrough arg must not override PORT ({flag} PORT)");
        }
        anyhow::bail!("podman passthrough arg must not override PORT ({flag} PORT=...)");
    }
    Ok(())
}

/// Deny known isolation-weakening / Russel-owned flags with stable error text.
/// Returns `Ok(())` when `arg` is not a special-cased deny (caller continues
/// allowlist matching). Bails when `arg` is reserved or always-denied.
fn deny_known_unsafe_passthrough(arg: &str) -> anyhow::Result<()> {
    // ── Reserved by Russel (owned flags) ──────────────────────────────────
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

    // Port publish is owned by Russel (managed -p mapping).
    if arg == "-p"
        || arg == "--publish"
        || arg == "--publish-all"
        || arg == "-P"
        || arg.starts_with("--publish=")
        || (arg.starts_with("-p") && arg != "-p" && !arg.starts_with("--"))
    {
        anyhow::bail!("podman passthrough arg denied for security: port publish ({arg})");
    }

    // ── Always-denied isolation / escape flags (explicit messages) ────────
    // All --privileged variants (including =false) — deny for simplicity.
    if arg == "--privileged" || arg.starts_with("--privileged=") {
        anyhow::bail!("podman passthrough arg denied for security: --privileged");
    }
    if arg == "--pid" || arg.starts_with("--pid=") {
        anyhow::bail!("podman passthrough arg denied for security: --pid");
    }
    if arg == "--user" || arg == "-u" || arg.starts_with("--user=") || arg.starts_with("-u=") {
        anyhow::bail!("podman passthrough arg denied for security: --user/-u");
    }
    if arg == "--entrypoint" || arg.starts_with("--entrypoint=") {
        anyhow::bail!("podman passthrough arg denied for security: --entrypoint");
    }
    if arg == "--env-file" || arg.starts_with("--env-file=") {
        anyhow::bail!("podman passthrough arg denied for security: --env-file");
    }
    if arg == "--security-opt" || arg.starts_with("--security-opt=") {
        anyhow::bail!("podman passthrough arg denied for security: --security-opt");
    }
    if arg == "--cap-add" || arg.starts_with("--cap-add=") {
        anyhow::bail!("podman passthrough arg denied for security: --cap-add");
    }
    if arg == "--device" || arg.starts_with("--device=") {
        anyhow::bail!("podman passthrough arg denied for security: --device");
    }
    if arg == "--add-device" || arg.starts_with("--add-device=") {
        anyhow::bail!("podman passthrough arg denied for security: --add-device");
    }
    // Isolation-weakening / host-control surfaces not on the allowlist.
    if arg == "--hooks-dir" || arg.starts_with("--hooks-dir=") {
        anyhow::bail!("podman passthrough arg denied for security: --hooks-dir");
    }
    if arg == "--runtime" || arg.starts_with("--runtime=") {
        anyhow::bail!("podman passthrough arg denied for security: --runtime");
    }
    if arg == "--log-driver" || arg.starts_with("--log-driver=") {
        anyhow::bail!("podman passthrough arg denied for security: --log-driver");
    }
    if arg == "--log-opt" || arg.starts_with("--log-opt=") {
        anyhow::bail!("podman passthrough arg denied for security: --log-opt");
    }
    // Disallow flipping managed read-only rootfs off via passthrough.
    if arg == "--read-only=false" || arg == "--read-only=0" || arg == "--read-only=no" {
        anyhow::bail!("podman passthrough arg denied for security: --read-only=false");
    }
    if arg == "--read-only" || arg.starts_with("--read-only=") {
        // Managed by Russel; not a passthrough knob.
        anyhow::bail!("podman passthrough arg denied for security: --read-only");
    }

    Ok(())
}

/// Validate podman passthrough args with an **allowlist** of known-safe flags
/// (Issue #191). Unknown `--*` / `-X` flags are denied by default.
///
/// Allowed (with value constraints where noted):
/// - `--network` / `--net` / `-net` — bridge | none | slirp4netns | pasta
/// - `--userns=keep-id` only
/// - `--cap-drop`, `--secret`, `-e`/`--env` (non-PORT), `--label`/`-l`,
///   `--annotation`, memory/cpu limits, `--tmpfs`, `--shm-size`, `--hostname`,
///   `--ulimit`
/// - `-v` / `--volume` / `--mount` — `/nix/store/` sources, read-only only
///
/// Set `RUSSEL_ALLOW_PODMAN_ARGS=0` to reject any non-empty extras.
pub fn validate_podman_passthrough_args(args: &[String]) -> anyhow::Result<()> {
    if !args.is_empty() && podman_passthrough_disabled() {
        anyhow::bail!(
            "podman passthrough args disabled (RUSSEL_ALLOW_PODMAN_ARGS=0); \
             unset or set to 1 to allow allowlisted extras"
        );
    }

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let next = args.get(i + 1);

        // Positional tokens are never valid in passthrough (would become COMMAND).
        if !arg.starts_with('-') {
            anyhow::bail!(
                "podman passthrough arg denied for security: unexpected positional '{arg}'"
            );
        }

        // Stable denials for reserved / known-unsafe flags (before allowlist match).
        deny_known_unsafe_passthrough(arg)?;

        // ── Allowlist: network ────────────────────────────────────────────
        if arg == "--network" || arg == "--net" || arg == "-net" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_network_mode(arg, val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--network=") {
            validate_passthrough_network_mode("--network=", val)?;
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--net=") {
            validate_passthrough_network_mode("--net=", val)?;
            i += 1;
            continue;
        }

        // ── Allowlist: userns (keep-id only) ──────────────────────────────
        if arg == "--userns" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_userns(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--userns=") {
            // Preserve "userns=…" wording for disallowed values.
            if val != "keep-id" {
                anyhow::bail!(
                    "podman passthrough arg denied for security: --userns={val} \
                     (only keep-id is permitted)"
                );
            }
            i += 1;
            continue;
        }

        // ── Allowlist: cap-drop (further drops only; re-asserted after extras) ─
        if arg == "--cap-drop" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--cap-drop=") {
            i += 1;
            continue;
        }

        // ── Allowlist: secret ─────────────────────────────────────────────
        if arg == "--secret" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--secret=") {
            i += 1;
            continue;
        }

        // ── Allowlist: env (-e / --env), non-PORT ─────────────────────────
        // Long forms before compact `-e…` so `--env` is not misparsed.
        if arg == "--env" || arg == "-e" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_env_assignment(arg, val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--env=") {
            validate_passthrough_env_assignment("--env", val)?;
            i += 1;
            continue;
        }
        // Compact -eKEY / -eKEY=value (single-letter short opt only).
        if let Some(val) = arg.strip_prefix("-e")
            && !val.is_empty()
            && !arg.starts_with("--")
        {
            // Mirror historical compact-PORT messages.
            if val == "PORT" || val.starts_with("PORT=") {
                anyhow::bail!("podman passthrough arg must not override PORT ({arg})");
            }
            i += 1;
            continue;
        }

        // ── Allowlist: label / annotation ─────────────────────────────────
        if arg == "--label" || arg == "-l" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--label=") {
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("-l")
            && !rest.is_empty()
            && !arg.starts_with("--")
        {
            i += 1;
            continue;
        }
        if arg == "--annotation" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--annotation=") {
            i += 1;
            continue;
        }

        // ── Allowlist: resource limits ────────────────────────────────────
        const RESOURCE_FLAGS: &[&str] = &[
            "--memory",
            "--memory-swap",
            "--cpus",
            "--cpu-shares",
            "--cpu-quota",
            "--cpu-period",
            "--shm-size",
            "--ulimit",
            "--hostname",
        ];
        if RESOURCE_FLAGS.contains(&arg) {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if RESOURCE_FLAGS.iter().any(|f| arg.starts_with(&format!("{f}="))) {
            i += 1;
            continue;
        }

        // ── Allowlist: tmpfs (in-container only; path:options) ────────────
        if arg == "--tmpfs" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_tmpfs(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--tmpfs=") {
            validate_passthrough_tmpfs(val)?;
            i += 1;
            continue;
        }

        // ── Allowlist: volume / mount (nix-store RO only) ─────────────────
        // Long forms before compact `-v…`.
        if arg == "--volume" || arg == "-v" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_volume(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--volume=") {
            validate_passthrough_volume(val)?;
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("-v")
            && !val.is_empty()
            && !arg.starts_with("--")
        {
            validate_passthrough_volume(val)?;
            i += 1;
            continue;
        }
        if arg == "--mount" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_mount(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--mount=") {
            validate_passthrough_mount(val)?;
            i += 1;
            continue;
        }

        // ── Default deny: unknown flag ────────────────────────────────────
        // Prefer a short flag name in the message for operators.
        let flag_name = arg.split('=').next().unwrap_or(arg);
        anyhow::bail!(
            "podman passthrough arg denied for security: {flag_name} is not on the allowlist"
        );
    }
    Ok(())
}

/// `--tmpfs` destinations must be absolute container paths (no host bind).
fn validate_passthrough_tmpfs(val: &str) -> anyhow::Result<()> {
    let dest = val.split_once(':').map(|(d, _)| d).unwrap_or(val);
    if dest.is_empty() || !dest.starts_with('/') {
        anyhow::bail!(
            "podman passthrough arg denied for security: --tmpfs destination \
             must be an absolute container path, got {val}"
        );
    }
    if dest.split('/').any(|seg| seg == "..") {
        anyhow::bail!(
            "podman passthrough arg denied for security: --tmpfs path must not \
             contain '..' components: {val}"
        );
    }
    Ok(())
}
/// Normalize and validate a passthrough mount/volume source path.
///
/// Requires an absolute path that lexically resolves under `/nix/store/` with
/// no `..` path components. Prefix-only checks are insufficient: paths like
/// `/nix/store/../etc/shadow` start with `/nix/store/` but escape the allowlist.
///
/// For non-existent paths (common for nix store entries not present on the
/// validating host), we manually normalize by resolving `.` and rejecting `..`
/// without filesystem access. When the path exists, we also `canonicalize` and
/// re-check the result remains under `/nix/store/`.
fn validate_nix_store_source(source: &str) -> anyhow::Result<()> {
    if source.is_empty() {
        anyhow::bail!("empty source path");
    }

    // Fast string-level rejection of `..` path segments (also covers odd
    // encodings that Path::components may still treat as ParentDir).
    if source.split('/').any(|seg| seg == "..") {
        anyhow::bail!("source path must not contain '..' components: {source}");
    }

    let path = Path::new(source);
    if !path.is_absolute() {
        // Relative sources can never be under /nix/store/; keep the allowlist
        // phrasing so call-site error checks stay stable.
        anyhow::bail!("source {source} is not under /nix/store/");
    }

    // Lexical normalization via Path::components — reject ParentDir, collapse
    // CurDir / repeated separators, require RootDir-rooted absolute form.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) => {
                anyhow::bail!("source path has unexpected prefix: {source}");
            }
            std::path::Component::RootDir => {
                normalized.push(std::path::Component::RootDir.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                anyhow::bail!("source path must not contain '..' components: {source}");
            }
            std::path::Component::Normal(c) => {
                normalized.push(c);
            }
        }
    }

    let normalized_str = normalized
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 source path: {source}"))?;

    // Must remain under /nix/store/<...>, not merely equal to /nix/store.
    if !normalized_str.starts_with("/nix/store/") {
        anyhow::bail!("source {source} is not under /nix/store/");
    }

    // Optional FS check: if the path exists, ensure real path still under store
    // (catches symlinks that escape /nix/store).
    if path.exists() {
        match path.canonicalize() {
            Ok(canon) => {
                let canon_str = canon
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 canonical path for {source}"))?;
                if !canon_str.starts_with("/nix/store/") {
                    anyhow::bail!(
                        "source {source} resolves outside /nix/store/ (canonical: {canon_str})"
                    );
                }
            }
            Err(e) => {
                anyhow::bail!("source path could not be canonicalized: {source}: {e}");
            }
        }
    }

    Ok(())
}

/// Validate a `-v` / `--volume` value: only bind mounts from /nix/store/ with
/// `:ro` are permitted.  Format: `SOURCE:DESTINATION[:OPTIONS]`.
/// Explicit `rw` is always denied even if `ro` is also listed.
fn validate_passthrough_volume(val: &str) -> anyhow::Result<()> {
    // Split on first colon to get source; the rest are dest+options.
    let (source, rest) = val.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("podman passthrough arg -v/--volume value missing ':' separator: {val}")
    })?;

    if source.is_empty() {
        anyhow::bail!("podman passthrough arg -v/--volume has empty source: {val}");
    }

    validate_nix_store_source(source).map_err(|e| {
        anyhow::anyhow!("podman passthrough arg -v/--volume denied for security: {e}")
    })?;

    // The rest contains DESTINATION[:OPTIONS] where OPTIONS is a
    // comma-separated list (e.g. "z,ro" or just "ro").
    let (_dest, options) = rest.split_once(':').unwrap_or((rest, ""));
    let opts: Vec<&str> = options
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let ro_present = opts.iter().any(|opt| *opt == "ro");
    let rw_present = opts.iter().any(|opt| *opt == "rw");

    if rw_present {
        anyhow::bail!(
            "podman passthrough arg -v/--volume denied for security: \
             {val} requests read-write (rw is not permitted; use :ro only)"
        );
    }

    if !ro_present {
        anyhow::bail!(
            "podman passthrough arg -v/--volume denied for security: \
             {val} is not read-only (missing :ro)"
        );
    }

    Ok(())
}

/// Validate a `--mount` value: only bind mounts from /nix/store/ with
/// `ro=true` or `readonly` are permitted.
/// Format: `type=TYPE,src=SOURCE,dst=DEST[,OPTIONS]`.
/// Explicit `rw` / `rw=true` is always denied.
fn validate_passthrough_mount(val: &str) -> anyhow::Result<()> {
    let mut mount_type = None;
    let mut source = None;
    let mut readonly = false;
    let mut readwrite = false;

    for kv in val.split(',') {
        let (key, value) = match kv.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (kv.trim(), ""),
        };
        match key {
            "type" => mount_type = Some(value),
            "src" | "source" => source = Some(value),
            "ro" | "readonly" if value.is_empty() || value == "true" || value == "1" => {
                readonly = true;
            }
            "rw" if value.is_empty() || value == "true" || value == "1" => {
                readwrite = true;
            }
            _ => {}
        }
    }

    let mount_type = mount_type.unwrap_or("bind");
    if mount_type != "bind" {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             only type=bind is permitted, got type={mount_type}"
        );
    }

    let source = source.ok_or_else(|| {
        anyhow::anyhow!("podman passthrough arg --mount missing source/src: {val}")
    })?;

    if source.is_empty() {
        anyhow::bail!("podman passthrough arg --mount has empty source: {val}");
    }

    validate_nix_store_source(source).map_err(|e| {
        anyhow::anyhow!("podman passthrough arg --mount denied for security: {e}")
    })?;

    if readwrite {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             {val} requests read-write (rw is not permitted; use ro=true/readonly)"
        );
    }

    if !readonly {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             {val} is not read-only (missing ro=true or readonly)"
        );
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
///
/// **Security note:** Environment variables (including values resolved from
/// `secret://` references) are passed via `-e KEY=value` and are visible to
/// anyone who can run `podman inspect` on the host. Prefer the microvm runtime
/// for secret-heavy workloads; its deploy.env is written to a 0700 directory.
pub fn build_run_args(spec: &ContainerStartSpec, log_path: &Path) -> anyhow::Result<Vec<String>> {
    crate::microvm::MicrovmRunner::validate_service_id(&spec.service_id)?;

    let name = ContainerRunner::container_name(&spec.service_id);
    let bind = crate::network::publish_bind_addr();
    // Podman -p: HOST:CONTAINER or IP:HOST:CONTAINER. Bracket IPv6 (contains ':').
    let port_mapping = if bind == "0.0.0.0" || bind == "::" {
        format!("{}:{}", spec.host_port, spec.guest_port)
    } else if bind.contains(':') {
        format!("[{}]:{}:{}", bind, spec.host_port, spec.guest_port)
    } else {
        format!("{}:{}:{}", bind, spec.host_port, spec.guest_port)
    };
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
        // ── Hardening: drop all capabilities, prevent privilege escalation,
        //     mount rootfs read-only with writable tmpfs for /tmp and /run.
        //     These are re-asserted after passthrough extras so last-wins
        //     cannot weaken isolation (Issue #191).
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--tmpfs".to_string(),
        "/run".to_string(),
    ];

    if !spec.extra_args.is_empty() {
        validate_podman_passthrough_args(&spec.extra_args)?;
        args.extend(spec.extra_args.clone());
    }

    // Re-assert isolation after extras: podman last-wins for boolean flags and
    // security-opt; cap-drop is cumulative but ALL here documents intent and
    // covers any attempt to re-order capability handling.
    args.push("--cap-drop".to_string());
    args.push("ALL".to_string());
    args.push("--security-opt".to_string());
    args.push("no-new-privileges".to_string());
    args.push("--read-only".to_string());

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
        .await
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
    let stop_fut = podman_command()
        .await
        .args(["stop", "-t", PODMAN_STOP_TIMEOUT_SECS, name])
        .output();

    match tokio::time::timeout(
        std::time::Duration::from_secs(PODMAN_STOP_WALL_SECS),
        stop_fut,
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() || is_missing_container(&output) => {
            return Ok(());
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                container = %name,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "podman stop failed — forcing kill"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(container = %name, error = %e, "podman stop spawn failed — forcing kill");
        }
        Err(_) => {
            tracing::warn!(
                container = %name,
                wall_secs = PODMAN_STOP_WALL_SECS,
                "podman stop timed out — forcing kill"
            );
        }
    }

    // Force path: SIGKILL via podman, treat missing as success.
    force_kill_container(name).await
}

async fn force_kill_container(name: &str) -> anyhow::Result<()> {
    let kill_fut = podman_command().await.args(["kill", name]).output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(10), kill_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            anyhow::bail!("failed to run podman kill: {e}");
        }
        Err(_) => {
            anyhow::bail!("podman kill timed out for container {name}");
        }
    };

    if output.status.success() || is_missing_container(&output) {
        return Ok(());
    }

    // Last resort: rm -f (also kills). Bound with a 15 s timeout — a hung
    // podman must not wedge stop/destroy/redeploy handlers indefinitely.
    let rm_fut = podman_command().await.args(["rm", "-f", name]).output();
    let rm = match tokio::time::timeout(std::time::Duration::from_secs(15), rm_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            anyhow::bail!("failed to run podman rm -f: {e}");
        }
        Err(_) => {
            anyhow::bail!("podman rm -f timed out for container {name}");
        }
    };
    if rm.status.success() || is_missing_container(&rm) {
        return Ok(());
    }

    anyhow::bail!(
        "podman kill/rm failed for {name}: kill={} rm={}",
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&rm.stderr).trim()
    )
}

async fn remove_container(name: &str) -> anyhow::Result<()> {
    let output = podman_command()
        .await
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

#[async_trait::async_trait]
impl crate::runtime::RuntimeLifecycle for ContainerRunner {
    async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        self.stop(service_id).await
    }

    async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        self.destroy(service_id).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
        let has_adjacent = |flag: &str, val: &str| -> bool {
            args.windows(2).any(|w| w[0] == flag && w[1] == val)
        };
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

    // ── Volume / mount denials ────────────────────────────────────────────

    #[test]
    fn reject_volume_non_nix_store_source() {
        for val in [
            "/data:/dest:ro",
            "/etc/passwd:/dest:ro",
            "relative:/dest:ro",
        ] {
            let err =
                validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
            assert!(
                err.to_string().contains("not under /nix/store/"),
                "expected rejection for -v {val}: {err}"
            );
        }
    }

    #[test]
    fn reject_volume_missing_ro() {
        let err =
            validate_podman_passthrough_args(&["-v".into(), "/nix/store/abc123:/dest".into()])
                .unwrap_err();
        assert!(
            err.to_string().contains("not read-only"),
            "expected ro rejection: {err}"
        );
    }

    #[test]
    fn accept_volume_nix_store_with_ro() {
        validate_podman_passthrough_args(&["-v".into(), "/nix/store/abc123:/dest:ro".into()])
            .unwrap();
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
        let err =
            validate_podman_passthrough_args(&["--volume=/data:/dest:ro".into()]).unwrap_err();
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
        validate_podman_passthrough_args(&[
            "--volume".into(),
            "/nix/store/abc123:/dest:z,ro".into(),
        ])
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
        let err = validate_podman_passthrough_args(&[
            "--mount".into(),
            "type=bind,dst=/dest,ro=true".into(),
        ])
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
            let err =
                validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
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
            let err =
                validate_podman_passthrough_args(&["--mount".into(), val.clone()]).unwrap_err();
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
        validate_podman_passthrough_args(&[
            "-v".into(),
            "/nix/store/./abc123/bin/foo:/dest:ro".into(),
        ])
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
            let err =
                validate_podman_passthrough_args(&["-v".into(), val.to_string()]).unwrap_err();
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
        let rel_err = validate_nix_store_source("relative/path").unwrap_err().to_string();
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
            let err = validate_podman_passthrough_args(&["--network".into(), val.to_string()])
                .unwrap_err();
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
            let err = validate_podman_passthrough_args(&["--userns".into(), val.to_string()])
                .unwrap_err();
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
        for arg in ["--read-only=false", "--read-only=0", "--read-only", "--read-only=true"] {
            let err = validate_podman_passthrough_args(&[arg.to_string()]).unwrap_err();
            assert!(
                err.to_string().contains("read-only"),
                "expected rejection for {arg}: {err}"
            );
        }
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
        let err = validate_podman_passthrough_args(&["--tmpfs".into(), "relative".into()])
            .unwrap_err();
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
        with_allow_podman_args_env(Some("0"), || {
            let err = validate_podman_passthrough_args(&["--network".into(), "bridge".into()])
                .unwrap_err();
            assert!(
                err.to_string().contains("RUSSEL_ALLOW_PODMAN_ARGS"),
                "expected disable message: {err}"
            );
            // Empty extras still ok.
            validate_podman_passthrough_args(&[]).unwrap();
        });
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
}
