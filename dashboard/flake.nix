{
  description = "Russel Web Dashboard development environment (Astro + Bun)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
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
            pkgs = import nixpkgs { inherit system; };
          }
        );
    in
    {
      devShells = forEachSupportedSystem (
        { pkgs }: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              bun
            ];

            shellHook = ''
              echo "Russel Dashboard dev environment loaded (bun $(bun --version))"
              echo "Use system chromium for visual QA/screenshots (/run/current-system/sw/bin/chromium)"
            '';
          };
        }
      );
    };
}
