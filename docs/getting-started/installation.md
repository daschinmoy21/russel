---
title: Installation
description: Install russel and russel-ctrl and pick a topology — same host, SSH tunnel, or HTTPS.
sidebar_position: 1
keywords: [install, topology, ssh tunnel, https, caddy, nginx, nixos, systemd]
---

# Installation

You install two binaries. The release path downloads prebuilt x86_64 Linux binaries and needs no checkout and no compiler. `.deb`, OCI, and cross-architecture packages do not exist yet.

| Binary | Where it runs | Job |
|---|---|---|
| `russel` | Laptop or same-host client (crate `russel-cli`) | Deploy, `ps`, logs, `login` |
| `russel-ctrl` | Linux control-plane host | Build + run microVMs/containers, HTTP API |

When you build from source, build each binary on the OS where it will run. A `russel-ctrl` linked against NixOS glibc will not start on Debian (`required file not found`). The published release binaries have the same constraint; see [glibc, NixOS, and other libc](#glibc-nixos-and-other-libc).

## What you'll need

- Control-plane host: writable `/var/lib/russel` (mode `0700`), rootless Podman for containers; Nix with flakes, KVM + TAP + `cloud-hypervisor` + `virtiofsd` + `socat` + `ip` + `iptables` for microVMs.
- A Rust toolchain only on machines where you build from source (rustup, or `nix develop` in the checkout).
- A 32+ char `RUSSEL_API_TOKEN` for any non-loopback or production use (the host installer generates one for you).

## Get Russel

Install the published release without a checkout or a compiler:

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | RUSSEL_VERSION=v0.1.0 bash -s -- cli
```

Swap `cli` for `ctrl`, `all`, or `host`. The installer downloads `russel`, `russel-ctrl`, and the dashboard `dist` tarball, then checks every asset against the release `SHA256SUMS` before it installs anything. A missing `RUSSEL_VERSION` with no local build is an error, not a silent fallback to "latest". `RUSSEL_RELEASE_BASE` repoints the artifact base URL at a mirror or an internal file server; the default is `https://github.com/daschinmoy21/russel/releases/download`.

The same script is safe to run from a checkout. With a local `target/release` it copies the binaries instead of downloading, unless `RUSSEL_VERSION` is set.

### glibc, NixOS, and other libc

The release binaries are linked against glibc 2.35 (Ubuntu 22.04 builders). They run on newer glibc (Ubuntu 24.04, Debian 12) and fail to start on older ones, on NixOS, and on musl.

- **NixOS:** use `services.russel`, never the release binary and never `install.sh host`. The module builds the control plane from your pinned nixpkgs. For microVMs, put the release `bzImage` at `/var/lib/russel/_pool/kernel/bzImage` or set `RUSSEL_KERNEL_PATH`.
- **Alpine or another musl distribution:** build from source on that machine.
- **Older glibc than 2.35 (Ubuntu 20.04, Debian 11):** build from source on that machine.

### Build from source

Clone the public mirror and build once per machine that will run a binary:

```bash
git clone https://github.com/daschinmoy21/russel
cd russel
cargo build --release -p russel-cli -p russel-ctrl
# → target/release/russel
# → target/release/russel-ctrl
```

Then install from that checkout (staying in the repo root):

```bash
./contrib/install.sh cli    # → ~/.local/bin/russel
./contrib/install.sh ctrl   # → /usr/local/bin/russel-ctrl (sudo if needed)
./contrib/install.sh all    # both
```

> **Note:** contributor setup (dev shell, debug builds, `cargo test`, lint) lives in [Contributing](../project/development.md). Everything below is the operator path.

`install.sh` also manages the non-NixOS host lifecycle:

```bash
./contrib/install.sh [--force-unit] [--take-state-ownership] host   # user-systemd install (refuses NixOS/root)
./contrib/install.sh connect user@host                              # anchored SSH forward
./contrib/install.sh status                                         # endpoint, listener, unit, CLI origin
```

`host` installs `/usr/local/bin/russel-ctrl`, copies the dashboard dist to `/usr/local/share/russel/dashboard`, installs `contrib/russel-ctrl.service` as a user unit, and creates `~/.config/russel/env` (mode `0600`) when missing. A new `/var/lib/russel` is created with mode `0700`; an existing operator-owned directory keeps its current mode. `--take-state-ownership` takes a foreign-owned directory and sets mode `0700`. Restart failure restores the previous binary. Token values are never printed. `connect` opens the anchored forward `127.0.0.1:7878:127.0.0.1:7878` with `BatchMode` + `ExitOnForwardFailure`. `status` probes `/vms` with a bounded timeout. A Russel endpoint is HTTP 200 with a `vms` array, or HTTP 401 with `WWW-Authenticate: Bearer realm="russel-ctrl"`. A bare 401 from some other listener is not Russel.

### microVM kernel

`host` probes `/dev/kvm` before it touches the kernel pool. On a KVM host it downloads the release `russel-kernel-<version>-x86_64.bzImage` (a required release asset), verifies it against `SHA256SUMS`, and installs it at `/var/lib/russel/_pool/kernel/bzImage`. A missing kernel asset fails the install. The pool directories are chowned to the operator, the same account that owns `/var/lib/russel`, so ctrl reads and rewrites its own cache instead of inheriting root-owned `0700` directories. Private GitHub downloads can pass `RUSSEL_GITHUB_TOKEN` (or `GH_TOKEN` / `GITHUB_TOKEN`).

Without `/dev/kvm` the install prints a containers-only skip and leaves the pool alone. `all` never fetches the kernel; only `host` does. To use a locally built kernel instead, `nix build .#microvm-kernel` and export `RUSSEL_KERNEL_PATH` to `result/bzImage` before starting ctrl.


## Choose a topology

| Topology | Control plane | Client and dashboard |
|---|---|---|
| **A. Same host** | `russel-ctrl` on loopback | `russel` on the same host; open `http://127.0.0.1:7878/`; Settings `/api` |
| **B. Split + SSH** | `russel-ctrl` on loopback | Anchored SSH forward; CLI + browser on the laptop to `http://127.0.0.1:7878/`; Settings `/api` |
| **C. Split + HTTPS** | `russel-ctrl` on loopback behind a TLS proxy | CLI + browser use the same HTTPS origin; Settings `/api` |

Recommended transports are loopback HTTP (same host), the anchored SSH forward from `install.sh connect`, or HTTPS at the reverse proxy. **Do not bind the control plane to a public or private network address.**

### Topology A — same host

On the Linux host, from your checkout:

```bash
./contrib/install.sh cli
./contrib/install.sh host
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

Dashboard on the same host: open `http://127.0.0.1:7878/` and set Settings to `/api`. `russel-ctrl` serves the UI itself. Paste the token from `~/.config/russel/env` into Settings.

### Topology B — split + SSH tunnel

On the control-plane host:

```bash
./contrib/install.sh host
```

On the laptop, install the client and copy the env file privately:

```bash
./contrib/install.sh cli
umask 077 && mkdir -p ~/.config/russel
scp user@host:~/.config/russel/env ~/.config/russel/env
chmod 600 ~/.config/russel/env
./contrib/install.sh connect user@host
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

Keep the background SSH forward up while using the CLI or dashboard. Do not replace it with a hand-rolled forward command. On the laptop open `http://127.0.0.1:7878/` (Settings `/api`); the tunnel carries both the UI and the API.

Manual equivalent (must stay loopback-anchored and fail if the forward fails):

```bash
ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:7878:127.0.0.1:7878 user@host
```

### Topology C — split + HTTPS

On the control-plane host, keep the unit on loopback (`./contrib/install.sh host`). Put Caddy or nginx in front of `127.0.0.1:7878`. The proxy must terminate TLS, strip `/api` before forwarding to the control plane, and leave deploy NDJSON unbuffered. Full configs: [TLS reverse proxy](../guides/tls-reverse-proxy.md).

On the laptop:

```bash
umask 077 && mkdir -p ~/.config/russel
scp user@host:~/.config/russel/env ~/.config/russel/env
chmod 600 ~/.config/russel/env
./contrib/install.sh cli
russel login https://russel.example.com --token-file ~/.config/russel/env
russel origin
russel ps
```

The reverse proxy can forward `/` and `/api/` to loopback ctrl (ctrl serves the dashboard and nests the API under `/api`). Stripping `/api` and serving `dashboard/dist` yourself still works. In Settings use `/api`. Enter the dashboard token there; it stays in tab-scoped `sessionStorage`. CLI login does not populate the dashboard.

## Auth quick reference

```bash
russel login [<url>] [--token-file PATH]   # token from file, env, or stdin
russel logout
russel origin                              # which ctrl this CLI will hit
```

- `RUSSEL_API_TOKEN` (env) wins over `~/.config/russel/config.toml` (mode `0600`).
- `login --token-file` accepts a bare token or a `RUSSEL_API_TOKEN=…` line.
- **Fish:** Fish does not `export` `KEY=VALUE` files. Do not `source` an env file. Use `russel login … --token-file ~/.config/russel/env`.

- The CLI **refuses** to send the token over `http://` to a non-loopback host. Override only with `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1` (not recommended). Prefer HTTPS.

## NixOS

NixOS hosts use `services.russel`, not `install.sh host`:

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl"; # or services.russel.package
  services.russel.environmentFile = "/etc/russel/env"; # 0600, RUSSEL_API_TOKEN=
}
```

The module defaults to `127.0.0.1:7878`, `RUSSEL_REQUIRE_AUTH=1`, `/var/lib/russel` mode `0700`, warm pool off, and rootless-Podman integration on. Put a TLS proxy in front for split HTTPS access. Details: [Operations: systemd + NixOS](../operations/systemd-nixos.md).

## Other Linux (systemd user unit)

Copy `contrib/russel-ctrl.service` to `~/.config/systemd/user/`, write `~/.config/russel/env` (mode `0600`) with `RUSSEL_API_TOKEN`, `chown` `/var/lib/russel` to yourself (mode `0700`), then `systemctl --user enable --now russel-ctrl`. Or run `./contrib/install.sh host`, which does all of this.

## Deploy from a remote client

Use a git URL when the control plane is remote — a laptop filesystem path is **not** uploaded:

```bash
russel deploy https://github.com/your-account/app.git --vm-id app -p 8080:3000 --runtime container
```

A local absolute path is resolved on the **control-plane host** (must already exist there) and only when the control plane sets `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1`. Keep `--config` repository-relative. See [First deploy](first-deploy.md).

## Upgrade and inspect

Other Linux, release download (the installer keeps env + state and restarts the user unit):

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | RUSSEL_VERSION=v0.2.0 bash -s -- host
./contrib/install.sh status
```

Pinned versions matter here. Re-running with the same `RUSSEL_VERSION` is a no-op; a new tag replaces the binary and leaves the token and `/var/lib/russel` in place.

Other Linux, from an updated checkout:

```bash
cd russel && git pull
cargo build --release -p russel-cli -p russel-ctrl
./contrib/install.sh host
./contrib/install.sh status
```

Direct binary replacement:

```bash
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
systemctl --user restart russel-ctrl
./contrib/install.sh status
```

Never delete `~/.config/russel/env` or `/var/lib/russel` on upgrade. NixOS upgrades use normal module activation. Full runbook: [Upgrades + backups](../operations/upgrades-backup.md).

## Verify

```bash
russel origin   # url, auth, reachable
russel ps       # list workloads (aliases: list, vms)
./contrib/install.sh status
```

Typical VPS has no KVM — set `type = "container"` in every Russelfile (`russel init --type container`). Next: [First deploy](first-deploy.md) · [Single-VPS checklist](../guides/vps-one-dev.md).

## Related

- [First deploy](first-deploy.md) · [Dashboard](dashboard.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md) · [systemd + NixOS](../operations/systemd-nixos.md) · [CLI reference](../reference/cli.md)
