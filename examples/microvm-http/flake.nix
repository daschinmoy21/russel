{
  description = "microvm-http — basic-http packaged for microVM runtime demos";

  # Self-contained sources (copied from basic-http). Do not use src = ../basic-http:
  # flake evaluation cannot reach parent paths outside the flake root.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

      app = pkgs.buildGoModule {
        pname = "basic-http";
        version = "0.1.0";
        src = ./.;
        vendorHash = null;
      };
    in {
      packages.${system}.default = app;

      devShells.${system}.default = pkgs.mkShell {
        buildInputs = [ pkgs.go pkgs.gopls pkgs.gotools ];
      };
    };
}
