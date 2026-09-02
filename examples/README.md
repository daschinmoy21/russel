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

All examples ship a `Dockerfile` so `./bench.sh` can race Russel against raw podman/docker.

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
