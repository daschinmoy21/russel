{
  description = "Russel basic HTTP test app";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = f:
        nixpkgs.lib.genAttrs supportedSystems (system:
          f (import nixpkgs { inherit system; }));
    in {
      packages = forEachSystem (pkgs: {
        default = pkgs.buildGoModule {
          pname = "russel-basic-http";
          version = "0.1.0";
          src = ./.;
          vendorHash = null;

          postInstall = ''
            mv $out/bin/basic-http $out/bin/api
          '';
        };
      });
    };
}
