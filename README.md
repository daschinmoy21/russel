# Russel

Self-hosted platform for deploying Nix-built services as
[Cloud Hypervisor](https://www.cloudhypervisor.org/) **microVMs** or
**rootless Podman** containers.

```
russel-cli  ──HTTP──►  russel-ctrl  ──►  nix build  ──►  microVM | container
```

## Quick start

Linux host, [Nix](https://nixos.org/download/) with flakes, writable `/var/lib/russel`.
Containers need rootless Podman. MicroVMs need KVM, TAP, and iptables.

```bash
nix develop
cargo build

export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"
export RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1   # local examples only
./target/debug/russel-ctrl                # http://127.0.0.1:7878

# other terminal — same token
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id test-api
curl http://127.0.0.1:8080/health
./target/debug/russel-cli destroy test-api
```

Scaffold an app with `russel init` (`--type container` on a typical VPS, `--with-flake` to write `flake.nix`).

Remote control planes should get a **git URL**, not a laptop path. Bind off-loopback only with a ≥32-character `RUSSEL_API_TOKEN`, and terminate TLS in front of ctrl — see [docs/security-tls.md](docs/security-tls.md).

## Docs

| Doc | What it covers |
|-----|----------------|
| [Architecture](docs/architecture.md) | Control plane, runtimes, networking |
| [API & CLI](docs/api.md) | HTTP endpoints, auth, `russel-cli` |
| [Russelfile](docs/russelfile.md) | Service config reference |
| [Deployment](docs/deployment.md) | Packaging an app for Russel |
| [Examples](docs/examples.md) | Sample services in `examples/` |
| [Traefik](docs/traefik.md) | HTTP ingress (`<id>.russel.local`) |
| [TLS](docs/security-tls.md) | Reverse-proxy TLS in front of ctrl |
| [Nix build security](docs/security/nix-builds.md) | What a `flake.nix` can do |
| [One-dev VPS](docs/vps-one-dev.md) | Single-operator remote setup |
| [Flake auto-generation](docs/auto-generation.md) | When `flake.nix` is missing |
| [Contributing](docs/contributing.md) | Checks, PRs, coding standards |
| [Dashboard](dashboard/README.md) | Web UI |

## Layout

| Path | Role |
|------|------|
| `crates/core` | Shared types (`Russelfile`, API) |
| `crates/cli` | `russel-cli` |
| `crates/ctrl` | `russel-ctrl` control plane |
| `crates/agent` | `russel-agent` (multi-host lifecycle RPC) |
| `dashboard/` | Static web UI |
| `examples/` | Deployable sample apps |
| `docs/` | Operator and contributor docs |

## License

Copyright 2026 Chinmoy Das

Licensed under the [Apache License, Version 2.0](LICENSE).
