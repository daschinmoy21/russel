use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub store_path: PathBuf,
}

static CURRENT_SYSTEM: OnceLock<String> = OnceLock::new();

pub async fn current_system() -> String {
    if let Some(sys) = CURRENT_SYSTEM.get() {
        return sys.clone();
    }
    let out = Command::new("nix")
        .args(["eval", "--impure", "--raw", "--expr", "builtins.currentSystem"])
        .output()
        .await;
    let sys = match out {
        Ok(output) if output.status.success() => {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !s.is_empty() {
                s
            } else {
                "x86_64-linux".to_string()
            }
        }
        _ => "x86_64-linux".to_string(),
    };
    let _ = CURRENT_SYSTEM.set(sys.clone());
    sys
}

#[derive(Debug, Default)]
pub struct NixBuilder;

impl NixBuilder {
    pub async fn ensure_flake_exists(&self, repo_path: &Path) -> Result<()> {
        let flake_path = repo_path.join("flake.nix");
        if flake_path.exists() {
            return Ok(());
        }

        let system = current_system().await;
        tracing::info!(system = %system, "No flake.nix found. Auto-detecting project type to generate a default flake.");

        let flake_content = if repo_path.join("Cargo.toml").exists() {
            tracing::info!("Detected Rust project.");
            format!(r#"{{
  description = "Auto-generated Rust flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = {{ self, nixpkgs }}:
    let
      pkgs = nixpkgs.legacyPackages.{system};
    in {{
      packages.{system}.default = pkgs.rustPlatform.buildRustPackage {{
        pname = "app";
        version = "0.1.0";
        src = ./.;
        cargoLock = {{
          lockFile = ./Cargo.lock;
        }};
      }};
    }};
}}"#, system = system)
        } else if repo_path.join("go.mod").exists() {
            tracing::info!("Detected Go project.");
            format!(r#"{{
  description = "Auto-generated Go flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = {{ self, nixpkgs }}:
    let
      pkgs = nixpkgs.legacyPackages.{system};
    in {{
      packages.{system}.default = pkgs.buildGoModule {{
        pname = "app";
        version = "0.1.0";
        src = ./.;
        vendorHash = null;
      }};
    }};
}}"#, system = system)
        } else {
            tracing::info!("Defaulting to static web server flake.");
            format!(r#"{{
  description = "Auto-generated Static site flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = {{ self, nixpkgs }}:
    let
      pkgs = nixpkgs.legacyPackages.{system};
    in {{
      packages.{system}.default = pkgs.writeShellScriptBin "app" ''
        cd ${{./.}}
        exec ${{pkgs.python3}}/bin/python3 -m http.server "$PORT"
      '';
    }};
}}"#, system = system)
        };

        std::fs::write(&flake_path, flake_content)?;
        tracing::info!("Successfully generated default flake.nix");
        Ok(())
    }

    pub async fn build(&self, repo_path: &Path) -> Result<BuildOutput> {
        self.ensure_flake_exists(repo_path).await?;

        let system = current_system().await;
        let flake_ref = format!("path:{}#packages.{}.default", repo_path.display(), system);
        let mut output = Command::new("nix")
            .arg("build")
            .arg(&flake_ref)
            .arg("--no-link")
            .arg("--print-out-paths")
            .stderr(std::process::Stdio::inherit())
            .output()
            .await?;

        if !output.status.success() {
            let fallback_ref = format!("path:{}#defaultPackage.{}", repo_path.display(), system);
            tracing::info!(system = %system, "packages.{}.default failed, trying defaultPackage", system);
            output = Command::new("nix")
                .arg("build")
                .arg(&fallback_ref)
                .arg("--no-link")
                .arg("--print-out-paths")
                .stderr(std::process::Stdio::piped())
                .output()
                .await?;

            if !output.status.success() {
                anyhow::bail!(
                    "nix build failed for both {} and {}:\n{}",
                    flake_ref,
                    fallback_ref,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
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
