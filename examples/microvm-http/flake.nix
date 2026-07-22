{
  description = "microvm-http — basic-http packaged for microVM runtime demos";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # Reuse the basic-http package so we do not duplicate Go sources.
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      basic = pkgs.buildGoModule {
        pname = "basic-http";
        version = "0.1.0";
        src = ../basic-http;
        vendorHash = null;
      };
    in {
      packages.${system}.default = basic;
    };
}
