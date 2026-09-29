---
title: Upgrades and backups
description: Upgrade binaries without losing tokens, state, or history — and back up what matters.
sidebar_position: 2
keywords: [upgrade, backup, state, metadata, secrets, deployments]
---

Upgrades replace binaries and restart the unit. They must never delete the env file or state dir.

## What to back up

| Path | Contains | Perms |
|---|---|---|
| `/etc/russel/env` | `RUSSEL_API_TOKEN` (+ optional ctrl env) | `0640` `root:russel` (installer), `0600` (NixOS) |
| `~/.config/russel/config.toml` | CLI login (URL + token) | `0600` |
| `/var/lib/russel/` | `metadata.json` per service, `deployments.json` history, `secrets/` store, `traefik/dynamic/`, `rootfs/`, logs | `0700` `russel:russel` (secrets `0600`) |
| `/nix/store` | Build closures (rebuilt if lost, but slow) | — |

```bash
umask 077
sudo tar -czf russel-backup-$(date +%F).tar.gz /etc/russel/env /var/lib/russel ~/.config/russel/config.toml
```

Restore as root with `tar -xzpf` so ownership (`russel`, and the rootless Podman sub-ids inside volumes) comes back as it was.

Bench scripts move/replace `/var/lib/russel` (and legacy `/var/lib/microvms`) with a `flock` run-lock — never run bench and prod ctrl on the same state dir.

## Upgrade — other Linux

Rebuild the release from an updated checkout of the public mirror, then reinstall from that checkout:

```bash
cd russel && git pull
cargo build --release -p russel-cli -p russel-ctrl
sudo ./contrib/install.sh host # replaces binary, keeps env + state, restarts unit
./contrib/install.sh status
```

Direct replacement (not from v0.1.0: that leaves its broken unit in place, so use `install.sh host` or follow [systemd and NixOS: Upgrading from v0.1.0](./systemd-nixos.md)):

```bash
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
sudo systemctl restart russel-ctrl
./contrib/install.sh status
```

The installer restores the previous binary if the restart probe fails. Verify with `russel ps` and one `russel status <id>`.

## Upgrade — NixOS

Normal module activation flow; retains the configured `environmentFile` and state dir. Bump the flake input / `services.russel.package`, `nixos-rebuild switch`, then `russel ps`.

## Rollback of the control plane

- Other Linux: the installer keeps the previous binary on failed restart; otherwise reinstall the prior `target/release/russel-ctrl` and `sudo systemctl restart russel-ctrl`.
- Workload rollback (app versions) is separate — see [Update and rollback](../guides/update-rollback.md). Ctrl restarts never destroy workloads; reconcile re-adopts them.

## Related

- [systemd and NixOS](./systemd-nixos.md) · [Update and rollback](../guides/update-rollback.md) · [Troubleshooting](../guides/troubleshooting.md)
