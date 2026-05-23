{
  description = "Auto-generated Static site flake by Russel";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      packages.x86_64-linux.default = pkgs.writeShellScriptBin "app" ''
        cd ${./.}
        exec ${pkgs.python3}/bin/python3 -m http.server "$PORT"
      '';
    };
}