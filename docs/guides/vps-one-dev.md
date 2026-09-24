---
title: Single-VPS checklist
description: Run russel-ctrl on one VPS as a single trusted operator — containers only.
sidebar_position: 1
keywords: [vps, checklist, single operator, containers, host, connect, status]
---

# Single-VPS one-developer checklist

**Audience:** one trusted operator running `russel-ctrl` on a Linux VPS or home server, deploying with `russel` from a client.

**Scope:** container runtime on typical no-KVM VPS images. MicroVMs need `/dev/kvm` and are optional on bare metal or nested virt.

**Related:** [TLS reverse proxy](tls-reverse-proxy.md) · [Traefik ingress](traefik-ingress.md) · [App packaging](../concepts/builds.md)

## Readiness summary

| Area | Ready for one-dev VPS? | Notes |
|---|---|---|
| Core loop | **Yes** | CLI → HTTP API → Nix build → rootless Podman |
| Auth | **Yes** | `RUSSEL_API_TOKEN` ≥32 chars, `RUSSEL_REQUIRE_AUTH=1` |
| Control-plane transport | **Yes** | Loopback, `install.sh connect`, or HTTPS proxy |
| Typical cheap VPS | **Containers only** | `type = "container"` in every `Russelfile.toml` |
| NixOS host | **Yes** | `services.russel` |
| Other Linux host | **Yes** | `./contrib/install.sh host` + user-systemd |
| Dashboard | **Yes** | Vite `/api` for A/B, same-origin HTTPS for C |
| Multi-tenant / multi-user | **No** | Single trusted operator model |
| Managed databases | **No** | `[database.*]` is a placeholder |

## Operator checklist

Copy into a runbook and tick as you go.

### A. Host prerequisites

- [ ] Linux x86_64 or aarch64 matching the app flake `system`
- [ ] Nix with flakes enabled
- [ ] Rootless Podman: `podman info` reports rootless
- [ ] Persistent user manager: `loginctl enable-linger "$(id -un)"`
- [ ] Writable state dir: `/var/lib/russel`, mode `0700`, owned by the control-plane user
- [ ] Firewall allows SSH + 443 (+80 for ACME); **do not expose 7878**
- [ ] Disk for the Nix store and builds
- [ ] No-KVM path: containers only, skip microVM deps
- [ ] KVM path (if needed): `/dev/kvm`, TAP, `cloud-hypervisor`, `virtiofsd`, `socat`, `ip`, `iptables`

### B. Install the control plane and client

Produce the release once per machine from the public mirror (see [Installation](../getting-started/installation.md)), then install it — no dev setup, no debug builds:

```bash
git clone https://github.com/daschinmoy21/russel
cd russel
cargo build --release -p russel-cli -p russel-ctrl
./contrib/install.sh host
./contrib/install.sh status
```

`host` installs `/usr/local/bin/russel-ctrl` + `contrib/russel-ctrl.service` as a systemd user unit and creates `~/.config/russel/env` (mode `0600`) when missing. A new `/var/lib/russel` is created with mode `0700`; an existing operator-owned directory keeps its current mode. It refuses NixOS, root, and system-managed units. NixOS uses `services.russel` instead:

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl";
  services.russel.environmentFile = "/etc/russel/env"; # 0600, RUSSEL_API_TOKEN=
}
```

Client on the laptop or same-host client:

```bash
./contrib/install.sh cli
```

### C. Select a control-plane topology

#### A. Same host

```bash
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

Dashboard on that host: `http://127.0.0.1:7878/`, Settings `/api`.

#### B. Split + SSH tunnel

On the laptop, copy the host env privately, then open the installer-managed forward:

```bash
umask 077 && mkdir -p ~/.config/russel
scp user@host:~/.config/russel/env ~/.config/russel/env
chmod 600 ~/.config/russel/env
./contrib/install.sh connect user@host
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

Anchored to `127.0.0.1:7878:127.0.0.1:7878` (`BatchMode`, `ExitOnForwardFailure`). Keep it up for CLI/dashboard. Dashboard Settings `/api`.

#### C. Split + HTTPS

Keep `russel-ctrl` on `127.0.0.1:7878`; terminate TLS in Caddy/nginx; forward `/` and `/api/` to loopback ctrl (or serve `dist/` yourself and strip `/api`); keep NDJSON unbuffered. See [TLS reverse proxy](tls-reverse-proxy.md).

```bash
umask 077 && mkdir -p ~/.config/russel
scp user@host:~/.config/russel/env ~/.config/russel/env
chmod 600 ~/.config/russel/env
russel login https://russel.example.com --token-file ~/.config/russel/env
russel origin
russel ps
```

Dashboard Settings `/api`; token in `sessionStorage`; CLI login does not fill it.

### D. First deploy

Git URL for remote control planes (laptop paths are not uploaded; absolute local paths resolve on the **ctrl host** and need the opt-in):

```bash
russel deploy https://github.com/your-account/app.git --vm-id demo -p 8080:3000 --runtime container
russel status demo
russel logs demo
russel destroy demo
```

Keep `--config` repository-relative. Every VPS Russelfile sets `type = "container"` (`russel init --type container`).

### E. App reachability

Choose app ingress separately from the control plane:

- [ ] Publish selected app ports with `-p HOST:GUEST`, firewall only those ports
- [ ] Or use Traefik for HTTP routes and keep ctrl on loopback — see [Traefik ingress](traefik-ingress.md)

Never bind the control-plane port to a public/private network address.

### F. Operations and upgrades

- [ ] Keep `~/.config/russel/env` private (mode `0600`)
- [ ] Back up `~/.config/russel/env` + `/var/lib/russel` for token/metadata/secrets/history recovery
- [ ] Inspect with `./contrib/install.sh status`
- [ ] Keep local-path deploys disabled on a remote ctrl

Upgrade (other Linux) — rebuild the release from an updated checkout, then reinstall:

```bash
cd russel && git pull
cargo build --release -p russel-cli -p russel-ctrl
./contrib/install.sh host
./contrib/install.sh status
```

Direct replacement:

```bash
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
systemctl --user restart russel-ctrl
./contrib/install.sh status
```

Never delete env or state on upgrade. NixOS uses normal module activation. Full runbook: [Upgrades + backups](../operations/upgrades-backup.md).

## Code / product checklist (maintainers)

What must be true in-tree for the operator path above. Tick when verifying a release.

### P0 for one-dev remote use

- [x] Reserved `service_id` rejected on deploy/destroy
- [x] Non-loopback bind requires `RUSSEL_API_TOKEN`; min length ≥32 + header-safe charset
- [x] `RUSSEL_REQUIRE_AUTH` fail-closed on loopback
- [x] CLI refuses cleartext Bearer to non-loopback; TLS proxy docs
- [x] Local absolute path deploy gated (`RUSSEL_ALLOW_LOCAL_PATH_DEPLOY`)
- [x] Container path (rootless Podman) works without KVM
- [x] Secrets API + host store
- [x] Dashboard does not bake public API tokens; default bind localhost
- [x] Operator docs: this file + installation + TLS runbook
- [x] NixOS module + systemd user unit with loopback / token / `0700` defaults
- [x] `russel login` / config file (`~/.config/russel/config.toml`, `0600`)

### Still DIY / later

- [ ] deb/OCI packages
- [ ] Native TLS in `russel-ctrl` (proxy is enough for one-dev)
- [ ] Fresh-VPS e2e automated in CI
- [ ] Multi-tenant isolation, horizontal scale, managed DBs

## Example services for VPS smoke tests

Use examples with `type = "container"`:

| Example | Notes |
|---|---|
| `examples/basic-http` | Go `/health` + static assets |
| `examples/hello-rust` | Minimal Rust HTTP |
| `examples/static-test` | Static files |
| `examples/shortlink` | In-memory shortener |
| `examples/env-config` | Env + `secret://` |
| `examples/microvm-http` | **microVM only** — needs KVM |

See [Examples](../reference/examples.md).
