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

        let checkout = checkout_root.join(sanitize_repo_name(repo));
        let status = Command::new("git")
            .arg("clone")
            .arg(repo)
            .arg(&checkout)
            .status()
            .await?;

        if !status.success() {
            anyhow::bail!(
                "repo path was not found locally and git clone failed for {repo}; use an absolute path for local deploys"
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
