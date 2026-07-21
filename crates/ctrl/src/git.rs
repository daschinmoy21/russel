use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Default)]
pub struct GitClient;

impl GitClient {
    pub async fn clone_or_use_local(&self, repo: &str) -> Result<PathBuf> {
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
            return Ok(path);
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

        // Unique dir per URL: content hash avoids sanitize collisions (#121 / #38).
        let checkout = checkout_root.join(checkout_dir_name(repo));
        if checkout.exists() {
            // Replace existing checkout for this URL (idempotent redeploy).
            let _ = fs::remove_dir_all(&checkout);
        }

        let output = Command::new("git")
            .arg("clone")
            .arg("--")
            .arg(repo)
            .arg(&checkout)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git clone failed for {repo}: {}.\nUse an absolute path for local deploys.",
                stderr.trim()
            );
        }

        Ok(checkout)
    }
}

/// Stable, unique directory name for a remote repo URL.
///
/// Format: `{sanitized_prefix}-{16 hex of FNV-1a of full URL}`.
fn checkout_dir_name(repo: &str) -> String {
    let hash = repo.bytes().fold(2_166_136_261u32, |acc, b| {
        acc.wrapping_mul(16_777_619) ^ b as u32
    });
    let prefix: String = repo
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    let prefix = prefix.trim_matches('-');
    if prefix.is_empty() {
        format!("repo-{hash:08x}")
    } else {
        format!("{prefix}-{hash:08x}")
    }
}

/// Remove checkout directories older than `max_age`.
pub fn gc_old_checkouts(checkout_root: &Path, max_age: Duration) -> anyhow::Result<usize> {
    if !checkout_root.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in fs::read_dir(checkout_root)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_dir() {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > max_age {
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    tracing::info!(path = %entry.path().display(), "GC removed old git checkout");
                    removed += 1;
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn checkout_dir_name_is_unique_for_distinct_urls() {
        let a = checkout_dir_name("https://github.com/org/repo-a.git");
        let b = checkout_dir_name("https://github.com/org/repo-b.git");
        assert_ne!(a, b);
        // Same URL is stable.
        assert_eq!(a, checkout_dir_name("https://github.com/org/repo-a.git"));
    }

    #[test]
    fn checkout_dir_name_avoids_sanitize_collision() {
        // Previously both would become the same sanitize string.
        let a = checkout_dir_name("https://github.com/foo/bar");
        let b = checkout_dir_name("https://gitlab.com/foo/bar");
        assert_ne!(a, b);
    }

    #[test]
    fn gc_removes_old_dirs() {
        let tmp = TempDir::new().unwrap();
        let old = tmp.path().join("old-checkout");
        fs::create_dir_all(&old).unwrap();
        // Set mtime to the past via filetime is not available; touch then GC with zero age
        // would remove everything. Use max_age=0 to remove all.
        let removed = gc_old_checkouts(tmp.path(), Duration::from_secs(0)).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists());
    }
}
