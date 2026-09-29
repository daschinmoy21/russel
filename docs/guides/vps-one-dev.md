---
title: Single-VPS checklist
description: Run Russel on one VPS as its only operator, deploy apps from your laptop, and put them on the internet.
sidebar_position: 1
keywords: [vps, checklist, single operator, containers, host, connect, status]
---

# Single-VPS checklist

This is the path Russel v0.1 is built for: **one person, one Linux VPS**, apps running as rootless containers, deployed from a laptop over SSH. Tick the boxes as you go.

Russel trusts whoever holds the API token completely. It is not built for sharing a server between people or teams.

## 1. The server

- [ ] Linux x86_64 with systemd; Debian 12 or Ubuntu 22.04+ is easiest. No KVM needed.
- [ ] We suggest 2 GB of RAM and 20 GB of disk or more. Nix builds and the Nix store take most of the disk.
- [ ] An ordinary login user with `sudo`. The installer adds a separate `russel` account that runs everything; your user only needs `sudo` to install.
- [ ] Firewall: allow SSH, plus 80 and 443 if you'll serve web apps. **Never open 7878**, the control plane port.

## 2. Install

- [ ] Follow [Installation](../getting-started/installation.md) steps 1 and 2 on the server: Podman, Nix, `install.sh check`, then `sudo install.sh host`.
- [ ] Log out and back in, so your user's new `russel` group membership applies.
- [ ] Check it's running:

  ```bash
  systemctl status russel-ctrl
  ```

## 3. Connect from your laptop

- [ ] Install the CLI on the laptop, copy the token, and open the tunnel:

  ```bash
  umask 077 && mkdir -p ~/.config/russel
  scp user@vps:/etc/russel/env ~/.config/russel/env
  ./contrib/install.sh connect user@vps
  russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
  russel ps
  ```

- [ ] Open `http://127.0.0.1:7878/` on the laptop for the dashboard, and paste the token into Settings.

Rather use HTTPS than a tunnel? See [TLS reverse proxy](tls-reverse-proxy.md).

## 4. Deploy from git

A remote control plane can't see your laptop's files, so deploy from a git URL. The server clones and builds it:

```bash
russel apply https://github.com/you/app.git
russel ps
russel logs app
```

These commands assume the app's Russelfile says `name = "app"`. For a private repo, use an `ssh://` or `git@host:path` URL, and put a read-only deploy key for it in `/var/lib/russel/.ssh/` (owned by `russel`, mode `0600`). That is the home directory of the account that clones.

Leave `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` off on a VPS you deploy to remotely.

## 5. Put apps on the internet

Russel publishes each app on a `127.0.0.1` port on the server, so nothing is public until you choose. Pick one:

- [ ] **Traefik (recommended for web apps).** Russel writes a Traefik route for each service, so `app.example.com` reaches the right app. Traefik can also get TLS certificates from Let's Encrypt. See [Traefik ingress](traefik-ingress.md).
- [ ] **Your own reverse proxy.** Pin the port with `[ingress]` and `port = 8080` in the Russelfile, and point Caddy or nginx at `127.0.0.1:8080`.

## 6. Secrets and data

- [ ] Store secrets on the server, never in the repo:

  ```bash
  printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD
  ```

  Then reference them in the Russelfile as `DB_PASSWORD = "secret://DB_PASSWORD"` under `[service.env]`. See [Env + secrets](env-secrets.md).
- [ ] Keep app data in `[[volumes]]` so it survives redeploys. See the [Russelfile reference](../reference/russelfile.md).

## 7. Backups and upgrades

- [ ] Back up `/etc/russel/env` (your token) and `/var/lib/russel` (secrets, volumes, deployment history). Both need `sudo` to read.
- [ ] Upgrade by re-running the installer with a new `RUSSEL_VERSION`. See [Installation: Upgrade](../getting-started/installation.md#upgrade) and [Upgrades + backups](../operations/upgrades-backup.md).

## Good first apps

Every example except `microvm-http` runs as a container on a no-KVM VPS. Russel builds from the **root** of a git repo, so to deploy an example from git, copy its folder into a repo of its own first:

```bash
cp -r russel/examples/hello-rust hello-rust && cd hello-rust
git init && git add . && git commit -m "hello-rust"
git remote add origin git@github.com:you/hello-rust.git && git push -u origin HEAD
russel apply https://github.com/you/hello-rust.git
```

| Example | What it is |
|---|---|
| `hello-rust` | Minimal Rust HTTP server |
| `basic-http` | Go server with `/health` and static files |
| `shortlink` | In-memory URL shortener |
| `env-config` | Shows env vars and `secret://` |
| `redis`, `postgres` | Databases from nixpkgs, with a data volume |
| `vaultwarden`, `navidrome` | Real self-hosted apps from nixpkgs |

All of them: [Examples](../reference/examples.md).

## What v0.1 doesn't do

- Several users or teams on one server
- More than one server
- Managed databases: run Postgres or Redis as ordinary services instead
- TLS in the control plane itself: use SSH or a reverse proxy
- Guaranteed restart after a server reboot. The control plane comes back by itself, but containers may not. Check `russel ps` after a reboot, and run `russel apply --force` for anything that is down. This is tracked in #450.
