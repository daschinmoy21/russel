---
title: systemd and NixOS
description: Run russel-ctrl as a dedicated russel account, from the installer or the NixOS module — loopback, tokens, and Podman.
sidebar_position: 1
keywords: [systemd, nixos, service, linger, podman, system unit, russel user]
---

# systemd and NixOS

Two supported ways to run `russel-ctrl` persistently. Both keep it on loopback with fail-closed auth and `0700` state.

## What you'll need

- Release `russel-ctrl` built on the host distro (NixOS glibc binaries do not run on Debian).
- `sudo` once, to install. Both setups run russel-ctrl and every workload as a dedicated, unprivileged `russel` account.
- Rootless Podman for containers; `/dev/kvm` plus `cloud-hypervisor`, `virtiofsd`, and `passt` for microVMs.

## Other Linux — system unit as `russel` (via installer)

```bash
./contrib/install.sh check
sudo ./contrib/install.sh host
./contrib/install.sh status
```

`check` lists missing prerequisites with a fix for each and changes nothing. `host` runs the same checks first (`--skip-checks` overrides), then:

- Creates the `russel` system group and account (home `/var/lib/russel`, no login shell), adds a subuid/subgid range after every existing one, and enables linger so `user@<uid>.service` gives it `/run/user/<uid>` and a user bus from boot.
- Adds the invoking `sudo` user to the `russel` group, and adds `russel` to `kvm` when `/dev/kvm` exists.
- Creates `/etc/russel` (`root:russel`, `0750`) and `/etc/russel/env` (`root:russel`, `0640`) when missing. Never overwrites, never prints tokens. An old per-user `~/.config/russel/env` token is carried over.
- Creates `/var/lib/russel` (`russel:russel`, `0700`). A directory owned by anyone else is refused unless `--take-state-ownership`, which hands the whole tree to `russel`.
- Renders `contrib/russel-ctrl.service` with russel's uid into `/etc/systemd/system/russel-ctrl.service` (`--force-unit` overwrites a changed unit).
- Installs `/usr/local/bin/russel-ctrl`, keeping the previous binary; a failed restart puts it back.
- Enables or restarts the unit and probes `/vms` (bounded). Alive means HTTP 200 with a `vms` array, or HTTP 401 with `WWW-Authenticate: Bearer realm="russel-ctrl"`.

It refuses NixOS, non-Linux, a `russel.service` from the NixOS module, a Nix-store unit, and an old per-user install (it prints the steps to move over).

Manual equivalent (Debian/Ubuntu):

```bash
sudo groupadd --system russel
sudo useradd --system --gid russel --home-dir /var/lib/russel --no-create-home --shell /usr/sbin/nologin russel
sudo usermod --add-subuids 200000-265535 --add-subgids 200000-265535 russel   # a range no one else uses
sudo loginctl enable-linger russel
sudo usermod --append --groups russel "$USER"
sudo install -d -o russel -g russel -m 700 /var/lib/russel
sudo install -d -o root -g russel -m 750 /etc/russel
printf 'RUSSEL_API_TOKEN=%s\n' "$(openssl rand -hex 32)" | sudo install -o root -g russel -m 640 /dev/stdin /etc/russel/env
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
sed "s/@RUSSEL_UID@/$(id -u russel)/g" contrib/russel-ctrl.service | sudo tee /etc/systemd/system/russel-ctrl.service >/dev/null
sudo systemctl daemon-reload && sudo systemctl enable --now russel-ctrl
```

Unit defaults (see `contrib/russel-ctrl.service`): `User=russel`, loopback bind, `RUSSEL_REQUIRE_AUTH=1`, `EnvironmentFile=/etc/russel/env`, the Nix daemon profile on `PATH`, `XDG_RUNTIME_DIR` and the user bus for rootless Podman, `ProtectSystem=strict` with only `/var/lib/russel` and the account's `/run/user/<uid>` writable, restart-on-failure.

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
systemctl status russel-ctrl     # installer; on NixOS the unit is russel.service
./contrib/install.sh status
russel origin && russel ps
ss -ltnp 'sport = :7878'
```

## Related

- [Installation](../getting-started/installation.md) · [Upgrades + backups](upgrades-backup.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md)
