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
    pub fn ensure_flake_exists(&self, repo_path: &Path) -> Result<()> {
        let flake_path = repo_path.join("flake.nix");
        if flake_path.exists() {
            return Ok(());
        }

        tracing::info!("No flake.nix found. Auto-detecting project type to generate a default flake.");

        let flake_content = if repo_path.join("Cargo.toml").exists() {
            tracing::info!("Detected Rust project.");
            r#"{
  description = "Auto-generated Rust flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      packages.x86_64-linux.default = pkgs.rustPlatform.buildRustPackage {
        pname = "app";
        version = "0.1.0";
        src = ./.;
        cargoLock = {
          lockFile = ./Cargo.lock;
        };
      };
    };
}"#
        } else if repo_path.join("go.mod").exists() {
            tracing::info!("Detected Go project.");
            r#"{
  description = "Auto-generated Go flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      packages.x86_64-linux.default = pkgs.buildGoModule {
        pname = "app";
        version = "0.1.0";
        src = ./.;
        vendorHash = null;
      };
    };
}"#
        } else {
            tracing::info!("Defaulting to static web server flake.");
            r#"{
  description = "Auto-generated Static site flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      packages.x86_64-linux.default = pkgs.writeShellScriptBin "app" ''
        cd ${./.}
        exec ${pkgs.python3}/bin/python3 -m http.server "$PORT"
      '';
    };
}"#
        };

        std::fs::write(&flake_path, flake_content)?;
        tracing::info!("Successfully generated default flake.nix");
        Ok(())
    }

    pub async fn build(&self, repo_path: &Path) -> Result<BuildOutput> {
        self.ensure_flake_exists(repo_path)?;

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
