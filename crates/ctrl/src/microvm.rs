use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
};

use russel_core::config::Russelfile;
use tokio::process::{Child, Command};

#[derive(Debug, Clone)]
pub struct GeneratedMicrovmConfig {
    pub path: PathBuf,
    pub contents: String,
    pub flake_dir: PathBuf,
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
        let repo_root = std::env::current_dir()?;
        let microvm_input = repo_root.join("microvm.nix");
        let flake_dir = PathBuf::from(format!("/tmp/russel/microvms/{service_id}"));
        let log_path = flake_dir.join("console.log");
        let contents = flake_template()
            .replace("%MICROVM_INPUT%", &microvm_input.display().to_string())
            .replace("%STORE_PATH%", &store_path.display().to_string())
            .replace("%SERVICE_ID%", service_id)
            .replace(
                "%MEMORY%",
                &config.service.memory.as_mebibytes().to_string(),
            )
            .replace("%HOST_PORT%", &host_port.to_string())
            .replace("%GUEST_PORT%", &guest_port.to_string())
            .replace("%BINARY_NAME%", &config.service.name);

        Ok(GeneratedMicrovmConfig {
            path: flake_dir.join("flake.nix"),
            contents,
            flake_dir,
            log_path,
        })
    }
}

impl GeneratedMicrovmConfig {
    pub fn persist(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.flake_dir)?;
        fs::write(&self.path, &self.contents)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct StartedMicrovm {
    pub runner_path: PathBuf,
    pub child: Child,
}

#[derive(Debug, Default)]
pub struct MicrovmRunner;

impl MicrovmRunner {
    pub async fn build_runner(&self, config: &GeneratedMicrovmConfig) -> anyhow::Result<PathBuf> {
        let flake_ref = format!("path:{}#default", config.flake_dir.display());
        let output = Command::new("nix")
            .arg("build")
            .arg(&flake_ref)
            .arg("--print-out-paths")
            .arg("--no-link")
            .arg("--impure")
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }

        let runner_path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .into();

        Ok(runner_path)
    }

    pub async fn start(
        &self,
        config: &GeneratedMicrovmConfig,
        runner_path: PathBuf,
    ) -> anyhow::Result<StartedMicrovm> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.log_path)?;
        let log_err = log.try_clone()?;
        let runner = runner_path.join("bin/microvm-run");

        let child = Command::new(&runner)
            .current_dir(&config.flake_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()?;

        Ok(StartedMicrovm { runner_path, child })
    }
}

fn flake_template() -> &'static str {
    r#"{
  description = "Russel generated microVM";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    microvm = {
      url = "path:%MICROVM_INPUT%";
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
