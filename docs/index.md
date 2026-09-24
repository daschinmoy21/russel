---
title: Russel documentation
description: Self-hosted platform for deploying Nix-built services as microVMs or rootless containers.
sidebar_position: 1
slug: /
keywords: [russel, nix, microvm, cloud-hypervisor, podman, self-hosted, deployment]
---

# Russel documentation

Russel is a self-hosted platform for deploying Nix-built services as **microVMs** (Cloud Hypervisor + KVM) or **Russel containers** (rootless Podman `--rootfs`). You push a repo with a `Russelfile.toml`; Russel builds it with Nix and runs the resulting `/nix/store` closure. No registries, no image builds, no YAML DSL.

```bash
russel init --type container
russel deploy . -p 8080:3000 --vm-id api
curl http://127.0.0.1:8080/health
# → ok
```

## When to use Russel

| Use Russel when | Skip Russel when |
|---|---|
| You want one Linux host to run trusted services from source | You need multi-tenant isolation or autoscaling |
| You like Nix closures and want them live in ~1–2 s | You need managed databases (Russel has none) |
| You have a cheap no-KVM VPS and want rootless containers | You need native TLS in the control plane (use a proxy) |
| You want Traefik `Host()` routing without writing Dockerfiles | You want hosted CI/CD (Russel is deploy-from-source) |

## How Russel works

1. **Resolve** — Clone the repo (or use a trusted local path) and load `Russelfile.toml`.
2. **Build** — `nix build` the flake's `packages.<system>.default` output. Missing `flake.nix` is auto-generated (Rust / Go / static).
3. **Run** — Branch on `service.type`:
   - `microvm` (default): minimal initramfs → TAP + `socat` + `virtiofsd` → Cloud Hypervisor → app execs from virtiofs `/nix/store`.
   - `container`: minimal rootfs → rootless `podman --rootfs` + `/nix/store:ro` bind → `-p HOST:GUEST`.
4. **Route** — Register with the `Ingress` trait. Default `TraefikFileIngress` writes a dynamic file; Traefik routes `Host(<id>.<domain>)` to the backend. `-p` is an escape hatch.

See [Concepts: Architecture](concepts/architecture.md) for the full pipeline, [Concepts: Runtimes](concepts/runtimes.md) for the isolation tradeoff, and [Concepts: Networking](concepts/networking.md) for TAP/subnet/ports.

## Choose your path

| I want to… | Start here |
|---|---|
| Deploy in 5 minutes on this machine | [Quickstart](quickstart.md) |
| Install the client + control plane (same host, SSH tunnel, or HTTPS) | [Getting started: Installation](getting-started/installation.md) |
| Run one VPS as a single operator | [Guides: Single-VPS checklist](guides/vps-one-dev.md) |
| Expose the API or apps over TLS | [Guides: TLS reverse proxy](guides/tls-reverse-proxy.md) · [Guides: Traefik ingress](guides/traefik-ingress.md) |
| Write a `Russelfile.toml` | [Reference: Russelfile](reference/russelfile.md) · `russel init` in [Reference: CLI](reference/cli.md) |
| Call the API directly | [Reference: API](reference/api.md) |
| Harden a host | [Security: Overview](security/overview.md) · [Security: Nix builds](security/nix-builds.md) |
| Operate upgrades, backups, systemd/NixOS | [Operations](operations/systemd-nixos.md) |

## Product surface

| Binary | Runs on | Job |
|---|---|---|
| `russel` | Laptop or same-host client (crate `russel-cli`) | Deploy, inspect, `login`, `ps`, logs |
| `russel-ctrl` | Linux control-plane host | Build + run microVMs/containers, HTTP API `:7878` |
| `russel-agent` | Multi-host nodes (experimental) | Node-local lifecycle RPC (heartbeat + stop/destroy/status proxies) |

Control-plane API defaults to `http://127.0.0.1:7878`. Auth is `RUSSEL_API_TOKEN` (≥32 chars) or `russel login`. The control plane is HTTP-only; terminate TLS at Caddy/nginx/Traefik or use the installer-managed SSH tunnel.

## Docs map

This site follows Cloudflare/Tailscale conventions: **Getting started** (do this now), **Concepts** (how it works), **Guides** (do this task), **Reference** (exact flags/fields/codes), **Security**, **Operations**.

```text
docs/
├── index.md                  # this page
├── quickstart.md             # 5-minute local deploy
├── getting-started/          # installation · first deploy · dashboard
├── concepts/                 # architecture · runtimes · networking · lifecycle · builds
├── guides/                   # vps checklist · tls · traefik · env/secrets · rollback · troubleshooting · benchmarks
├── reference/                # cli · api · russelfile · environment · examples
├── security/                 # overview · nix builds
├── operations/               # systemd + nixos · upgrades + backups
└── project/                  # development (contributing) · releases (changelog)
```

The live contract is `reference/` plus the code (`crates/core/src/config.rs`, `crates/cli`, `crates/ctrl/src/api/router.rs`).

## Next steps

- [Quickstart: deploy `examples/basic-http` locally](quickstart.md)
- [Installation topologies A/B/C](getting-started/installation.md)
- [Russelfile reference](reference/russelfile.md)
