{
  description = "hello-rust — pure-std Rust HTTP example for Russel";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in {
      # Single-file std-only binary — avoid cargoHash/lock churn.
      packages.${system}.default = pkgs.stdenv.mkDerivation {
        pname = "hello-rust";
        version = "0.1.0";
        src = ./.;
        nativeBuildInputs = [ pkgs.rustc ];
        buildPhase = ''
          runHook preBuild
          rustc -C opt-level=2 -o hello-rust src/main.rs
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out/bin
          install -m755 hello-rust $out/bin/hello-rust
          runHook postInstall
        '';
      };

      devShells.${system}.default = pkgs.mkShell {
        buildInputs = [ pkgs.rustc pkgs.cargo pkgs.rustfmt ];
      };
    };
}
