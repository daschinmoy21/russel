//! Deploy pipeline: cold deploy, dual-live generations, and rollback helpers.

use std::collections::HashMap;

mod config;
mod env;
mod phases;
mod pipeline;
mod rollback;
mod runtime;

// Re-export the pre-split public surface so callers keep `crate::deploy::…`.
pub use crate::metadata::prior_runtime_from_disk;
pub use env::{build_container_env, shell_quote, validate_bin_name};
pub use pipeline::DeployPipeline;
pub(crate) use pipeline::{DeployJob, Relaunch};
pub(crate) use rollback::{FailedLaunch, RecordedMicrovm, STOP_FILE};

/// How long a new generation that replaces a live one must keep running after
/// its app first answers, before the old one is gone for good (#493). An app
/// that listens and then crashes (postgres without /dev/shm died ~200 ms after
/// its port opened) fails the deploy inside this window, and the previous
/// generation keeps its traffic or gets it back. First deploys and relaunches
/// have nothing live to protect and skip it. Dual-live watches throughout
/// the longer drain and rechecks before retirement; later crashes are the
/// supervisor's job. See docs/concepts/lifecycle.md, "When a deploy counts as
/// ready".
pub(crate) const WATCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Dual-live only (#562): how long the previous generation keeps running
/// after the proxy serves the new one, before it is asked to stop. It gets no
/// new requests in that time, and the requests it already has can finish,
/// even in an app that exits at once on SIGTERM. Counts from the switch, so
/// the [`WATCH_WINDOW`] is part of it.
pub(crate) const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a retired generation gets between the stop request and SIGKILL
/// (#562). A container gets SIGTERM from `podman stop -t 5`; a microVM's app
/// gets it from the guest init ([`STOP_FILE`]).
pub(crate) const RETIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

type Events = tokio::sync::mpsc::Sender<russel_core::api::DeployEvent>;

/// Stream one progress line to the client. A client that hung up does not
/// stop the deploy.
async fn progress(tx: &Events, phase: &str, description: impl Into<String>) {
    let _ = tx
        .send(russel_core::api::DeployEvent::Progress {
            phase: phase.into(),
            description: description.into(),
        })
        .await;
}

/// Who the app runs as inside its sandbox (#466).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunAs {
    /// Unprivileged: the container's Podman user (`--userns=keep-id`), or in a
    /// microVM the uid/gid from [`microvm_app_ids`].
    App,
    /// `service.user = "root"`.
    Root,
}

impl RunAs {
    pub(crate) fn from_user(user: Option<&str>) -> Self {
        if user == Some("root") {
            Self::Root
        } else {
            Self::App
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Root => "root",
        }
    }

    /// From a recorded `desired_state`. Generations from before #466 have no
    /// `run_as`: they ran unprivileged only with `userns = "keep-id"`.
    pub(crate) fn from_desired_state(ds: Option<&serde_json::Value>) -> Self {
        let field = |key: &str| ds.and_then(|d| d.get(key)).and_then(|v| v.as_str());
        match field("run_as") {
            Some("root") => Self::Root,
            Some(_) => Self::App,
            None if field("userns") == Some("keep-id") => Self::App,
            None => Self::Root,
        }
    }

    /// `HOME` for an unprivileged app that does not set its own: the only
    /// writable place besides volumes, since the root filesystem is read-only.
    pub(crate) fn apply_env_defaults(self, env: &mut HashMap<String, String>) {
        if self == Self::App {
            env.entry("HOME".into()).or_insert_with(|| "/tmp".into());
        }
    }
}

/// uid/gid of an unprivileged microVM app: the control plane's own, so volume
/// files belong to the same user on the host and in the guest. Under a root
/// ctrl, `nobody`.
pub(crate) fn microvm_app_ids() -> (u32, u32) {
    // Safety: pure POSIX queries of this process.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if uid == 0 { (65534, 65534) } else { (uid, gid) }
}

/// Under a root ctrl, virtiofsd runs as root and a microVM app runs as
/// `nobody`, so hand the managed volume dirs to it. Absolute `host =` binds
/// stay the operator's to own. A non-root ctrl already owns them as the app.
pub(crate) fn chown_managed_volumes_for_app(
    volumes: &[russel_core::volumes::ResolvedVolume],
    run_as: RunAs,
) -> anyhow::Result<()> {
    // Safety: pure POSIX query of this process.
    if run_as != RunAs::App || unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let (uid, gid) = microvm_app_ids();
    for vol in volumes.iter().filter(|v| v.managed) {
        std::os::unix::fs::chown(&vol.host_path, Some(uid), Some(gid))
            .map_err(|e| anyhow::anyhow!("chown {}: {e}", vol.host_path.display()))?;
    }
    Ok(())
}

/// Write `deploy.env` into `cfg_dir` (created with mode `0700`) for the guest
/// agent init: `VM_IP`/`HOST_IP`/`PORT`/`APP` plus the shell-quoted user env.
///
/// Shared by cold deploy and microVM rollback so the file format, quoting, and
/// atomic write semantics stay identical across both paths.
pub(crate) fn write_deploy_env(
    cfg_dir: &str,
    vm_ip: &str,
    host_ip: &str,
    guest_port: u16,
    app_path: &str,
    env: &HashMap<String, String>,
    args: &[String],
    volumes: &[russel_core::volumes::ResolvedVolume],
    run_as: RunAs,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(cfg_dir)
        .map_err(|e| anyhow::anyhow!("failed to create config dir {}: {}", cfg_dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cfg_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("chmod config dir: {e}"))?;
    }
    // Shell-quote APP path to prevent injection through deploy.env.
    let mut deploy_env = format!(
        "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
        vm_ip,
        host_ip,
        guest_port,
        shell_quote(app_path)
    );
    // Append user env vars, shell-quoted (may include expanded secrets).
    for (key, value) in env {
        deploy_env.push_str(&format!("{}={}\n", key, shell_quote(value)));
    }
    // argv (service.args, one per line; core rejects newlines in entries) and
    // mounts ([[volumes]], #386) go before deploy.env, which is what the
    // guest agent waits for. Both are always written so a dropped list
    // clears the previous one.
    let argv_path = std::path::PathBuf::from(format!("{cfg_dir}/argv"));
    crate::secrets::secure_write(&argv_path, render_argv(args)?.as_bytes())?;
    let mounts_path = std::path::PathBuf::from(format!("{cfg_dir}/mounts"));
    crate::secrets::secure_write(
        &mounts_path,
        crate::microvm::render_guest_mounts(volumes).as_bytes(),
    )?;
    // `uid gid` the agent drops to before starting the app (#466); absent
    // means root.
    let user_path = std::path::PathBuf::from(format!("{cfg_dir}/user"));
    match run_as {
        RunAs::App => {
            let (uid, gid) = microvm_app_ids();
            crate::secrets::secure_write(&user_path, format!("{uid} {gid}\n").as_bytes())?;
        }
        RunAs::Root => match std::fs::remove_file(&user_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("remove {}: {e}", user_path.display()),
        },
    }
    // A dir that comes back from `.bak` (rollback) holds the stop request
    // its last retirement wrote; a VM that saw it would stop again (#562).
    let stop_path = std::path::PathBuf::from(format!("{cfg_dir}/{STOP_FILE}"));
    match std::fs::remove_file(&stop_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("remove {}: {e}", stop_path.display()),
    }
    let deploy_env_path = std::path::PathBuf::from(format!("{cfg_dir}/deploy.env"));
    crate::secrets::secure_write(&deploy_env_path, deploy_env.as_bytes())
}

/// `/config/argv` for the guest agent: each entry followed by `\n`.
pub(crate) fn render_argv(args: &[String]) -> anyhow::Result<String> {
    let mut out = String::new();
    for arg in args {
        if arg.contains(['\n', '\r', '\0']) {
            anyhow::bail!("service.args entry must not contain NUL or newlines");
        }
        out.push_str(arg);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod traffic_tests;
