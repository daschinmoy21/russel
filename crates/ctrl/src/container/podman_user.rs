//! Rootless podman identity when ctrl runs privileged for microVMs.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tokio::process::Command;

static PODMAN_PROGRAM: OnceLock<PathBuf> = OnceLock::new();

/// Test support: run `program` instead of `podman` for the rest of the
/// process. Integration tests point this at a missing binary so `/vms`
/// discovery and teardown never see or touch the host's real containers.
/// First call wins; returns the pinned program.
#[doc(hidden)]
pub fn pin_podman_program(program: impl Into<PathBuf>) -> &'static Path {
    PODMAN_PROGRAM.get_or_init(|| program.into())
}

fn podman_program() -> &'static Path {
    PODMAN_PROGRAM
        .get()
        .map_or(Path::new("podman"), PathBuf::as_path)
}

// microVMs need a privileged ctrl (TAP/KVM). Containers must stay rootless.
// When ctrl is root, run podman as RUSSEL_PODMAN_USER or SUDO_USER.

/// Pure resolution: explicit env wins, then SUDO_USER when euid is root.
pub(crate) fn resolve_podman_user(
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

pub(crate) fn configured_podman_user() -> Option<String> {
    let explicit = std::env::var("RUSSEL_PODMAN_USER").ok();
    let sudo_user = std::env::var("SUDO_USER").ok();
    let euid = unsafe { libc::geteuid() };
    resolve_podman_user(explicit.as_deref(), sudo_user.as_deref(), euid)
}

/// Numeric `uid` and `gid` for a user (`id -u` / `id -g`).
async fn podman_user_ids(user: &str) -> anyhow::Result<(String, String)> {
    let uid = id_number(user, "-u").await?;
    let gid = id_number(user, "-g").await?;
    Ok((uid, gid))
}

async fn id_number(user: &str, flag: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("id")
        .args([flag, user])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("id {flag} {user}: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "id {flag} {user} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

/// The unified (cgroup v2) path from `/proc/self/cgroup`: the `0::` line.
pub(crate) fn unified_cgroup_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

/// Whether podman must manage cgroups itself instead of asking systemd.
///
/// `install.sh host` and the NixOS module run ctrl as an unprivileged account
/// in a system unit (`/system.slice/…`) with `Delegate=yes`. Rootless podman's
/// default systemd cgroup manager asks that account's user manager to create
/// each container scope, and systemd refuses to move a process out of a system
/// unit, so every container fails to start (#524). With the cgroupfs manager,
/// podman creates the container cgroups inside the delegated subtree, where
/// `--cpus` and `--memory` still apply. Only when that subtree is ours: a ctrl
/// in a login session or a user unit keeps the systemd manager.
pub(crate) fn needs_cgroupfs_manager(
    cgroup_path: Option<&str>,
    cgroup_owner: Option<u32>,
    euid: u32,
) -> bool {
    euid != 0
        && cgroup_owner == Some(euid)
        && cgroup_path.is_some_and(|p| p.starts_with("/system.slice/"))
}

fn system_unit_cgroupfs() -> bool {
    static CGROUPFS: OnceLock<bool> = OnceLock::new();
    *CGROUPFS.get_or_init(|| {
        use std::os::unix::fs::MetadataExt;
        let proc_self = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let path = unified_cgroup_path(&proc_self);
        let owner = path.and_then(|p| {
            std::fs::metadata(format!("/sys/fs/cgroup{p}"))
                .ok()
                .map(|m| m.uid())
        });
        let euid = unsafe { libc::geteuid() };
        let cgroupfs = needs_cgroupfs_manager(path, owner, euid);
        if cgroupfs {
            tracing::info!(
                cgroup = path.unwrap_or_default(),
                "ctrl runs in a delegated system unit; podman uses --cgroup-manager=cgroupfs"
            );
        }
        cgroupfs
    })
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
        cmd.arg(podman_program());
        cmd
    } else {
        // Ambient podman. If ctrl is root (no user wrap), strip session vars
        // that `sudo -E` may have preserved — otherwise rootful podman writes
        // crun state into the invoking user's /run/user/UID as root:root and
        // later rootless runs fail with Permission denied.
        let mut cmd = Command::new(podman_program());
        if unsafe { libc::geteuid() } == 0 {
            cmd.env_remove("XDG_RUNTIME_DIR");
            cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
        } else if system_unit_cgroupfs() {
            cmd.arg("--cgroup-manager=cgroupfs");
        }
        cmd
    }
}

/// Remove root-owned OCI runtime dirs under the podman user's XDG_RUNTIME_DIR.
///
/// A prior rootful `podman` that inherited `XDG_RUNTIME_DIR=/run/user/UID`
/// (common with `sudo -E`) leaves `crun/` owned by root:root mode 0700. Rootless
/// podman then cannot open its own runtime dir.
pub(super) async fn sanitize_podman_user_runtime_dir() -> anyhow::Result<()> {
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

/// Make rootfs + container.log usable by a non-root podman user when
/// `RUSSEL_PODMAN_USER` is set (ctrl runs as root via sudo for microVMs).
///
/// **Security (Issue #193):** the service base directory stays root-owned and
/// non-group-writable (0750). Directory write would let the podman user
/// unlink/replace root-owned `metadata.json`. Only the rootfs tree is chowned
/// to the podman user. `container.log` is pre-created and chowned so the
/// k8s-file log driver can write without directory write permission.
///
/// Ancestor world `a+rx` is intentionally not applied (that made paths
/// world-traversable). Traversal for the podman user uses ACL `u:user:rx` on
/// the service dir, falling back to `root:podman_gid` + mode 0750.
pub(super) async fn ensure_rootfs_readable_for_podman_user(rootfs: &Path) -> anyhow::Result<()> {
    let Some(user) = configured_podman_user() else {
        return Ok(());
    };

    let base = rootfs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| rootfs.to_path_buf());
    tokio::fs::create_dir_all(&base)
        .await
        .map_err(|e| anyhow::anyhow!("create service dir {}: {e}", base.display()))?;

    let (uid, gid) = podman_user_ids(&user).await?;
    let base_s = base.display().to_string();

    // Keep service dir root-owned (NOT chowned to the podman user).
    let output = tokio::process::Command::new("chown")
        .args(["root:root", &base_s])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown service dir root:root failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // 0750: owner rwx, group rx, other none — never group/other write.
    let output = tokio::process::Command::new("chmod")
        .args(["0750", &base_s])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chmod 0750 service dir failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Grant podman user traverse+read on the service dir without write
    // (cannot unlink metadata.json). Prefer ACL; fall back to root:gid 0750.
    let acl = tokio::process::Command::new("setfacl")
        .args(["-m", &format!("u:{user}:rx"), &base_s])
        .output()
        .await;
    let acl_failed = match &acl {
        Ok(out) if out.status.success() => false,
        Ok(out) => {
            tracing::debug!(
                path = %base_s,
                user = %user,
                gid = %gid,
                err = %String::from_utf8_lossy(&out.stderr).trim(),
                "setfacl on service dir failed; falling back to root:gid 0750"
            );
            true
        }
        Err(e) => {
            tracing::debug!(
                path = %base_s,
                user = %user,
                gid = %gid,
                error = %e,
                "setfacl unavailable; falling back to root:gid 0750"
            );
            true
        }
    };
    if acl_failed {
        let output = tokio::process::Command::new("chown")
            .args([&format!("root:{gid}"), &base_s])
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "chown service dir root:{gid} fallback failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    // Best-effort: allow the podman user to traverse /var/lib/russel without
    // world a+rx. User execute-only ACL; skip on failure.
    let russel_root = crate::paths::data_root();
    if russel_root.exists() {
        let root_s = russel_root.display().to_string();
        let output = tokio::process::Command::new("setfacl")
            .args(["-m", &format!("u:{user}:--x"), &root_s])
            .output()
            .await;
        if let Ok(out) = &output
            && !out.status.success()
        {
            tracing::debug!(
                path = %root_s,
                user = %user,
                err = %String::from_utf8_lossy(&out.stderr).trim(),
                "setfacl execute on /var/lib/russel skipped"
            );
        }
    }

    // Pre-create container.log owned by the podman user so the log driver can
    // write without needing write permission on the service directory.
    let log_path = base.join("container.log");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o640);
        opts.open(&log_path)
            .map_err(|e| anyhow::anyhow!("create container.log {}: {e}", log_path.display()))?;
    }
    let log_s = log_path.display().to_string();
    let output = tokio::process::Command::new("chown")
        .args([&format!("{uid}:{gid}"), &log_s])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown container.log to {user} ({uid}:{gid}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // ACL write as a belt-and-suspenders for the log file.
    let _ = tokio::process::Command::new("setfacl")
        .args(["-m", &format!("u:{user}:rw"), &log_s])
        .output()
        .await;

    // Rootfs only (recursive) — container filesystem for the podman user.
    // Do NOT chown the service base dir.
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

/// Make a managed volume directory usable by the rootless podman user.
///
/// `volumes/<name>` is created root-owned 0700, so rootless podman cannot open
/// the bind source and `podman run` fails. Only the named volume is handed to
/// the podman user; the `volumes/` parent stays root-owned and non-writable
/// (traverse-only ACL, or `root:gid` 0750 fallback) so the container cannot
/// unlink sibling volume directories.
pub(super) async fn ensure_volume_dir_owned_by_podman_user(
    volume_dir: &Path,
) -> anyhow::Result<()> {
    let Some(user) = configured_podman_user() else {
        return Ok(());
    };

    let volumes_dir = volume_dir.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "managed volume {} has no parent directory",
            volume_dir.display()
        )
    })?;
    let volumes_s = volumes_dir.display().to_string();
    let (uid, gid) = podman_user_ids(&user).await?;

    // Keep the volumes dir root-owned (NOT chowned to the podman user).
    let output = tokio::process::Command::new("chown")
        .args(["root:root", &volumes_s])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown volumes dir root:root failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // 0750: owner rwx, group rx, other none — never group/other write.
    let output = tokio::process::Command::new("chmod")
        .args(["0750", &volumes_s])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chmod 0750 volumes dir failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Traverse without write, so the container cannot unlink sibling volume
    // dirs. Prefer ACL; fall back to root:gid 0750.
    let acl = tokio::process::Command::new("setfacl")
        .args(["-m", &format!("u:{user}:rx"), &volumes_s])
        .output()
        .await;
    let acl_failed = match &acl {
        Ok(out) if out.status.success() => false,
        Ok(out) => {
            tracing::debug!(
                path = %volumes_s,
                user = %user,
                gid = %gid,
                err = %String::from_utf8_lossy(&out.stderr).trim(),
                "setfacl on volumes dir failed; falling back to root:gid 0750"
            );
            true
        }
        Err(e) => {
            tracing::debug!(
                path = %volumes_s,
                user = %user,
                gid = %gid,
                error = %e,
                "setfacl unavailable; falling back to root:gid 0750"
            );
            true
        }
    };
    if acl_failed {
        let output = tokio::process::Command::new("chown")
            .args([&format!("root:{gid}"), &volumes_s])
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "chown volumes dir root:{gid} fallback failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    // Recursive: persisted data may predate the current podman user (e.g. after
    // RUSSEL_PODMAN_USER changes), and the container writes as that user.
    let output = tokio::process::Command::new("chown")
        .args([
            "-R",
            &format!("{uid}:{gid}"),
            &volume_dir.display().to_string(),
        ])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "chown volume dir to {user} ({uid}:{gid}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}
