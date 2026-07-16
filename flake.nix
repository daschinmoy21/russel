{
  description = "A simple Rust development environment";

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
