use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub store_path: PathBuf,
}

static CURRENT_SYSTEM: OnceLock<String> = OnceLock::new();
/// Detect and cache the Nix system triple (e.g. `x86_64-linux`).
/// Called once at startup; must complete before any build/deploy.
pub async fn init_current_system() -> Result<()> {
    let output = Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to detect Nix system: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let sys = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sys.is_empty() {
        anyhow::bail!("nix eval builtins.currentSystem returned empty output");
    }

    let _ = CURRENT_SYSTEM.set(sys);
    Ok(())
}

/// Return the cached system triple; panics if `init_current_system` was not called.
#[inline]
pub fn current_system() -> &'static str {
    CURRENT_SYSTEM
        .get()
        .expect("current_system() called before init_current_system() — call init_current_system() at startup")
        .as_str()
}

#[derive(Debug, Default)]
pub struct NixBuilder;

impl NixBuilder {
    pub async fn ensure_flake_exists(&self, repo_path: &Path) -> Result<()> {
        let flake_path = repo_path.join("flake.nix");
        // Use create_new for atomic check-and-create (fail if exists, no TOCTOU).
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = match opts.open(&flake_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let system = current_system();
        tracing::info!(system = %system, "No flake.nix found. Auto-detecting project type to generate a default flake.");

        let flake_content = if repo_path.join("Cargo.toml").exists() {
            tracing::info!("Detected Rust project.");
            format!(
                r#"{{
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
}}"#,
                system = system
            )
        } else if repo_path.join("go.mod").exists() {
            tracing::info!("Detected Go project.");
            format!(
                r#"{{
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
}}"#,
                system = system
            )
        } else {
            tracing::info!("Defaulting to static web server flake.");
            format!(
                r#"{{
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
}}"#,
                system = system
            )
        };

        file.write_all(flake_content.as_bytes())?;
        tracing::info!("Successfully generated default flake.nix");
        Ok(())
    }

    pub async fn build(&self, repo_path: &Path) -> Result<BuildOutput> {
        self.ensure_flake_exists(repo_path).await?;

        let system = current_system();
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
