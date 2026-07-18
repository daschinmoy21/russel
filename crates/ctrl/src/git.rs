use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Default)]
pub struct GitClient;

impl GitClient {
    pub async fn clone_or_use_local(&self, repo: &str) -> Result<PathBuf> {
        let path = PathBuf::from(repo);
        if path.exists() {
            return Ok(path);
        }

        let checkout_root = PathBuf::from("/tmp/russel/checkouts");
        fs::create_dir_all(&checkout_root).context("failed to create checkout directory")?;

        if repo.starts_with('-') {
            anyhow::bail!("repository URL cannot start with '-' (looks like a git option)");
        }

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
