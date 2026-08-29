use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::process::Command;

use super::allowlist::{extract_url_host, validate_remote_host, validate_remote_host_dns};
use super::gc::gc_old_checkouts;
use super::lease::CheckoutLease;
use super::redact::{redact_repo_url, scrub_logged_repo_text};

static CHECKOUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
pub struct GitClient;

impl GitClient {
    pub async fn clone_or_use_local(&self, repo: &str) -> Result<(PathBuf, CheckoutLease)> {
        // Reject URLs starting with '-' (git option injection).
        if repo.starts_with('-') {
            anyhow::bail!("repository URL cannot start with '-' (looks like a git option)");
        }

        // Local path: must be absolute, exist, and not contain traversal.
        // Absolute local deploys are gated behind RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1
        // (default off) so a remote/shared control plane cannot treat the entire
        // host tree as a build root unless the operator opts in.
        let path = PathBuf::from(repo);
        if path.is_absolute() {
            if !local_path_deploy_allowed() {
                anyhow::bail!(
                    "local absolute path deploys are disabled; use a git URL \
                     (https://, http://, ssh://, or git@host:path) or set \
                     RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1 on the control plane for \
                     single-tenant trusted hosts"
                );
            }
            if !path.exists() {
                anyhow::bail!("local path does not exist: {}", repo);
            }
            // Reject '..' components in the path string.
            for component in path.components() {
                if component == std::path::Component::ParentDir {
                    anyhow::bail!("local path must not contain '..' components");
                }
            }
            return Ok((path, CheckoutLease::inactive()));
        }

        // If it looks like a local relative path that exists, reject — require absolute.
        if PathBuf::from(repo).exists() {
            anyhow::bail!(
                "local path '{}' is relative; use an absolute path for local deploys",
                repo
            );
        }

        // Remote: validate URL scheme and extract the host for SSRF checks.
        let scheme = if repo.starts_with("https://") {
            "https"
        } else if repo.starts_with("http://") {
            "http"
        } else if repo.starts_with("ssh://") {
            "ssh"
        } else if repo.starts_with("git@") {
            // git@host:path — allowed, no scheme prefix to strip.
            "git-scp"
        } else if repo.starts_with("file://") {
            anyhow::bail!("file:// URLs are not allowed for security");
        } else {
            anyhow::bail!(
                "unsupported repository URL scheme; must be https://, http://, ssh://, or git@host:path"
            );
        };

        // Block private/metadata/link-local/loopback hosts for all remote schemes.
        //
        // Residual risk — DNS rebinding TOCTOU:
        // We resolve hostnames and reject private addresses before clone, but a
        // malicious authoritative DNS server can return a public IP for our check
        // and a private IP for git's subsequent lookup. Full mitigation would
        // require pinning the resolved address for the whole clone (not practical
        // with stock git). Prefer HTTPS + known hosts; use
        // RUSSEL_GIT_HOST_ALLOWLIST only for trusted internal hostnames.
        //
        // HTTP redirects: we set `http.followRedirects=false` on http(s) clones so
        // an allowed host cannot 30x into a blocked one. Canonical clone URLs
        // (e.g. GitHub) do not require redirects.
        let host = extract_url_host(repo, scheme)?;
        validate_remote_host(&host)?;
        validate_remote_host_dns(&host).await?;

        let checkout_root = PathBuf::from("/tmp/russel/checkouts");
        fs::create_dir_all(&checkout_root).context("failed to create checkout directory")?;

        // Best-effort GC of old checkouts (age-based) before clone.
        if let Err(e) = gc_old_checkouts(&checkout_root, Duration::from_secs(24 * 3600)) {
            tracing::warn!(error = %e, "checkout GC failed (continuing)");
        }

        // Immutable per-deploy workdir: URL hash prefix + unique deploy stamp so
        // concurrent deploys / age-based GC cannot clobber an in-use checkout.
        let checkout = reserve_checkout_dir(&checkout_root, repo)?;

        let mut cmd = Command::new("git");
        for arg in clone_security_config_args(scheme) {
            cmd.arg(arg);
        }
        let output = cmd
            .arg("clone")
            .arg("--")
            .arg(repo)
            .arg(&checkout)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Err(cleanup_err) = fs::remove_dir_all(&checkout)
                && cleanup_err.kind() != std::io::ErrorKind::NotFound
            {
                anyhow::bail!(
                    "{}",
                    clone_failure_message(
                        repo,
                        stderr.as_ref(),
                        Some((checkout.as_path(), &cleanup_err)),
                    )
                );
            }
            anyhow::bail!("{}", clone_failure_message(repo, stderr.as_ref(), None));
        }

        Ok((checkout.clone(), CheckoutLease::active(checkout)))
    }

    /// Keep a resolved checkout out of age-based GC until the caller finishes
    /// reading it. The checkout is immutable and may safely be used by other
    /// concurrent deployments, but must not be removed while this lease lives.
    pub fn hold_checkout(&self, path: &Path) -> CheckoutLease {
        CheckoutLease::active(path.to_path_buf())
    }
}

/// Format a clone failure without interpolating HTTP userinfo from `repo`.
///
/// `stderr` may itself echo the clone URL (git often does), sometimes with
/// a different host case than the request string.
pub(super) fn clone_failure_message(
    repo: &str,
    stderr: &str,
    cleanup: Option<(&Path, &std::io::Error)>,
) -> String {
    let safe_repo = redact_repo_url(repo);
    let stderr = scrub_logged_repo_text(stderr, repo);
    let stderr = stderr.trim();
    match cleanup {
        Some((checkout, cleanup_err)) => format!(
            "git clone failed for {safe_repo}: {stderr} (also failed to remove partial checkout {}: {cleanup_err})",
            checkout.display()
        ),
        None => format!(
            "git clone failed for {safe_repo}: {stderr}.\nUse an absolute path for local deploys."
        ),
    }
}

/// Git `-c` options applied to remote clones to reduce SSRF surface.
///
/// For http/https clones, disables following HTTP redirects so an allowlisted
/// public host cannot 30x into a private/metadata endpoint.
pub(super) fn clone_security_config_args(scheme: &str) -> Vec<&'static str> {
    let mut args = Vec::new();
    if matches!(scheme, "http" | "https") {
        args.extend(["-c", "http.followRedirects=false"]);
    }
    args
}

/// Whether absolute local path deploys are permitted.
///
/// Default **off**. Set `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane
/// only on single-tenant trusted hosts (local dev). When a remote client sends
/// an absolute path, the path is resolved on the **control-plane host**, not the
/// client — so leaving this enabled on a shared/remote ctrl exposes the host
/// filesystem as a build root.
pub(super) fn local_path_deploy_allowed() -> bool {
    std::env::var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

pub(super) fn fnv1a_u64(bytes: impl AsRef<[u8]>) -> u64 {
    bytes
        .as_ref()
        .iter()
        .fold(14_695_981_039_346_656_037u64, |acc, &b| {
            (acc ^ b as u64).wrapping_mul(1_099_511_628_211)
        })
}

/// Stable URL identity: the standard 64-bit FNV-1a result as 16 lowercase hex
/// digits.
fn url_hash_hex(repo: &str) -> String {
    format!("{:016x}", fnv1a_u64(repo.as_bytes()))
}

/// Stable directory name stem for a remote repo URL.
///
/// Hex of FNV-1a over the original URL. Distinct URLs stay unique; userinfo
/// never appears as a path component.
pub(super) fn checkout_dir_name(repo: &str) -> String {
    format!("repo-{}", url_hash_hex(repo))
}

/// Per-deploy immutable directory under the URL stem.
pub(super) fn unique_checkout_dir_name(repo: &str) -> String {
    unique_checkout_dir_name_with(
        repo,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        CHECKOUT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

pub(super) fn unique_checkout_dir_name_with(repo: &str, nanos: u128, sequence: u64) -> String {
    let stamp = format!("{:x}", nanos ^ ((std::process::id() as u128) << 16));
    format!("{}-d{stamp}-{sequence:016x}", checkout_dir_name(repo))
}

/// Atomically reserve an empty destination directory for a clone. The empty
/// directory is accepted by `git clone`; `create_dir` makes name collisions
/// observable and allows a retry without deleting another deployment's tree.
pub(super) fn reserve_checkout_dir(checkout_root: &Path, repo: &str) -> anyhow::Result<PathBuf> {
    for _ in 0..1024 {
        let checkout = checkout_root.join(unique_checkout_dir_name(repo));
        match fs::create_dir(&checkout) {
            Ok(()) => return Ok(checkout),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to reserve checkout directory {}",
                        checkout.display()
                    )
                });
            }
        }
    }
    anyhow::bail!("could not reserve a unique checkout directory after 1024 attempts")
}
