---
title: Upgrades and backups
description: Upgrade Russel without losing your token, secrets, or history, and back up the files that matter.
sidebar_position: 2
keywords: [upgrade, backup, restore, state, secrets, deployments]
---

An upgrade replaces the `russel-ctrl` binary and restarts the service. Your apps keep running through it, and your token and state stay where they are.

## What to back up

| Path | Holds |
|---|---|
| `/etc/russel/env` | The API token and any control-plane settings |
| `/var/lib/russel/` | Secrets, volumes, deployment history, and each service's metadata and logs |
| `~/.config/russel/config.toml` | The CLI's saved login, on each machine you use it from |

The Nix store doesn't need a backup: Russel rebuilds anything missing on the next deploy, just more slowly.

```bash
sudo tar -czf russel-backup-$(date +%F).tar.gz /etc/russel/env /var/lib/russel
sudo chmod 600 russel-backup-*.tar.gz
```

The archive contains your secrets, so store it somewhere private. Restore it as root with `sudo tar -xzpf … -C /`, which keeps file owners, including the rootless Podman ids inside volumes.

## Upgrade: Debian, Ubuntu, and other systemd distributions

Run the installer again with the new version:

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh \
  | sudo RUSSEL_VERSION=v0.1.1 bash -s -- host
```

It keeps `/etc/russel/env` and `/var/lib/russel`, replaces the binary, and restarts the service. If the new version doesn't come up, it puts the old binary back. Upgrade the CLI on each machine the same way, with `cli` in place of `host` and without `sudo`.

From source:

```bash
cd russel && git pull
cargo build --release -p russel-cli -p russel-ctrl
sudo ./contrib/install.sh host
./contrib/install.sh cli
```

Upgrading from v0.1.0 also replaces its service unit, which stopped containers from starting. Use the installer for that upgrade, because copying the binary by hand leaves the old unit in place. See [systemd and NixOS](./systemd-nixos.md).

Check the result:

```bash
russel --version
russel ps
```

## Upgrade: NixOS

Build the new `russel-ctrl` on the machine, install it at the path `services.russel.bin` points to, and restart:

```bash
cd russel && git pull
cargo build --release -p russel-ctrl
sudo install -m 755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
sudo systemctl restart russel
```

If you set `services.russel.package` instead, update that package and run `nixos-rebuild switch`. Either way `environmentFile` and `/var/lib/russel` are left alone.

## Going back to the previous version

- **Installer:** if the new version fails to start, the installer restores the old binary for you. To go back later, run the installer again with the older `RUSSEL_VERSION`.
- **Apps:** rolling back an app is separate from rolling back Russel; see [Update and rollback](../guides/update-rollback.md).

Restarting or replacing the control plane never stops or removes your apps. On start it finds them and takes them back over.

## Related

- [systemd and NixOS](./systemd-nixos.md) · [Installation](../getting-started/installation.md#upgrade) · [Troubleshooting](../guides/troubleshooting.md)
