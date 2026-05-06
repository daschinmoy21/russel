use std::path::Path;
use russel_core::config::Russelfile;
use crate::network::SubnetAllocation;

/// Generate a self-contained `deploy.nix` that imports the project's `flake.nix`
/// and defines a NixOS system for the MicroVM.
pub fn generate_deploy_flake(
    service_id: &str,
    _repo_path: &Path,
    config: &Russelfile,
    alloc: &SubnetAllocation,
    guest_port: u16,
    app_store_path: &Path,
    nixpkgs_path: &str,
) -> String {
    let mem_mb = config.service.memory.as_mebibytes();
    let service_name = &config.service.name;
    let host_ip = &alloc.host_ip;
    let vm_ip = &alloc.vm_ip;
    let tap_id = &alloc.tap_id;
    let mac = &alloc.mac;
    let bin_name = config.service.bin_name();
    let app_store = app_store_path.display();

    format!(r#"{{
  description = "Russel deployment: {service_id}";

  inputs = {{
    nixpkgs.url = "path:{nixpkgs_path}";
    microvm = {{
      url = "path:/home/crimxnhaze/russel-dev/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    }};
    app-pkg = {{
      url = "path:{app_store}";
      flake = false;
    }};
  }};

  outputs = {{ self, nixpkgs, microvm, app-pkg }}:
    let
      system = "x86_64-linux";
    in {{
      nixosConfigurations.{service_id} = nixpkgs.lib.nixosSystem {{
        inherit system;
        modules = [
          microvm.nixosModules.microvm
          ({{ config, lib, pkgs, ... }}: {{
            system.stateVersion = config.system.nixos.release;
            networking.hostName = "{service_id}";

            # ── Standard Fast Boot ──────────────────────────────────────────
            boot.kernelParams = [
              "quiet" "loglevel=3"
              "systemd.show_status=false"
              "rd.udev.log_level=3"
              "panic=-1"
              "random.trust_cpu=on"
              "console=ttyS0"
            ];
            
            # Disable documentation to save closure size
            documentation.enable = false;

            # ── Networking ──────────────────────────────────────────────────
            networking.useNetworkd = true;
            networking.useDHCP = false;
            networking.firewall.enable = false;
            networking.usePredictableInterfaceNames = false;

            systemd.network = {{
              enable = true;
              wait-online.enable = false;
              networks."10-eth" = {{
                matchConfig.Name = "eth0";
                networkConfig = {{
                  Address = "{vm_ip}/30";
                  Gateway = "{host_ip}";
                  IPv6AcceptRA = false;
                }};
                linkConfig.RequiredForOnline = "no";
              }};
            }};

            # ── microVM hardware ────────────────────────────────────────────
            microvm = {{
              hypervisor = "cloud-hypervisor";
              mem = {mem_mb};
              vcpu = 1;
              vsock.cid = 100;
              graphics.enable = false;
              interfaces = [{{
                type = "tap";
                id = "{tap_id}";
                mac = "{mac}";
              }}];
              shares = [{{
                tag = "ro-store";
                source = "/nix/store";
                mountPoint = "/nix/.ro-store";
                proto = "virtiofs";
              }}];
            }};

            # ── Application ─────────────────────────────────────────────────
            systemd.services.russel-app = {{
              description = "{service_name}";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              serviceConfig = {{
                ExecStart = "${{app-pkg}}/bin/{bin_name}";
                Environment = "PORT={guest_port}";
                Restart = "always";
                RestartSec = "1s";
                StandardOutput = "journal+console";
              }};
            }};
          }})
        ];
      }};
    }};
}}
"#)
}

#[derive(Debug, Clone, Copy)]
pub struct MicrovmRunner;

impl MicrovmRunner {
    pub async fn create(
        &self,
        service_id: &str,
        repo_path: &Path,
        config: &Russelfile,
        alloc: &SubnetAllocation,
        guest_port: u16,
        app_store_path: &Path,
    ) -> anyhow::Result<()> {
        // Hard cleanup to bypass "already exists" errors
        let _ = self.destroy(service_id).await;

        let deploy_dir = std::path::PathBuf::from(format!("/var/lib/russel/{}", service_id));
        std::fs::create_dir_all(&deploy_dir)?;
        
        let nixpkgs_output = tokio::process::Command::new("nix-instantiate")
            .args(["--eval", "-E", "(import <nixpkgs> {}).path"])
            .output().await?;
        
        let nixpkgs_path = String::from_utf8_lossy(&nixpkgs_output.stdout)
            .trim()
            .trim_matches('"')
            .to_string();

        if nixpkgs_path.is_empty() {
            anyhow::bail!("failed to resolve <nixpkgs> path");
        }

        let deploy_nix = generate_deploy_flake(
            service_id, 
            repo_path, 
            config, 
            alloc, 
            guest_port, 
            app_store_path,
            &nixpkgs_path
        );
        std::fs::write(deploy_dir.join("flake.nix"), deploy_nix)?;

        let flake_ref = format!("path:{}", deploy_dir.display());
        tracing::info!(service_id, flake_ref, "registering microvm (nix evaluation)");
        
        let status = tokio::process::Command::new("microvm")
            .args(["-c", service_id, "-f", &flake_ref])
            .env("NIX_CONFIG", "experimental-features = nix-command flakes")
            .spawn()?
            .wait()
            .await?;

        if !status.success() {
            anyhow::bail!("microvm create failed for {}", service_id);
        }
        Ok(())
    }

    pub async fn start(&self, service_id: &str) -> anyhow::Result<StartedMicrovm> {
        let unit = format!("microvm@{}.service", service_id);
        let _ = tokio::process::Command::new("systemctl").args(["enable", &unit]).output().await;
        
        let mut child = tokio::process::Command::new("systemctl")
            .args(["start", &unit])
            .spawn()?;
        
        child.wait().await?;
        Ok(StartedMicrovm { child })
    }

    pub async fn stop(&self, service_id: &str) -> anyhow::Result<()> {
        let unit = format!("microvm@{}.service", service_id);
        let _ = tokio::process::Command::new("systemctl").args(["stop", &unit]).output().await;
        Ok(())
    }

    pub async fn destroy(&self, service_id: &str) -> anyhow::Result<()> {
        let _ = self.stop(service_id).await;
        // Manual cleanup of the microvm state directory and GC roots
        let _ = tokio::process::Command::new("rm").args(["-rf", &format!("/var/lib/microvms/{}", service_id)]).output().await;
        let _ = tokio::process::Command::new("rm").args(["-rf", &format!("/var/lib/russel/{}", service_id)]).output().await;
        let _ = tokio::process::Command::new("rm").args(["-f", &format!("/nix/var/nix/gcroots/microvm/{}", service_id)]).output().await;
        let _ = tokio::process::Command::new("rm").args(["-f", &format!("/nix/var/nix/gcroots/microvm/booted-{}", service_id)]).output().await;
        Ok(())
    }

    pub async fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut vms = Vec::new();
        let state_dir = Path::new("/var/lib/microvms");
        if state_dir.exists() {
            if let Ok(mut entries) = tokio::fs::read_dir(state_dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if entry.file_type().await?.is_dir() {
                        if let Some(name) = entry.file_name().to_str() {
                            vms.push(name.to_string());
                        }
                    }
                }
            }
        }
        Ok(vms)
    }
}

pub struct StartedMicrovm {
    pub child: tokio::process::Child,
}
