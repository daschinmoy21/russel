---
title: systemd and NixOS
description: Run russel-ctrl as a user unit or a NixOS service — loopback, tokens, and Podman.
sidebar_position: 1
keywords: [systemd, nixos, service, linger, podman, user unit]
---

# systemd and NixOS

Two supported ways to run `russel-ctrl` persistently. Both keep it on loopback with fail-closed auth and `0700` state.

## What you'll need

- Release `russel-ctrl` built on the host distro (NixOS glibc binaries do not run on Debian).
- A non-root operator account (host installer refuses root).
- Rootless Podman for containers; KVM/TAP/iptables privileges for microVMs.

## Other Linux — systemd user unit (via installer)

```bash
./contrib/install.sh host
./contrib/install.sh status
```

`host` (other-Linux, user-systemd only; refuses NixOS, root, non-Linux, and system-managed `russel*.service` units):

- Installs `/usr/local/bin/russel-ctrl` (sudo when the dest dir is not writable; restores the previous binary if the restart fails).
- Installs `contrib/russel-ctrl.service` → `~/.config/systemd/user/russel-ctrl.service` (use `--force-unit` to overwrite a changed unit).
- Creates `~/.config/russel/env` (`0600`) when missing — never overwrites, never prints tokens.
- Creates `/var/lib/russel` with mode `0700` when that directory is new. An existing operator-owned directory keeps its current mode. `--take-state-ownership` takes a foreign-owned directory and sets mode `0700`.
- Enables linger (`loginctl enable-linger`) warning on headless hosts so `/run/user/$(id -u)` and the user manager survive logout.
- Restarts the active user unit and probes `/vms` (bounded). Alive means HTTP 200 with a `vms` array, or HTTP 401 with `WWW-Authenticate: Bearer realm="russel-ctrl"`.

Manual equivalent:

```bash
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
mkdir -p ~/.config/systemd/user ~/.config/russel
cp contrib/russel-ctrl.service ~/.config/systemd/user/
umask 077 && printf 'RUSSEL_API_TOKEN=%s\n' "$(openssl rand -hex 32)" > ~/.config/russel/env
sudo mkdir -p /var/lib/russel && sudo chown "$USER:" /var/lib/russel && chmod 700 /var/lib/russel
loginctl enable-linger "$(id -un)"
systemctl --user daemon-reload && systemctl --user enable --now russel-ctrl
```

Unit defaults (see `contrib/russel-ctrl.service`): loopback bind, `RUSSEL_REQUIRE_AUTH=1`, `EnvironmentFile=%h/.config/russel/env`, state dir handling, restart-on-failure. `Documentation=` points at the public install doc.

## NixOS — `services.russel`

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl"; # or services.russel.package
  services.russel.environmentFile = "/etc/russel/env"; # 0600, RUSSEL_API_TOKEN=
  # Existing login account. Linger is set. Set group, or omit it:
  # services.russel.user = "you";
  # services.russel.group = "users";
  # services.russel.createUser = false;
  # NNP on, no Podman (does not configure microVMs):
  # services.russel.rootlessPodman = false;
}
```

Module behavior:

- Defaults `127.0.0.1:7878`, `RUSSEL_REQUIRE_AUTH=1`, `/var/lib/russel` mode `0700`, warm pool off.
- `rootlessPodman` defaults true: `NoNewPrivileges` off, `/run/wrappers` on `PATH` so cap-wrapped `newuidmap` is found, `virtualisation.podman.enable` default on, linger on the service user. Set false only to keep `NoNewPrivileges` on — that does not set up microVMs.
- KVM/TAP are optional on a container-only VPS. Put Caddy/nginx/Traefik in front for split HTTPS.

Use `services.russel`, never `install.sh host`, on NixOS.

## Verify

```bash
systemctl --user status russel-ctrl
./contrib/install.sh status
russel origin && russel ps
ss -ltnp 'sport = :7878'
```

## Related

- [Installation](../getting-started/installation.md) · [Upgrades + backups](upgrades-backup.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md)
