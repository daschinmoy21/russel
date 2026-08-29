{
  description = "Filebrowser — Russel deployment example";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs   = import nixpkgs { inherit system; };
      fb     = "${pkgs.filebrowser}/bin/filebrowser";
    in {
      # Russel deploy path: nixpkgs filebrowser, not the optional Dockerfile.
      packages.${system}.default = pkgs.writeShellScriptBin "filebrowser" ''
        mkdir -p /tmp/files
        echo "Welcome to Russel File Manager inside your MicroVM!" > /tmp/files/README.txt
        echo "This VM booted in under 2 seconds using a minimal BusyBox initramfs." >> /tmp/files/README.txt

        # Warn whenever the effective password is the public demo string,
        # including when FILEBROWSER_PASSWORD is set explicitly.
        demo_password="demo-only-not-for-production"
        password="''${FILEBROWSER_PASSWORD:-$demo_password}"
        if [ "$password" = "$demo_password" ]; then
          echo "WARNING: demo login admin / demo-only-not-for-production. Not for production. Set FILEBROWSER_PASSWORD (Russelfile: secret://FILEBROWSER_PASSWORD)." >&2
        fi

        hash="$(${fb} hash "$password")"

        # Guest 0.0.0.0 is required for ordinary Podman/Docker -p (slirp/pasta
        # hit the container IP, not guest loopback). Host isolation is
        # RUSSEL_PUBLISH_BIND (default 127.0.0.1). Do not copy --noauth.
        exec ${fb} \
          --address 0.0.0.0 \
          --port "''${PORT:-8080}" \
          --database /tmp/filebrowser.db \
          --root /tmp/files \
          --username admin \
          --password "$hash"
      '';
    };
}
