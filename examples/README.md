# Russel examples

Self-contained apps for trying **container** (default) and **microVM** deploys.

| Example | Runtime default | Stack | Port | Notes |
|---------|-----------------|-------|------|--------|
| [basic-http](basic-http/) | container | Go + embedded static | 3000 | Main demo; `/health`; Dockerfile |
| [microvm-http](microvm-http/) | microVM | same binary as basic-http | 3000 | Needs KVM/TAP; Dockerfile for baseline |
| [hello-rust](hello-rust/) | container | pure-std Rust | 3000 | No crates.io deps; Dockerfile |
| [env-config](env-config/) | container | Go + `[service.env]` | 3000 | `secret://` demo; Dockerfile |
| [shortlink](shortlink/) | container | Go in-memory shortener | 3000 | POST / + GET /{id}; Dockerfile |
| [filebrowser](filebrowser/) | container | nixpkgs filebrowser | 8080 | Auth on; guest binds `0.0.0.0`; host publish stays loopback; demo password; Dockerfile optional |
| [static-test](static-test/) | container | Python http.server | 8000 | Static HTML; Dockerfile |
| [navidrome](navidrome/) | container | nixpkgs `navidrome` via `service.package` | 4533 | `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music`; music volume starts empty; `keep = true` |
| [vaultwarden](vaultwarden/) | container | nixpkgs `vaultwarden` via `service.package` | 8000 | `DATA_FOLDER=/data`; `keep = true` |
| [postgres](postgres/) | container | nixpkgs `postgresql` via `service.package` | 5432 | Needs a prepared `host` PGDATA under `RUSSEL_VOLUME_ROOTS` |
| [redis](redis/) | container | nixpkgs `redis` via `service.package` | 6379 | Managed `data` volume; `--dir`, `--bind 0.0.0.0`, demo password |
| [caddy](caddy/) | container | nixpkgs `caddy` via `service.package` | 8080 | Stateless `file-server`; no volume |
| [meilisearch](meilisearch/) | container | nixpkgs `meilisearch` via `service.package` | 7700 | Managed `data` volume; demo master key |

The container examples with a committed `Dockerfile` (the Go, Rust, Python, and filebrowser set) can race Russel against raw podman/docker via `./bench.sh`. The `service.package` demos build from nixpkgs and ship no Dockerfile.

## Prerequisites

```bash
nix develop
cargo build
# container-only (default for most examples):
./target/debug/russel-ctrl
# hybrid microVM + container (sudo preserves SUDO_USER for rootless podman):
# sudo -E ./target/debug/russel-ctrl
```

Rootless Podman is required for `type = "container"`. See root [README](../README.md) and [docs/examples.md](../docs/examples.md).

## Quick deploy

```bash
./target/debug/russel deploy examples/basic-http -p 8080:3000 --vm-id basic
curl http://127.0.0.1:8080/health
./target/debug/russel destroy basic
```

## Runtime switch (dual-live)

Redeploy the same `--vm-id` with a different Russelfile `type` (and matching `--runtime` if set). Russel boots a candidate generation, **Traefik `Ingress::swap`**, then tears down the old runtime. Prefer `http://<id>.russel.local` over a fixed `-p` for stable URLs.
