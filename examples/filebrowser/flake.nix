{
  description = "Filebrowser — Russel deployment example";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs   = import nixpkgs { inherit system; };
    in {
      packages.${system}.default = pkgs.writeShellScriptBin "filebrowser" ''
        # Setup files dir
        mkdir -p /tmp/files
        echo "Welcome to Russel File Manager inside your MicroVM!" > /tmp/files/README.txt
        echo "This VM booted in under 2 seconds using a minimal BusyBox initramfs." >> /tmp/files/README.txt

        # Start filebrowser
        exec ${pkgs.filebrowser}/bin/filebrowser \
          --noauth \
          --address 0.0.0.0 \
          --port "$PORT" \
          --database /tmp/filebrowser.db \
          --root /tmp/files
      '';
    };
}
