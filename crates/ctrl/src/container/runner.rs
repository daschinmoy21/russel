//! ContainerRunner lifecycle: start/stop/destroy, run-arg construction.

use std::path::{Path, PathBuf};

use super::passthrough::validate_podman_passthrough_args;
use super::podman_user::{
    ensure_rootfs_readable_for_podman_user, podman_command, sanitize_podman_user_runtime_dir,
};
use super::rootfs::{PreparedRootfs, RootfsSpec, default_base_dir, prepare_rootfs};

const CONTAINER_NAME_PREFIX: &str = "russel-";

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
    let path = format!("/var/lib/russel/{service_id}/metadata.json");
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
