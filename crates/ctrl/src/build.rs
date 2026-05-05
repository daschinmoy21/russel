use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub store_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct NixBuilder;

impl NixBuilder {
    pub async fn build(&self, repo_path: &Path) -> Result<BuildOutput> {
        let flake_ref = format!("path:{}", repo_path.display());
        let output = Command::new("nix")
            .arg("build")
            .arg(&flake_ref)
            .arg("--print-out-paths")
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }

        let store_path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .into();

        Ok(BuildOutput { store_path })
    }
}
