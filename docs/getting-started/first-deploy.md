---
title: First deploy
description: Scaffold a Russelfile, deploy it, configure env and secrets, and set container Podman arguments.
sidebar_position: 2
keywords: [deploy, russel init, env, secrets, podman args, update]
---

# First deploy

This guide takes a fresh app from `russel init` to a verified deploy, with env, secrets, and container Podman arguments.

## What you'll need

- A running control plane ([Installation](installation.md), [Quickstart](../quickstart.md)).
- `russel login` completed (`russel origin` shows the right URL and auth).

## 1. Scaffold

```bash
cd my-app
russel init                       # writes Russelfile.toml
russel init --type microvm        # experimental: KVM host + passt
russel init --with-flake          # also writes a starter flake.nix
russel init --port 3000 --memory 256mb --bin my-app --force  # overwrite
```

`init` infers the name from `Cargo.toml` / `go.mod` / directory, validates port/memory, and writes a commented manifest where every field and flag is documented inline. It refuses to overwrite without `--force`.

Minimal result:

```toml
[service]
name = "my-app"
source = "."
port = 3000
memory = "256mb"
bin = "my-app"
type = "container"   # default; or "microvm" (experimental)

[ingress]
port = 8080           # optional host-side port pin for direct access
```

Full schema: [Russelfile reference](../reference/russelfile.md). If no `flake.nix` exists, the control plane auto-generates one (Rust → `Cargo.toml`, Go → `go.mod`, else static). See [Concepts: Builds](../concepts/builds.md).

## 2. Deploy

```bash
russel apply .
```

| Flag | Meaning |
|---|---|
| `REPO` | A git URL (`https://…`, `ssh://…`, `git@host:path`) or a local folder. The CLI turns a local folder into an absolute path; the control plane accepts it only with `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1`. |
| `--config PATH` | Russelfile path inside the repo (default `Russelfile.toml`). Must not be a symlink; 1 MiB max. The build always uses the repo root. |
| `--force` | Redeploy even when the service already runs this commit and Russelfile. |

The CLI shows each phase (`resolve → build → create → start → ready`) and prints the host port when it finishes. Running `apply` again with the same commit and Russelfile does nothing. A real change starts the new version next to the old one and switches over once it is ready.

Remote control plane? Use a git URL — local paths are resolved on the **control-plane host**, not the laptop, and need `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane (trusted single-tenant hosts only):

```bash
russel apply https://github.com/you/app.git
```

The repository Russelfile supplies `service.name`, `service.type`, `[ingress]`, and `[service.env]`.

## 3. Env and secrets

`[service.env]` sets user env. Reserved keys are rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`. Keys must match `^[A-Za-z_][A-Za-z0-9_]*$` (max 64 keys); values reject NUL/`\n`/`\r` and cap at 4096 B.

Secrets live in the host store (`/var/lib/russel/secrets/`, mode `0600`), never in the repo:

```bash
printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD
russel secrets list          # names only — values are never returned
russel secrets delete OLD_KEY
```

Reference them as `secret://NAME` in env maps — the control plane resolves the value at deploy time:

```toml
[service.env]
LOG_LEVEL = "debug"
DB_PASSWORD = "secret://DB_PASSWORD"
```

```bash
russel apply .
```

> **Note (containers):** plain env is passed as `podman -e` and is visible via `podman inspect`. `secret://` values are delivered as Podman secrets, so they stay out of argv and `podman inspect`. Full model: [Env + secrets](../guides/env-secrets.md).

## 4. Process args and podman flags

These are two different things:

- **Process argv** (what your binary sees after its name) is `service.args` in the Russelfile, e.g. `args = ["--dir", "/data"]`. Works on both runtimes.
- **`podman run` flags** go in `service.podman_args`, one token per array entry (container only). They never reach the process.

`service.podman_args` is validated fail-closed. Russel-owned flags and isolation-weakening flags are rejected, and `-v` may only mount a `/nix/store` path read-only. Host directories go in `[[volumes]]` instead. For example:

```toml
[service]
type = "container"
podman_args = ["--network", "bridge", "--tmpfs", "/cache"]
```

## 5. Verify and inspect

```bash
russel ps                       # PORTS shows host port → app port
curl http://127.0.0.1:<host-port>/health
russel status my-app
russel logs my-app
```

`status` and `logs` take the service name. With only one service deployed you can leave it out.

## 6. Update

Rebuild the service from the commit and Russelfile recorded at its last deploy. Add `--refresh` to build the source's latest commit instead:

```bash
russel update my-app
russel update my-app --refresh
```

Output looks the same as `apply`. Details + rollback: [Update and rollback](../guides/update-rollback.md).

## 7. Stop / destroy

```bash
russel stop my-app      # stop workload, keep metadata/history
russel destroy my-app   # stop + remove Traefik file, ports, rootfs/TAP, metadata
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `relative paths are rejected` | Bare `.` on a remote ctrl without opt-in | Use a git URL; enable `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` only on trusted hosts |
| `invalid service type` | Unsupported `service.type` value | Set `service.type` to `microvm` or `container` |
| `invalid env key` | Reserved key or bad charset | Rename; see reserved list above |
| `secret not found` | `secret://NAME` with no host secret | `russel secrets set NAME` on the control-plane host |
| `passthrough rejected` | Blocked podman flag | Drop the flag or use a supported mount/network form |

More: [Troubleshooting](../guides/troubleshooting.md).

## Next steps

- [Dashboard](dashboard.md) — inspect the same deploy in the UI.
- [Russelfile reference](../reference/russelfile.md) — every field + validation rule.
- [API reference](../reference/api.md) — drive deploys from scripts.
