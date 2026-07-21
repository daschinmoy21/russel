use std::{fs, path::PathBuf};

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

        let checkout = checkout_root.join(sanitize_repo_name(repo));
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

fn sanitize_repo_name(repo: &str) -> String {
    repo.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
