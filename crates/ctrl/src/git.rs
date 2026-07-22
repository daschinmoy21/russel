use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Default)]
struct CheckoutState {
    leases: usize,
    deleting: bool,
}

static ACTIVE_CHECKOUTS: LazyLock<Mutex<HashMap<PathBuf, CheckoutState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHECKOUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn active_checkouts() -> std::sync::MutexGuard<'static, HashMap<PathBuf, CheckoutState>> {
    ACTIVE_CHECKOUTS.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("ACTIVE_CHECKOUTS lock poisoned — recovering");
        poisoned.into_inner()
    })
}

/// Lease for a checkout that is still being read by a deployment.
#[derive(Debug)]
pub struct CheckoutLease {
    path: Option<PathBuf>,
}

impl CheckoutLease {
    fn inactive() -> Self {
        Self { path: None }
    }

    fn active(path: PathBuf) -> Self {
        loop {
            let mut active = active_checkouts();
            let deleting = active.get(&path).is_some_and(|state| state.deleting);
            if deleting {
                drop(active);
                std::thread::yield_now();
                continue;
            }
            active.entry(path.clone()).or_default().leases += 1;
            return Self { path: Some(path) };
        }
    }
}

impl Clone for CheckoutLease {
    fn clone(&self) -> Self {
        match &self.path {
            Some(path) => Self::active(path.clone()),
            None => Self::inactive(),
        }
    }
}

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let mut active = active_checkouts();
        if let Some(state) = active.get_mut(&path) {
            debug_assert!(state.leases > 0);
            state.leases = state.leases.saturating_sub(1);
            if state.leases == 0 && !state.deleting {
                active.remove(&path);
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct GitClient;

impl GitClient {
    pub async fn clone_or_use_local(&self, repo: &str) -> Result<(PathBuf, CheckoutLease)> {
        // Reject URLs starting with '-' (git option injection).
        if repo.starts_with('-') {
            anyhow::bail!("repository URL cannot start with '-' (looks like a git option)");
        }

        // Local path: must be absolute, exist, and not contain traversal.
        let path = PathBuf::from(repo);
        if path.is_absolute() {
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

        // Remote: validate URL scheme.
        let (scheme, rest) = if let Some(rest) = repo.strip_prefix("https://") {
            ("https", rest)
        } else if let Some(rest) = repo.strip_prefix("http://") {
            ("http", rest)
        } else if let Some(rest) = repo.strip_prefix("ssh://") {
            ("ssh", rest)
        } else if repo.starts_with("git@") {
            // git@host:path — allowed, no scheme prefix to strip.
            ("git-scp", repo)
        } else if repo.starts_with("file://") {
            anyhow::bail!("file:// URLs are not allowed for security");
        } else {
            anyhow::bail!(
                "unsupported repository URL scheme; must be https://, http://, ssh://, or git@host:path"
            );
        };

        // Reject link-local/metadata hosts (169.254.169.254) for http(s).
        if scheme == "https" || scheme == "http" {
            let host = rest.split('/').next().unwrap_or("");
            let host = host.split(':').next().unwrap_or(host); // strip port
            if host == "169.254.169.254" {
                anyhow::bail!("repository URL host is a link-local metadata service");
            }
            if host.is_empty() {
                anyhow::bail!("repository URL has empty host");
            }
        }

        let checkout_root = PathBuf::from("/tmp/russel/checkouts");
        fs::create_dir_all(&checkout_root).context("failed to create checkout directory")?;

        // Best-effort GC of old checkouts (age-based) before clone.
        if let Err(e) = gc_old_checkouts(&checkout_root, Duration::from_secs(24 * 3600)) {
            tracing::warn!(error = %e, "checkout GC failed (continuing)");
        }

        // Immutable per-deploy workdir: URL hash prefix + unique deploy stamp so
        // concurrent deploys / age-based GC cannot clobber an in-use checkout.
        let checkout = reserve_checkout_dir(&checkout_root, repo)?;

        let output = Command::new("git")
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
                    "git clone failed for {repo}: {} (also failed to remove partial checkout {}: {})",
                    stderr.trim(),
                    checkout.display(),
                    cleanup_err
                );
            }
            anyhow::bail!(
                "git clone failed for {repo}: {}.\nUse an absolute path for local deploys.",
                stderr.trim()
            );
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

fn fnv1a_u64(bytes: impl AsRef<[u8]>) -> u64 {
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

/// Stable, unique directory name stem for a remote repo URL.
///
/// Format: `{sanitized_prefix}-{16 hex FNV identity}`.
fn checkout_dir_name(repo: &str) -> String {
    let hash = url_hash_hex(repo);
    let prefix: String = repo
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    let prefix = prefix.trim_matches('-');
    if prefix.is_empty() {
        format!("repo-{hash}")
    } else {
        format!("{prefix}-{hash}")
    }
}

/// Per-deploy immutable directory under the URL stem.
fn unique_checkout_dir_name(repo: &str) -> String {
    unique_checkout_dir_name_with(
        repo,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        CHECKOUT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

fn unique_checkout_dir_name_with(repo: &str, nanos: u128, sequence: u64) -> String {
    let stamp = format!("{:x}", nanos ^ ((std::process::id() as u128) << 16));
    format!("{}-d{stamp}-{sequence:016x}", checkout_dir_name(repo))
}

/// Atomically reserve an empty destination directory for a clone. The empty
/// directory is accepted by `git clone`; `create_dir` makes name collisions
/// observable and allows a retry without deleting another deployment's tree.
fn reserve_checkout_dir(checkout_root: &Path, repo: &str) -> anyhow::Result<PathBuf> {
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

/// Claim an unleased checkout and delete it while holding the active-checkout
/// mutex. A new lease therefore cannot appear between the eligibility check
/// and the deletion.
fn remove_unleased_checkout(path: &Path) -> std::io::Result<bool> {
    let mut active = active_checkouts();
    let state = active.entry(path.to_path_buf()).or_default();
    if state.leases != 0 || state.deleting {
        return Ok(false);
    }
    state.deleting = true;
    let result = fs::remove_dir_all(path);
    active.remove(path);
    result.map(|()| true)
}

/// Remove checkout directories older than `max_age`.
///
/// Filesystem errors on individual entries are logged and skipped so one
/// bad directory does not abort GC of the rest.
pub fn gc_old_checkouts(checkout_root: &Path, max_age: Duration) -> anyhow::Result<usize> {
    if !checkout_root.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0usize;
    let entries = match fs::read_dir(checkout_root) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read checkout root for GC");
            return Ok(0);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "skip unreadable checkout entry during GC");
                continue;
            }
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unreadable metadata during GC"
                );
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unknown mtime during GC"
                );
                continue;
            }
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > max_age {
            match remove_unleased_checkout(&entry.path()) {
                Ok(true) => {
                    tracing::info!(path = %entry.path().display(), "GC removed old git checkout");
                    removed += 1;
                }
                Ok(false) => {
                    tracing::debug!(path = %entry.path().display(), "skip active checkout during GC");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "failed to GC git checkout"
                    );
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    #[test]
    fn fnv1a_uses_xor_before_multiply() {
        assert_eq!(fnv1a_u64("hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn checkout_dir_name_is_unique_for_distinct_urls() {
        let a = checkout_dir_name("https://github.com/org/repo-a.git");
        let b = checkout_dir_name("https://github.com/org/repo-b.git");
        assert_ne!(a, b);
        // Same URL is stable.
        assert_eq!(a, checkout_dir_name("https://github.com/org/repo-a.git"));
        // 16 hex digits in the hash suffix.
        let hash = a.rsplit('-').next().unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn checkout_dir_name_avoids_sanitize_collision() {
        // Identical 48-char sanitized prefix; only the hash must distinguish them.
        let base = "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let a = checkout_dir_name(&format!("{base}/one"));
        let b = checkout_dir_name(&format!("{base}/two"));
        assert_ne!(a, b);
        // Prefixes before the 16-hex hash should match.
        let pa = a.rsplit_once('-').unwrap().0;
        let pb = b.rsplit_once('-').unwrap().0;
        assert_eq!(pa, pb);
    }

    #[test]
    fn gc_removes_only_stale_dirs() {
        let tmp = TempDir::new().unwrap();
        let stale = tmp.path().join("stale-checkout");
        let fresh = tmp.path().join("fresh-checkout");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&fresh).unwrap();

        let now = SystemTime::now();
        let stale_time = now - Duration::from_secs(2 * 3600);
        fs::File::open(&stale)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stale_time))
            .unwrap();
        fs::File::open(&fresh)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now))
            .unwrap();

        let removed = gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn unique_checkout_dirs_share_stem() {
        let stem = checkout_dir_name("https://github.com/org/repo.git");
        let a = unique_checkout_dir_name("https://github.com/org/repo.git");
        assert!(a.starts_with(&stem));
        assert!(a.contains("-d"));
    }

    #[test]
    fn unique_checkout_names_differ_with_equal_timestamps() {
        let repo = "https://github.com/org/repo.git";
        let a = unique_checkout_dir_name_with(repo, 42, 10);
        let b = unique_checkout_dir_name_with(repo, 42, 11);
        assert_ne!(a, b);
    }

    #[test]
    fn concurrent_checkout_names_are_unique() {
        let repo = "https://github.com/org/repo.git";
        let names: HashSet<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| unique_checkout_dir_name(repo)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(names.len(), 16);
    }

    #[test]
    fn checkout_leases_are_ref_counted() {
        let tmp = TempDir::new().unwrap();
        let checkout = tmp.path().join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        fs::File::open(&checkout)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(2 * 3600)),
            )
            .unwrap();

        let first = GitClient.hold_checkout(&checkout);
        let second = first.clone();
        assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 2);
        drop(first);
        assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 1);
        assert_eq!(
            gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
            0
        );
        drop(second);
        assert!(!active_checkouts().contains_key(&checkout));
        assert_eq!(
            gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
            1
        );
    }

    #[test]
    fn checkout_destination_is_atomically_reserved() {
        let tmp = TempDir::new().unwrap();
        let repo = "https://github.com/org/repo.git";
        let a = reserve_checkout_dir(tmp.path(), repo).unwrap();
        let b = reserve_checkout_dir(tmp.path(), repo).unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir());
        assert!(b.is_dir());
    }
}
