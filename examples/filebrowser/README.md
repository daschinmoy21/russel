# filebrowser

> **Demo only.** This is not a production file manager. Login is `admin` /
> `demo-only-not-for-production` unless you set `FILEBROWSER_PASSWORD`. That
> password is public. Change it before you expose anything. Auth is required.
> Do not copy `--noauth`. Do not publish the host port on `0.0.0.0`.

Auth is on. The process binds `0.0.0.0` inside the guest so host publishing works with
ordinary Podman/Docker publish (slirp/pasta target the container IP, not guest
loopback). Isolation for the demo is host-side. Russel default
`RUSSEL_PUBLISH_BIND=127.0.0.1`.

| Field | Value |
|-------|--------|
| Runtime (default) | `container` |
| Port | 8080 |
| Binary | `filebrowser` (nixpkgs wrapper in `flake.nix`) |
| Login | `admin` / `FILEBROWSER_PASSWORD` |

## Deploy

To pin a direct host port, add this to `examples/filebrowser/Russelfile.toml`:

```toml
[ingress]
port = 8081
```

Then start the control plane and deploy:

```bash
./target/debug/russel-ctrl
./target/debug/russel deploy examples/filebrowser
```

Open `http://127.0.0.1:8081/` and sign in. Destroy the service with:

```bash
./target/debug/russel destroy filebrowser
```

## Password

The wrapper hashes `FILEBROWSER_PASSWORD` with `filebrowser hash` and passes
that to `--password`. The documented demo default is
`demo-only-not-for-production` (also set in the shipped Russelfile). If the
effective password is that public string, unset or set explicitly, the wrapper
prints a warning.

For a real secret:

```bash
printf '%s' 'your-password' | russel secrets set FILEBROWSER_PASSWORD
```

In `Russelfile.toml`:

```toml
[service.env]
FILEBROWSER_PASSWORD = "secret://FILEBROWSER_PASSWORD"
```

## Dockerfile

Optional. Used by `./bench.sh` against raw podman/docker. Russel deploy builds
`flake.nix` (`pkgs.filebrowser` from nixpkgs). The image download is pinned to
the sha256 of the filebrowser v2.31.2 `linux-amd64` tarball.
