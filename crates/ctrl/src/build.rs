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
        // Build the default package from the project flake.
        // We try `#packages.x86_64-linux.default` first for explicitness; if that
        // attribute doesn't exist nix will error and we surface the message clearly.
        let flake_ref = format!("path:{}#packages.x86_64-linux.default", repo_path.display());
        let output = Command::new("nix")
            .arg("build")
            .arg(&flake_ref)
            .arg("--no-link")
            .arg("--print-out-paths")
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "nix build failed for {}:\n{}",
                flake_ref,
                String::from_utf8_lossy(&output.stderr).trim()
            );
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
