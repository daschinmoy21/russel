{
  description = "env-config — Russelfile env + secrets demo for Russel";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in {
      packages.${system}.default = pkgs.buildGoModule {
        pname = "env-config";
        version = "0.1.0";
        src = ./.;
        vendorHash = null;
      };
    };
}
