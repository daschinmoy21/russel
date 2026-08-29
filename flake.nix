{
  description = "Russel — microvm-based deployment platform control plane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSupportedSystem =
        f:
        nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
          }
        );
    in
    {
      # Linux/NixOS only. Darwin can import the flake; the module is a no-op
      # until services.russel.enable is set on a NixOS host.
      nixosModules.default = ./nix/modules/russel-host.nix;
      nixosModules.russel = ./nix/modules/russel-host.nix;

      checks = forEachSupportedSystem (
        { pkgs }:
        pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          russel-host-eval =
            let
              results = import ./nix/tests/russel-host-eval.nix {
                inherit (nixpkgs) lib;
                nixosSystem = nixpkgs.lib.nixosSystem;
                module = self.nixosModules.russel;
                system = pkgs.stdenv.hostPlatform.system;
              };
            in
            assert results.ok;
            pkgs.runCommand "russel-host-eval" { } "touch $out";
        }
      );

      packages = forEachSupportedSystem (
        { pkgs }:
        pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          microvm-kernel = pkgs.callPackage ./nix/microvm-kernel.nix { };
        }
      );

      devShells = forEachSupportedSystem (
        { pkgs }: {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                rust-bin.stable.latest.default
                rust-analyzer
                pkg-config
                openssl
              ]
              # nixpkgs supports cloud-hypervisor only on aarch64-linux and x86_64-linux,
              # so it must be gated to Linux to avoid Darwin evaluation failures.
              ++ (lib.optionals stdenv.isLinux [
                cloud-hypervisor
                # Rootless Podman for Russel containers (`service.type = "container"`).
                podman
              ]);

            shellHook = ''
              echo "Rust development environment loaded!"
            '';
          };
        }
      );
    };
}
