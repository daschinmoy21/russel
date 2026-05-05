use std::{
    fs, path::Path, path::PathBuf,
};

use russel_core::config::Russelfile;
use tokio::process::Child;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GeneratedMicrovmConfig {
    pub service_id: String,
    pub store_path: PathBuf,
    pub flake_path: PathBuf,
    pub log_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct MicrovmConfigGenerator;

impl MicrovmConfigGenerator {
    pub fn generate(
        &self,
        service_id: &str,
        config: &Russelfile,
        store_path: &Path,
        host_port: u16,
        guest_port: u16,
    ) -> anyhow::Result<GeneratedMicrovmConfig> {
        let flake_dir = PathBuf::from(format!("/tmp/russel/flakes/{service_id}"));
        let log_path = PathBuf::from(format!("/var/lib/microvms/{service_id}/console.log"));

        Ok(GeneratedMicrovmConfig {
            service_id: service_id.to_string(),
            store_path: store_path.to_path_buf(),
            flake_path: flake_dir.join("flake.nix"),
            log_path,
        })
    }
}

impl GeneratedMicrovmConfig {
    pub fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.flake_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.flake_path, self.contents())?;
        Ok(())
    }

    fn contents(&self) -> String {
        flake_template()
            .replace("%STORE_PATH%", &self.store_path.display().to_string())
            .replace("%SERVICE_ID%", &self.service_id)
            .replace("%BINARY_NAME%", &self.service_id)
            .replace("%MEMORY%", "512")
            .replace("%HOST_PORT%", "8080")
            .replace("%GUEST_PORT%", "8080")
    }
}

fn flake_template() -> &'static str {
    r#"{
  description = "Russel generated microVM";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    microvm = {
      url = "github:mic92/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, microvm }:
    let
      system = "x86_64-linux";
      app = %STORE_PATH%;
    in {
      packages.${system}.default =
        self.nixosConfigurations."%SERVICE_ID%".config.microvm.declaredRunner;

      nixosConfigurations."%SERVICE_ID%" = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          microvm.nixosModules.microvm
          ({ config, lib, pkgs, ... }: {
            system.stateVersion = lib.trivial.release;
            networking.hostName = "%SERVICE_ID%";
            networking.firewall.allowedTCPPorts = [ %GUEST_PORT% ];

            microvm = {
              hypervisor = "qemu";
              mem = %MEMORY%;
              interfaces = [
                {
                  type = "user";
                  id = "qemu";
                  mac = "02:00:00:01:01:01";
                }
              ];
              forwardPorts = [
                {
                  from = "host";
                  host.address = "127.0.0.1";
                  host.port = %HOST_PORT%;
                  guest.port = %GUEST_PORT%;
                }
              ];
              shares = [
                {
                  tag = "ro-store";
                  source = "/nix/store";
                  mountPoint = "/nix/.ro-store";
                  proto = "9p";
                }
              ];
            };

            environment.systemPackages = [ app ];

            systemd.services.russel-app = {
              wantedBy = [ "multi-user.target" ];
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              serviceConfig = {
                ExecStart = "${app}/bin/%BINARY_NAME%";
                Environment = "PORT=%GUEST_PORT%";
                DynamicUser = true;
                NoNewPrivileges = true;
                Restart = "on-failure";
              };
            };
          })
        ];
      };
    };
}
"#
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct StartedMicrovm {
    pub child: Child,
}

#[derive(Debug, Default)]
pub struct MicrovmRunner;

impl MicrovmRunner {
    pub async fn create(&self, config: &GeneratedMicrovmConfig) -> anyhow::Result<()> {
        let output = tokio::process::Command::new("sudo")
            .arg("microvm")
            .arg("-c")
            .arg(&config.service_id)
            .arg("-f")
            .arg(&config.flake_path)
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "microvm create failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(())
    }

    pub async fn start(&self, service_id: &str) -> anyhow::Result<StartedMicrovm> {
        let child = tokio::process::Command::new("sudo")
            .arg("microvm")
            .arg("-r")
            .arg(service_id)
            .spawn()?;

        Ok(StartedMicrovm { child })
    }

    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        let output = tokio::process::Command::new("sudo")
            .arg("systemctl")
            .arg("stop")
            .arg(format!("microvm@{}.service", service_id))
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to stop microvm: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(())
    }

    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        let output = tokio::process::Command::new("sudo")
            .arg("rm")
            .arg("-rf")
            .arg(format!("/var/lib/microvms/{}", service_id))
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to destroy microvm: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(())
    }

    pub async fn list(&self) -> anyhow::Result<Vec<String>> {
        let output = tokio::process::Command::new("microvm")
            .arg("-l")
            .output()
            .await?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let output = String::from_utf8_lossy(&output.stdout);
        let vms: Vec<String> = output
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.split(':').next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(vms)
    }
}