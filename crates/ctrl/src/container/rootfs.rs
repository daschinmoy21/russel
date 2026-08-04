//! Container rootfs preparation (layout, store links, debug tools, entrypoint checks).

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
pub(crate) fn select_nix_tool_store_path(stdout: &[u8], tool: &str) -> anyhow::Result<PathBuf> {
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
