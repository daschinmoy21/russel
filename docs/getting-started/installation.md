---
title: Installation
description: Install the russel CLI and the russel-ctrl control plane on Linux, then connect to it from the same host or from your laptop.
sidebar_position: 1
keywords: [install, topology, ssh tunnel, https, caddy, nginx, nixos, systemd, check]
---

Russel has two programs:

| Program | Where it runs | What it does |
|---|---|---|
| `russel-ctrl` | Your Linux server (the **control plane**) | Builds apps with Nix and runs them. Serves the HTTP API and the dashboard on `127.0.0.1:7878`. |
| `russel` | Your laptop, or the server itself | The CLI: `deploy`, `update`, `ps`, `logs`, `rollback`, … |

This page gets both running. It takes about 15 minutes on a fresh server, most of it installing Nix and Podman.

> **On NixOS?** Skip to [NixOS](#nixos). The rest of this page is for Debian, Ubuntu, and other systemd distributions.

## How Russel runs

The installer creates a dedicated system account called `russel`. The control plane and every app run as that account, never as root and never as your own login user. So an app that escaped its container would find no SSH keys, no `sudo`, and none of your files.

You need `sudo` once, to install. After that, nothing runs as root.

## 1. Prepare the server

You need a Linux x86_64 server with systemd. A cheap VPS without KVM is fine: apps run as **rootless Podman containers** by default.

**Install Podman** and the pieces rootless Podman needs (Debian 12, Ubuntu 22.04 or newer):

```bash
sudo apt update
sudo apt install -y podman uidmap dbus-user-session git
```

**Install Nix** with the multi-user (daemon) installer, and turn on flakes for everyone:

```bash
sh <(curl -L https://nixos.org/nix/install) --daemon
echo 'experimental-features = nix-command flakes' | sudo tee -a /etc/nix/nix.conf
sudo systemctl restart nix-daemon
```

> **Note:** use the daemon install. The `russel` account builds through the Nix daemon; a single-user Nix install belongs to one user and can't be shared with it.

**Check the server.** The installer can tell you what's still missing, and how to fix it:

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | bash -s -- check
```

```text
Checking this host for Russel:
  ok    Linux with systemd
  ok    cgroup v2
  ok    podman 4.3.1
  ok    newuidmap and newgidmap (rootless Podman)
  FAIL  nix flakes are not enabled for all users
        echo 'experimental-features = nix-command flakes' | sudo tee -a /etc/nix/nix.conf
        sudo systemctl restart nix-daemon
  ...
1 problem(s) must be fixed before installing.
```

Fix anything marked `FAIL`. Lines marked `warn` are about optional features like microVMs. From a checkout, `./contrib/install.sh check` does the same.

## 2. Install Russel on the server

Pick **one** of these. Both run the same checks first and stop if something is missing.

### Option A: download a release

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | sudo RUSSEL_VERSION=v0.1.1 bash -s -- host
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | RUSSEL_VERSION=v0.1.1 bash -s -- cli
```

The installer downloads the binaries and checks each one against the release `SHA256SUMS` before installing anything. You must name a version; there is no "latest".

The release binaries need glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+). On older distributions, on musl (Alpine), or on NixOS, build from source instead.

### Option B: build from source

You need a Rust toolchain ([rustup](https://rustup.rs)).

```bash
git clone https://github.com/daschinmoy21/russel
cd russel
cargo build --release -p russel-cli -p russel-ctrl
sudo ./contrib/install.sh host
./contrib/install.sh cli
```

Build on the machine that will run the binary. A binary built on NixOS will not start on Debian.

### What `install.sh host` did

- Created the `russel` system account: no login shell, no `sudo`, home `/var/lib/russel`. It gets its own range of sub-IDs for rootless Podman, and "linger", so it runs from boot without anyone logged in.
- Created `/var/lib/russel` (mode `0700`, owned by `russel`). All Russel state lives here.
- Generated an API token in `/etc/russel/env` (owner `root`, group `russel`, mode `0640`). The token is never printed.
- Added **you** (the user who ran `sudo`) to the `russel` group, so you can read the token.
- Installed `russel-ctrl` to `/usr/local/bin` and the dashboard to `/usr/local/share/russel/dashboard`.
- Installed and started the system service `russel-ctrl`, running as `russel` and listening on `127.0.0.1:7878` only, with authentication required.

`install.sh cli` installed `russel` to `~/.local/bin`. Make sure that directory is on your `PATH`.

> **Log out and back in** (or run `newgrp russel`) before the next step. Group membership only applies to new logins.

## 3. Connect the CLI

The control plane only listens on `127.0.0.1`. **Never expose port 7878 to the network.** Reach it in one of three ways.

### Same server

```bash
russel login http://127.0.0.1:7878 --token-file /etc/russel/env
russel origin
russel ps
```

`russel origin` shows which control plane the CLI talks to and whether it is reachable. `russel ps` shows an empty table on a fresh install.

### From your laptop, over SSH (recommended)

Install the CLI on the laptop (Option A with `cli`, or Option B's `install.sh cli`). Then copy the token and open a tunnel:

```bash
umask 077 && mkdir -p ~/.config/russel
scp user@server:/etc/russel/env ~/.config/russel/env
./contrib/install.sh connect user@server
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel ps
```

`connect` starts a background SSH forward from the laptop's `127.0.0.1:7878` to the server's. It fails loudly if the forward cannot be set up. Keep it running while you use the CLI or dashboard. If you prefer to run it by hand:

```bash
ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:7878:127.0.0.1:7878 user@server
```

### From anywhere, over HTTPS

Put Caddy or nginx in front of `127.0.0.1:7878` on the server, then log in with the HTTPS URL:

```bash
russel login https://russel.example.com --token-file ~/.config/russel/env
```

The CLI refuses to send the token over plain `http://` to anything but loopback. Proxy configs: [TLS reverse proxy](../guides/tls-reverse-proxy.md).

> **Fish shell:** don't `source` the env file; fish can't read `KEY=VALUE` files. `--token-file` works in every shell.

## 4. Open the dashboard

Open `http://127.0.0.1:7878/` (through the tunnel from a laptop, or your HTTPS URL). Paste the token from `/etc/russel/env` into **Settings**. The dashboard keeps it for that browser tab only; `russel login` does not fill it in. More: [Dashboard](./dashboard.md).

## 5. Check the install

On the server:

```bash
systemctl status russel-ctrl
./contrib/install.sh status      # from a checkout, or pipe the script with `status`
```

The control plane's log is `/var/lib/russel/ctrl.log`; reading it needs `sudo`, because only `russel` can open that directory. `journalctl -u russel-ctrl` shows start-up errors.

You're ready: [Quickstart](../quickstart.md) deploys an example app in a few minutes.

## Optional: microVMs (experimental)

Containers are the default and what most servers should use. MicroVMs give each app its own kernel through Cloud Hypervisor. They need hardware virtualization, which most cheap VPSes don't expose.

You need `cloud-hypervisor` **v52 or newer** (older versions hang when `cpus` > 1), `virtiofsd`, and `passt` on `PATH`. `install.sh check` warns about each missing one.

When `/dev/kvm` exists, `install.sh host` adds `russel` to the `kvm` group and installs the release microVM kernel at `/var/lib/russel/_pool/kernel/bzImage`. Nothing else needs root. Without `/dev/kvm`, the installer prints "containers only" and moves on. To use your own kernel, build `nix build .#microvm-kernel` and set `RUSSEL_KERNEL_PATH` to `result/bzImage` in `/etc/russel/env`.

## NixOS

Use the `services.russel` module, not `install.sh host` and not the release binaries. Build `russel-ctrl` from source on the NixOS machine (Option B, `cargo build` only), install it, and point the module at it:

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl"; # or services.russel.package
  services.russel.environmentFile = "/etc/russel/env"; # mode 0600, contains RUSSEL_API_TOKEN=...
}
```

It works the same way as the installer: a dedicated `russel` account, `127.0.0.1:7878`, authentication required, `/var/lib/russel` at mode `0700`, rootless Podman. The service is `russel.service` (`systemctl status russel`). For microVMs add `services.russel.microvms.enable = true;`. Details: [systemd + NixOS](../operations/systemd-nixos.md).

## Upgrade

Run the same install command with the new version. Your token and `/var/lib/russel` are kept, and the service restarts:

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | sudo RUSSEL_VERSION=v0.2.0 bash -s -- host
```

From source: `git pull`, rebuild, and run `sudo ./contrib/install.sh host` again. If the new binary fails to start, the installer puts the old one back.

Upgrading from v0.1.0 also replaces its service unit, whose `PrivateTmp=yes` stopped containers from starting, and resets rootless Podman for the `russel` account. Drop-ins in `/etc/systemd/system/russel-ctrl.service.d/` are kept. Details: [systemd and NixOS: Upgrading from v0.1.0](../operations/systemd-nixos.md).

Never delete `/etc/russel/env` or `/var/lib/russel` when upgrading. Backups: [Upgrades + backups](../operations/upgrades-backup.md).

### Moving from an older per-user install

Earlier builds of `install.sh host` ran russel-ctrl as your own user, from `~/.config/systemd/user`. The new installer finds that setup and stops with the steps to move over:

1. `russel ps`, then `russel destroy` each service. The new `russel` account can't take over containers that your own Podman started.
2. `systemctl --user disable --now russel-ctrl`, then delete `~/.config/systemd/user/russel-ctrl.service`.
3. `sudo ./contrib/install.sh --take-state-ownership host`. This gives `/var/lib/russel` (secrets, history, volumes) to `russel`, and keeps the token from `~/.config/russel/env`, so your existing `russel login` keeps working.

Then `russel deploy` your services again.

## Installer reference

| Command | What it does |
|---|---|
| `install.sh check` | Lists what the server is missing, with a fix for each. Changes nothing; no `sudo` needed. |
| `sudo install.sh host` | Server install described above. Runs `check` first. Refuses NixOS. |
| `install.sh cli` | Installs `russel` to `~/.local/bin`. |
| `install.sh ctrl` | Installs only the `russel-ctrl` binary, no service. |
| `install.sh all` | `cli` + `ctrl`. |
| `install.sh connect user@server` | Opens the SSH forward to the server's control plane. |
| `install.sh status` | Shows the endpoint, the listener, the service, and the CLI's origin. |

| Flag or variable | Meaning |
|---|---|
| `RUSSEL_VERSION` | Release to download, such as `v0.1.1`. Required unless you run the script from a checkout with a `target/release` build. |
| `RUSSEL_RELEASE_BASE` | Download from a mirror instead of GitHub Releases. |
| `--take-state-ownership` | Give an existing `/var/lib/russel` owned by another user (and everything in it) to `russel`. |
| `--force-unit` | Replace an existing `/etc/systemd/system/russel-ctrl.service` that differs from the shipped one. Review your changes first. |
| `--skip-checks` | Install even when `check` reports problems. |

## Related

- [Quickstart](../quickstart.md) · [First deploy](./first-deploy.md) · [Single-VPS checklist](../guides/vps-one-dev.md) · [Troubleshooting](../guides/troubleshooting.md) · [CLI reference](../reference/cli.md)
