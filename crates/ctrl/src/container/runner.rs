//! ContainerRunner lifecycle: start/stop/destroy, run-arg construction.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::passthrough::validate_podman_passthrough_args;
use super::podman_user::{
    ensure_rootfs_readable_for_podman_user, ensure_volume_dir_owned_by_podman_user, podman_command,
    sanitize_podman_user_runtime_dir,
};
use super::rootfs::{PreparedRootfs, RootfsSpec, default_base_dir, prepare_rootfs};

const CONTAINER_NAME_PREFIX: &str = "russel-";

const INIT_FLAG: &str = "--init";

/// Set once `podman run --init` fails for want of an init binary (catatonit
/// is only a Recommends of Debian's podman). Later runs go without it.
static INIT_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Trusted Podman names for a service: canonical `russel-{service_id}`, or a
/// generation-scoped name `russel-{service_id}_g{hex}` that dual-live promote
/// may leave in metadata (only `service_id` is rewritten on promote).
///
/// Rejects arbitrary metadata `container_name` values so stop/destroy cannot be
/// redirected at attacker-chosen Podman names (Issue #193).
/// Public for agent status probes (#214) that must accept generation-scoped names.
pub fn is_trusted_container_name(service_id: &str, name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    let canonical = ContainerRunner::container_name(service_id);
    if name == canonical {
        return true;
    }
    // Generation-scoped: russel-{service_id}_g{hex}
    let gen_prefix = format!("{canonical}_g");
    if let Some(generation) = name.strip_prefix(&gen_prefix) {
        return !generation.is_empty()
            && generation.len() <= 32
            && generation.chars().all(|c| c.is_ascii_hexdigit());
    }
    false
}

/// Resolve Podman container name: prefer metadata when the stored name is a
/// trusted Russel name (canonical or gen-scoped), else `russel-{service_id}`.
pub(crate) fn resolve_container_name(service_id: &str) -> String {
    let path = crate::metadata::metadata_path(service_id)
        .display()
        .to_string();
    if let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(name) = value.get("container_name").and_then(|v| v.as_str())
    {
        if is_trusted_container_name(service_id, name) {
            return name.to_string();
        }
        tracing::warn!(
            service_id,
            container_name = %name,
            "metadata container_name rejected (expected russel-{{service_id}} or gen-scoped); using canonical"
        );
    }
    ContainerRunner::container_name(service_id)
}
const LABEL_SERVICE: &str = "russel.service";
const LABEL_RUNTIME: &str = "russel.runtime";
const RUNTIME_CONTAINER: &str = "container";
pub(crate) const NIX_STORE_MOUNT: &str =
    "type=bind,source=/nix/store,destination=/nix/store,ro=true";
/// Grace period passed to `podman stop -t` before Podman sends SIGKILL.
const PODMAN_STOP_TIMEOUT_SECS: &str = "5";
/// Hard ceiling for the whole stop attempt (stop + kill). Prevents hung HTTP
/// handlers when Podman itself stalls in "Stopping".
const PODMAN_STOP_WALL_SECS: u64 = 20;

/// Console output is written here when a container starts; also available via `podman logs`.
pub fn container_log_path(service_id: &str) -> PathBuf {
    default_base_dir(service_id).join("container.log")
}

#[derive(Debug, Default, Clone)]
pub struct ContainerRunner;

#[derive(Debug, Clone, Default)]
pub struct ContainerStartSpec {
    pub service_id: String,
    pub rootfs: PreparedRootfs,
    pub host_port: u16,
    pub guest_port: u16,
    pub memory_mb: u16,
    /// `service.cpus` as `podman run --cpus`; `None` sets no CPU limit.
    pub cpus: Option<u8>,
    pub env: Vec<(String, String)>,
    /// Values resolved from `secret://` refs. Delivered as Podman secrets
    /// (`--secret NAME,type=env`) so they stay out of argv and inspect (#457).
    pub secret_env: Vec<(String, String)>,
    /// Operator `podman run` flags from `--podman-arg` / API `podman_args`
    /// (validated, inserted before `--rootfs`). Never reach the process.
    pub podman_args: Vec<String>,
    pub volumes: Vec<russel_core::volumes::ResolvedVolume>,
    pub extra_ports: Vec<(u16, u16)>,
    /// Russelfile `service.args`: process argv after the entrypoint.
    pub service_args: Vec<String>,
    /// Run the app unprivileged as the Podman user (`service.user` omitted).
    pub userns_keep_id: bool,
    pub restart: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub service_id: String,
    pub container_name: String,
    pub container_id: String,
    pub rootfs_path: PathBuf,
    /// The `--cpus` limit podman was started with; `None` when none applies.
    pub effective_cpus: Option<u8>,
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
        Self::rootless_info().await.map(|_| ())
    }

    /// `podman info` JSON, after checking that Podman is rootless.
    ///
    /// Cached for the process lifetime once it succeeds: rootless mode and
    /// the delegated cgroup controllers do not change under a running ctrl,
    /// and `podman info` costs 60–150 ms on every deploy. A failure is not
    /// cached, so the next deploy asks again.
    async fn rootless_info() -> anyhow::Result<String> {
        use tokio::sync::OnceCell;
        static INFO: OnceCell<String> = OnceCell::const_new();
        // Cheap, and a rootful podman can pollute the runtime dir at any time.
        sanitize_podman_user_runtime_dir().await?;
        INFO.get_or_try_init(Self::query_rootless_info)
            .await
            .cloned()
    }

    async fn query_rootless_info() -> anyhow::Result<String> {
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

        let info = String::from_utf8_lossy(&output.stdout).into_owned();
        match parse_podman_rootless(&info) {
            Ok(true) => Ok(info),
            Ok(false) => anyhow::bail!(rootless_required_error("false")),
            Err(_) => anyhow::bail!(rootless_required_error("not found")),
        }
    }

    pub async fn prepare(&self, spec: &RootfsSpec) -> anyhow::Result<PreparedRootfs> {
        prepare_rootfs(spec).await
    }

    pub async fn start(&self, spec: &ContainerStartSpec) -> anyhow::Result<RunningContainer> {
        russel_core::ids::validate_service_id(&spec.service_id)?;

        // Validate args + rootless BEFORE stopping old container (#115).
        let log_path = container_log_path(&spec.service_id);
        let mut args = build_run_args(spec, &log_path)?;
        let info = Self::rootless_info().await?;
        let mut effective_cpus = spec.cpus;
        // Rootless `--cpus` needs the cpu controller delegated to the podman
        // user; without it `podman run` refuses to start. Run unlimited and
        // say so rather than fail every deploy (cpus defaults to 1).
        if spec.cpus.is_some() && !podman_has_cgroup_controller(&info, "cpu") {
            tracing::warn!(
                service_id = %spec.service_id,
                "service.cpus not applied: the cpu cgroup controller is not delegated to the \
                 podman user (see service.cpus in https://russel.mintlify.site/reference/russelfile)"
            );
            let unlimited = ContainerStartSpec {
                cpus: None,
                ..spec.clone()
            };
            args = build_run_args(&unlimited, &log_path)?;
            effective_cpus = None;
        }

        let name = Self::container_name(&spec.service_id);
        stop_and_remove_container(&name).await?;

        if let Some(parent) = spec.rootfs.rootfs_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        ensure_rootfs_readable_for_podman_user(&spec.rootfs.rootfs_path).await?;
        prepare_managed_volume_dirs(&spec.volumes).await?;
        // Free the held publish port immediately before podman binds it.
        drop(crate::network::PortAllocator::take_hold(&spec.service_id));
        for i in 0..spec.extra_ports.len() {
            drop(crate::network::PortAllocator::take_hold(
                &russel_core::volumes::extra_port_key(&spec.service_id, i),
            ));
        }
        if let Err(e) = create_podman_secrets(&name, &spec.secret_env).await {
            // Do not leave the keys created before the failing one behind.
            remove_podman_secrets(&name).await;
            return Err(e);
        }
        let mut output = run_podman(&args).await?;
        if !output.status.success()
            && args.iter().any(|a| a == INIT_FLAG)
            && is_missing_init_binary(&output)
        {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "podman has no init binary (install catatonit); containers run without \
                 --init, so an app that ignores SIGTERM as PID 1 is killed after the stop timeout"
            );
            INIT_UNAVAILABLE.store(true, Ordering::Relaxed);
            // A failed run can leave the created container holding the name.
            let _ = run_podman(&["rm".into(), "-f".into(), name.clone()]).await;
            args.retain(|a| a != INIT_FLAG);
            output = run_podman(&args).await?;
        }
        if !output.status.success() {
            remove_podman_secrets(&name).await;
            anyhow::bail!(
                "podman run failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if container_id.is_empty() {
            anyhow::bail!("podman run returned empty container id");
        }

        // The container is already up. A metadata write failure must not fail
        // start; heartbeat would then keep treating a prior stop as down.
        if let Err(e) = crate::metadata::set_container_running(&spec.service_id, true) {
            tracing::warn!(
                service_id = %spec.service_id,
                error = %e,
                "started container but failed to record container_running=true"
            );
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
            effective_cpus,
        })
    }

    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        russel_core::ids::validate_service_id(service_id)?;
        let name = resolve_container_name(service_id);
        stop_container(&name).await?;
        // The container is already stopped. A metadata write failure must not
        // make the API restore status to deployed.
        if let Err(e) = crate::metadata::set_container_running(service_id, false) {
            tracing::warn!(
                service_id,
                error = %e,
                "stopped container but failed to record container_running=false"
            );
        }
        Ok(())
    }

    /// Stop the container, remove it, and delete the prepared rootfs tree when present.
    /// Managed volumes follow each volume's `keep` field.
    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        self.destroy_with_policy(service_id, russel_core::VolumeDestroyPolicy::FollowFile)
            .await
    }

    /// Destroy the container and service dir. `policy` chooses which managed
    /// volume directories survive. Absolute `host =` binds are never deleted.
    pub async fn destroy_with_policy(
        &self,
        service_id: &str,
        policy: russel_core::VolumeDestroyPolicy,
    ) -> anyhow::Result<()> {
        russel_core::ids::validate_service_id(service_id)?;
        let name = resolve_container_name(service_id);
        stop_container(&name).await?;
        remove_container(&name).await?;
        crate::paths::remove_generation_links(service_id);
        let volumes = volumes_recorded_for(service_id);
        cleanup_service_dir_in(&default_base_dir(service_id), &volumes, policy).await
    }
}

/// Destroy a container service honoring the destroy policy.
///
/// In-process ctrl path for DELETE /vm/{id} with keep_volumes. The API layer
/// calls this directly (not RuntimeLifecycle destroy, which has no policy
/// slot) so managed volumes follow the operator override. Absolute host binds
/// are never deleted.
pub async fn destroy_with_policy_for(
    service_id: &str,
    policy: russel_core::VolumeDestroyPolicy,
) -> anyhow::Result<()> {
    ContainerRunner::new()
        .destroy_with_policy(service_id, policy)
        .await
}

/// Stop and remove the container, and delete service files other than `volumes/`.
///
/// Redeploy and failed-deploy cleanup use this. `keep` applies to operator
/// destroy, not to replacing a running generation that is still using the data.
pub async fn destroy_preserving_volumes(service_id: &str) -> anyhow::Result<()> {
    russel_core::ids::validate_service_id(service_id)?;
    let name = resolve_container_name(service_id);
    stop_container(&name).await?;
    remove_container(&name).await?;
    crate::paths::remove_generation_links(service_id);
    remove_service_payload_keep_volumes(&default_base_dir(service_id)).await
}

/// Sibling of the service dir that holds `volumes/` while the service dir is renamed.
///
/// `{id}.volumes-stash` is not a valid service id (the dot is rejected) and is
/// reserved so discovery does not adopt it.
pub fn volumes_stash_path(service_dir: &Path) -> PathBuf {
    let name = service_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("service");
    service_dir.with_file_name(format!("{name}.volumes-stash"))
}

/// Move `{service}/volumes` to the stash path. No-op when there is no volumes tree.
///
/// A leftover stash from a crash is recovered when `volumes/` is missing or empty.
/// A leftover empty stash is deleted so detach can park the live tree. Two
/// non-empty trees still bail.
pub async fn detach_managed_volumes(service_dir: &Path) -> anyhow::Result<()> {
    let src = service_dir.join("volumes");
    let stash = volumes_stash_path(service_dir);

    if stash.exists() {
        if !src.exists() {
            return Ok(());
        }
        if dir_is_empty(&stash).await? {
            tokio::fs::remove_dir(&stash).await.map_err(|e| {
                anyhow::anyhow!(
                    "failed to remove empty volume stash {}: {e}",
                    stash.display()
                )
            })?;
        } else if dir_is_empty(&src).await? {
            tokio::fs::remove_dir(&src).await.map_err(|e| {
                anyhow::anyhow!("failed to remove empty volumes dir {}: {e}", src.display())
            })?;
            return Ok(());
        } else {
            anyhow::bail!(
                "volume stash already exists at {}; refusing to overwrite managed data",
                stash.display()
            );
        }
    }

    if !src.exists() {
        return Ok(());
    }

    tokio::fs::rename(&src, &stash).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to stash volumes from {} to {}: {e}",
            src.display(),
            stash.display()
        )
    })
}

/// Move a stashed volume tree back to `{service}/volumes`. No-op when nothing is stashed.
pub async fn attach_managed_volumes(service_dir: &Path) -> anyhow::Result<()> {
    let stash = volumes_stash_path(service_dir);
    if !stash.exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(service_dir).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to create service dir {}: {e}",
            service_dir.display()
        )
    })?;
    let dest = service_dir.join("volumes");
    if dest.exists() {
        if dir_is_empty(&dest).await.unwrap_or(false) {
            tokio::fs::remove_dir(&dest).await.map_err(|e| {
                anyhow::anyhow!("failed to remove empty volumes dir {}: {e}", dest.display())
            })?;
        } else {
            anyhow::bail!(
                "both stashed volumes ({}) and {} exist; refusing to overwrite either",
                stash.display(),
                dest.display()
            );
        }
    }
    tokio::fs::rename(&stash, &dest).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to restore volumes from {} to {}: {e}",
            stash.display(),
            dest.display()
        )
    })
}

/// Put `backup` back at `live`, keeping managed volumes that were stashed or
/// that still sit in `live`.
///
/// Cold redeploy renames the service dir to `.bak` after detaching `volumes/`.
/// A failed deploy may have attached those volumes into a new `live` tree, or
/// left both `live/volumes` and the stash. An empty `live/volumes` is dropped
/// so the stash can be reattached. Two non-empty trees bail.
pub async fn restore_backed_up_service_dir(live: &Path, backup: &Path) -> anyhow::Result<()> {
    let stash = volumes_stash_path(live);
    let live_volumes = live.join("volumes");

    if stash.exists() && live_volumes.exists() {
        if dir_is_empty(&live_volumes).await? {
            tokio::fs::remove_dir(&live_volumes).await.map_err(|e| {
                anyhow::anyhow!(
                    "failed to remove empty volumes dir {}: {e}",
                    live_volumes.display()
                )
            })?;
        } else if !dir_is_empty(&stash).await? {
            anyhow::bail!(
                "both stashed volumes ({}) and {} exist; refusing to overwrite either",
                stash.display(),
                live_volumes.display()
            );
        }
    }

    if live.exists() {
        let already_parked = stash.exists() && !live.join("volumes").exists();
        if !already_parked {
            detach_managed_volumes(live).await?;
        }
        remove_tree(live).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to remove partial service dir {}: {e}",
                live.display()
            )
        })?;
    }
    if backup.exists() {
        tokio::fs::rename(backup, live).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to restore {} from {}: {e}",
                live.display(),
                backup.display()
            )
        })?;
    }
    attach_managed_volumes(live).await
}

/// True when the directory contains a `volumes/` child and nothing else.
///
/// Operator destroy with `keep` leaves exactly that tree. It is not a legacy
/// microVM and must not be `remove_dir_all`'d on the next deploy.
pub fn dir_is_kept_volumes_only(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    let mut saw_volumes = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if name == "volumes" && entry.path().is_dir() {
            saw_volumes = true;
            continue;
        }
        return false;
    }
    saw_volumes
}

/// `remove_dir_all` for trees a rootless container has written to.
///
/// Podman creates mount points and bind targets inside `--rootfs` (and an
/// app may create files in a volume) as the container's user namespace. With
/// `userns = "keep-id"` the container root maps to a subordinate uid, so a
/// non-root ctrl cannot unlink those entries (#464). On EACCES, retry inside
/// `podman unshare`, where the ctrl user is root over its subuids. A root
/// ctrl never needs the fallback.
pub(crate) async fn remove_tree(path: &Path) -> std::io::Result<()> {
    remove_tree_with(path, podman_unshare_rm).await
}

pub(super) async fn remove_tree_with<F, Fut>(path: &Path, fallback: F) -> std::io::Result<()>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    match tokio::fs::remove_dir_all(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::info!(
                path = %path.display(),
                "removing subordinate-uid files via podman unshare"
            );
            fallback(path.to_path_buf()).await?;
            // An error checking counts as still present: fail closed.
            if tokio::fs::try_exists(path).await.unwrap_or(true) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is still present after podman unshare rm",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        other => other,
    }
}

async fn podman_unshare_rm(path: PathBuf) -> std::io::Result<()> {
    let output = podman_command()
        .await
        .args(["unshare", "rm", "-rf", "--"])
        .arg(&path)
        .output()
        .await
        // Not the spawn error's own kind: a missing podman binary would read
        // as NotFound, which callers take to mean the tree is already gone.
        .map_err(|e| std::io::Error::other(format!("podman unshare rm: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "podman unshare rm -rf {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Delete everything in `base` except a `volumes/` directory.
pub(crate) async fn remove_service_payload_keep_volumes(base: &Path) -> anyhow::Result<()> {
    if !base.exists() {
        return Ok(());
    }
    let mut rd = tokio::fs::read_dir(base).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some("volumes") && path.is_dir() {
            continue;
        }
        if path.is_dir() {
            remove_tree(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

impl ContainerRunner {
    /// Inspect a running Russel container (used by e2e tests and future status API).
    pub async fn inspect(&self, service_id: &str) -> anyhow::Result<Option<RunningContainer>> {
        russel_core::ids::validate_service_id(service_id)?;
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
            effective_cpus: None,
        }))
    }
}

pub(crate) fn rootless_required_error(detail: &str) -> String {
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

/// Whether `podman info` lists `controller` under `host.cgroupControllers`.
/// A missing list (older Podman) counts as available and leaves the decision
/// to `podman run`.
pub(crate) fn podman_has_cgroup_controller(info_json: &str, controller: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(info_json) else {
        return true;
    };
    match value
        .pointer("/host/cgroupControllers")
        .and_then(serde_json::Value::as_array)
    {
        Some(list) => list.iter().any(|c| c.as_str() == Some(controller)),
        None => true,
    }
}

/// Build `podman run` arguments for unit testing and runtime use.
///
/// **Security note:** plain env goes on `-e KEY=value` and is visible in
/// `podman inspect`. Values from `secret://` refs (`spec.secret_env`) only
/// appear as `--secret NAME,type=env,target=KEY`; the value itself goes to
/// `podman secret create` over stdin (#457).
pub fn build_run_args(spec: &ContainerStartSpec, log_path: &Path) -> anyhow::Result<Vec<String>> {
    russel_core::ids::validate_service_id(&spec.service_id)?;

    let name = ContainerRunner::container_name(&spec.service_id);
    let bind = crate::network::publish_bind_addr();
    // Podman -p: HOST:CONTAINER or IP:HOST:CONTAINER. Bracket IPv6 (contains ':').
    let publish = |host: u16, guest: u16| {
        if bind == "0.0.0.0" || bind == "::" {
            format!("{host}:{guest}")
        } else if bind.contains(':') {
            format!("[{bind}]:{host}:{guest}")
        } else {
            format!("{bind}:{host}:{guest}")
        }
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
        name.clone(),
        "--label".to_string(),
        format!("{LABEL_SERVICE}={}", spec.service_id),
        "--label".to_string(),
        format!("{LABEL_RUNTIME}={RUNTIME_CONTAINER}"),
        "--mount".to_string(),
        NIX_STORE_MOUNT.to_string(),
        "-p".to_string(),
        publish(spec.host_port, spec.guest_port),
        "--memory".to_string(),
        format!("{}m", spec.memory_mb),
        "--workdir".to_string(),
        "/".to_string(),
        "--log-driver".to_string(),
        "k8s-file".to_string(),
        "--log-opt".to_string(),
        format!("path={log_path}"),
        // Drop all capabilities, prevent privilege escalation,
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

    // The app would be PID 1, and the kernel drops SIGTERM for a PID 1 with
    // no handler of its own, so `podman stop` waited out its timeout and
    // SIGKILLed it (meilisearch). Podman's init forwards signals and reaps.
    if !INIT_UNAVAILABLE.load(Ordering::Relaxed) {
        args.push(INIT_FLAG.to_string());
    }

    if let Some(cpus) = spec.cpus {
        args.extend(["--cpus".to_string(), cpus.to_string()]);
    }

    for vol in &spec.volumes {
        let source = vol.host_path.to_str().ok_or_else(|| {
            anyhow::anyhow!("non-UTF-8 volume host path {}", vol.host_path.display())
        })?;
        // Defense in depth: core already rejects these at load/resolve, but a
        // volume recorded before that check, or a canonical path that later
        // grew a comma/newline, must not reach `podman --mount`.
        russel_core::volumes::reject_mount_csv_metacharacters(source, "volume host path")?;
        russel_core::volumes::reject_mount_csv_metacharacters(&vol.guest, "volume guest path")?;
        let mut mount = format!("type=bind,source={source},destination={}", vol.guest);
        if !vol.rw {
            mount.push_str(",ro=true");
        }
        args.extend(["--mount".to_string(), mount]);
    }

    for &(host, guest) in &spec.extra_ports {
        args.extend(["-p".to_string(), publish(host, guest)]);
    }

    if spec.userns_keep_id {
        // Unprivileged app (#466): run as the Podman user, who owns the
        // volumes, and let it bind ports below 1024 inside its own netns.
        args.extend(
            [
                "--userns",
                "keep-id",
                "--sysctl",
                "net.ipv4.ip_unprivileged_port_start=0",
            ]
            .map(String::from),
        );
    }
    // Restart on exit is the default (#450); `restart = "no"` opts out.
    if russel_core::volumes::RestartPolicy::of(spec.restart.as_deref())
        == russel_core::volumes::RestartPolicy::UnlessStopped
    {
        args.extend(
            [
                "--restart",
                russel_core::volumes::RestartPolicy::UNLESS_STOPPED,
            ]
            .map(String::from),
        );
    }

    if !spec.podman_args.is_empty() {
        validate_podman_passthrough_args(&spec.podman_args)?;
        args.extend(spec.podman_args.clone());
    }

    // Re-assert isolation after extras: podman last-wins for boolean flags and
    // security-opt; cap-drop is cumulative but ALL here documents intent and
    // covers any attempt to re-order capability handling.
    args.extend(
        [
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--read-only",
        ]
        .map(String::from),
    );

    for (key, value) in &spec.env {
        args.extend(["-e".to_string(), format!("{key}={value}")]);
    }

    for (key, _) in &spec.secret_env {
        // The key lands in a comma-separated option; core already enforces
        // [A-Za-z_][A-Za-z0-9_]*, so this only guards other callers.
        russel_core::validate_env_key(key)?;
        let secret = podman_secret_name(&name, key);
        args.extend([
            "--secret".to_string(),
            format!("{secret},type=env,target={key}"),
        ]);
    }

    args.extend([
        "--rootfs".to_string(),
        spec.rootfs.rootfs_path.display().to_string(),
        spec.rootfs.entrypoint.display().to_string(),
    ]);
    args.extend(spec.service_args.iter().cloned());
    Ok(args)
}

pub(crate) async fn prepare_managed_volume_dirs(
    volumes: &[russel_core::volumes::ResolvedVolume],
) -> anyhow::Result<()> {
    for vol in volumes {
        if !vol.managed {
            continue;
        }
        tokio::fs::create_dir_all(&vol.host_path)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create managed volume {}: {e}",
                    vol.host_path.display()
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o700);
            tokio::fs::set_permissions(&vol.host_path, perm)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to chmod 0700 managed volume {}: {e}",
                        vol.host_path.display()
                    )
                })?;
        }
        ensure_volume_dir_owned_by_podman_user(&vol.host_path).await?;
    }
    Ok(())
}

/// Remove the service directory while honoring managed-volume keep policy.
pub async fn cleanup_service_dir(
    service_id: &str,
    policy: russel_core::VolumeDestroyPolicy,
) -> anyhow::Result<()> {
    let volumes = volumes_recorded_for(service_id);
    cleanup_service_dir_in(&default_base_dir(service_id), &volumes, policy).await
}

/// Testable cleanup against an arbitrary service directory.
pub async fn cleanup_service_dir_in(
    base: &Path,
    volumes: &[russel_core::volumes::ResolvedVolume],
    policy: russel_core::VolumeDestroyPolicy,
) -> anyhow::Result<()> {
    if !base.exists() {
        return Ok(());
    }

    let volumes_root = base.join("volumes");
    if volumes_root.exists() {
        if matches!(policy, russel_core::VolumeDestroyPolicy::DeleteAll) {
            // Wipe the whole managed tree so stale dirs from removed volume
            // names go too (not only currently recorded paths).
            remove_tree(&volumes_root).await?;
        } else if !volumes.is_empty() {
            let volumes_root_canon = volumes_root.canonicalize().map_err(|e| {
                anyhow::anyhow!(
                    "failed to canonicalize volumes root {}: {e}",
                    volumes_root.display()
                )
            })?;
            for vol in volumes {
                if !vol.managed {
                    continue;
                }
                if policy.keep_managed(vol.keep) {
                    continue;
                }
                if !vol.host_path.exists() {
                    continue;
                }
                let host_canon = vol.host_path.canonicalize().map_err(|e| {
                    anyhow::anyhow!(
                        "failed to canonicalize managed volume {}: {e}",
                        vol.host_path.display()
                    )
                })?;
                if !host_canon.starts_with(&volumes_root_canon) {
                    anyhow::bail!(
                        "refusing to delete managed volume {}: path escapes volumes root {}",
                        vol.host_path.display(),
                        volumes_root.display()
                    );
                }
                remove_tree(&host_canon).await.map_err(|e| {
                    anyhow::anyhow!(
                        "failed to delete managed volume {}: {e}",
                        vol.host_path.display()
                    )
                })?;
            }
            // Drop empty volumes/ if nothing kept.
            if dir_is_empty(&volumes_root).await.unwrap_or(false) {
                let _ = tokio::fs::remove_dir(&volumes_root).await;
            }
        }
        // FollowFile/KeepAll with no recorded volumes: leave the tree alone.
    }

    // Remove everything except a kept volumes/ directory.
    let mut rd = tokio::fs::read_dir(&base).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some("volumes") && path.is_dir() {
            continue;
        }
        if path.is_dir() {
            remove_tree(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    if dir_is_empty(base).await.unwrap_or(true) {
        let _ = tokio::fs::remove_dir(&base).await;
    }
    Ok(())
}

async fn dir_is_empty(path: &Path) -> anyhow::Result<bool> {
    let mut rd = tokio::fs::read_dir(path).await?;
    Ok(rd.next_entry().await?.is_none())
}

pub(crate) fn volumes_recorded_for(service_id: &str) -> Vec<russel_core::volumes::ResolvedVolume> {
    let path = default_base_dir(service_id).join("metadata.json");
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    value
        .get("desired_state")
        .and_then(|ds| ds.get("volumes"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
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
    // A first deploy has nothing to stop: one `exists` replaces stop, rm and
    // secret ls. Leftover secrets are still cleared by create_podman_secrets.
    if container_exists(name).await == Some(false) {
        return Ok(());
    }
    stop_container(name).await?;
    remove_container(name).await
}

/// `podman container exists`: `Some(false)` only on its "no such container"
/// exit (1). `None` when the answer is unknown, so callers fall back to the
/// full stop and remove.
async fn container_exists(name: &str) -> Option<bool> {
    let output = podman_command()
        .await
        .args(["container", "exists", name])
        .output()
        .await
        .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
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
        // Secrets go with the container that references them: removing one
        // a container still uses breaks that container's next restart.
        remove_podman_secrets(name).await;
        return Ok(());
    }

    anyhow::bail!(
        "podman rm failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Podman secret holding one `secret://` env value of `container`:
/// `{container}.{KEY}`. Service ids and env keys contain no dot, so the
/// prefix `{container}.` names exactly this container's secrets.
pub(crate) fn podman_secret_name(container: &str, key: &str) -> String {
    format!("{container}.{key}")
}

/// Create the Podman secrets for `container`. Values go over stdin, never
/// argv. Leftovers from an earlier run of the same container name are removed
/// first instead of using `--replace`, which needs Podman 4.7 (Debian 12 ships
/// 4.3).
async fn create_podman_secrets(
    container: &str,
    secret_env: &[(String, String)],
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    if secret_env.is_empty() {
        return Ok(());
    }
    remove_podman_secrets(container).await;
    for (key, value) in secret_env {
        let name = podman_secret_name(container, key);
        let mut child = podman_command()
            .await
            .args(["secret", "create", &name, "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to run podman secret create: {e}"))?;
        // Reap the child before reporting a failed write so it is not left
        // behind; a short write must still fail even if podman exits 0.
        let written = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(value.as_bytes()).await,
            None => Ok(()),
        };
        let output = child.wait_with_output().await?;
        written.map_err(|e| anyhow::anyhow!("failed to write podman secret {name}: {e}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "podman secret create {name} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Best-effort removal of every Podman secret created for `container`.
async fn remove_podman_secrets(container: &str) {
    let listed = match run_podman(&[
        "secret".to_string(),
        "ls".to_string(),
        "--format".to_string(),
        "{{.Name}}".to_string(),
    ])
    .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                container,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "podman secret ls failed; container secrets may be left behind"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(container, error = %e, "podman secret ls failed");
            return;
        }
    };
    let names = secrets_for_container(&String::from_utf8_lossy(&listed.stdout), container);
    if names.is_empty() {
        return;
    }
    let mut args = vec!["secret".to_string(), "rm".to_string()];
    args.extend(names);
    match run_podman(&args).await {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::warn!(
            container,
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "podman secret rm failed"
        ),
        Err(e) => tracing::warn!(container, error = %e, "podman secret rm failed"),
    }
}

/// Names from `podman secret ls` output that belong to `container`.
pub(crate) fn secrets_for_container(ls_output: &str, container: &str) -> Vec<String> {
    let prefix = format!("{container}.");
    ls_output
        .lines()
        .map(str::trim)
        .filter(|n| n.starts_with(&prefix))
        .map(str::to_string)
        .collect()
}

/// `podman run --init` failed because the host has no init binary: Podman 5
/// says `could not find "catatonit"`, Podman 4 `container-init binary not
/// found on the host`.
pub(crate) fn is_missing_init_binary(output: &std::process::Output) -> bool {
    missing_init_stderr(&String::from_utf8_lossy(&output.stderr))
}

pub(crate) fn missing_init_stderr(stderr: &str) -> bool {
    stderr.contains("catatonit") || stderr.contains("container-init")
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
