---
title: First deploy
description: Scaffold a Russelfile, deploy it, pass env and secrets, and use podman passthrough.
sidebar_position: 2
keywords: [deploy, russel init, env, secrets, podman args, update]
---

# First deploy

This guide takes a fresh app from `russel init` to a verified deploy, with env, secrets, and container passthrough.

## What you'll need

- A running control plane ([Installation](installation.md), [Quickstart](../quickstart.md)).
- `russel login` completed (`russel origin` shows the right URL and auth).

## 1. Scaffold

```bash
cd my-app
russel init                       # writes Russelfile.toml
russel init --type container      # no-KVM / typical VPS
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
type = "container"   # or "microvm" (default)
```

Full schema: [Russelfile reference](../reference/russelfile.md). If no `flake.nix` exists, the control plane auto-generates one (Rust → `Cargo.toml`, Go → `go.mod`, else static). See [Concepts: Builds](../concepts/builds.md).

## 2. Deploy

```bash
russel deploy . -p 8080:3000 --vm-id my-app --runtime container
```

| Flag | Meaning |
|---|---|
| `REPO` | `https://…`, `http://…`, `ssh://…`, `git@host:path`, or a local absolute path (control-plane opt-in only). Relative paths are always rejected. |
| `-p HOST:GUEST` | Publish a host port. Optional for HTTP apps behind Traefik; required for direct `curl` in this guide. |
| `--vm-id ID` | Service id `[A-Za-z0-9-_]` max 128. Derived from the repo when omitted. **Required by the API.** |
| `--config PATH` | Repository-relative Russelfile path (default `Russelfile.toml`). Opened via `openat` + `O_NOFOLLOW`, 1 MiB cap. Symlinks rejected. |
| `--runtime` | Must match `service.type` when both are set. Not an override — mismatch is a hard error. |
| `--env KEY=VALUE` (repeatable) | Overrides `[service.env]`. |
| `--env-file PATH` | `KEY=VALUE` file (`#` comments, blanks skipped). Merge order: file < env-file < `--env`. |
| `-- …` | Extra `podman run` args (**containers only**). Validated against an allowlist posture (see below). |

Progress streams as NDJSON (`resolve → build → create → start → ready → complete`). Redeploying an existing id kills + waits for the old generation before reusing ports.

Remote control plane? Use a git URL — local paths are resolved on the **control-plane host**, not the laptop, and need `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane (trusted single-tenant hosts only):

```bash
russel deploy https://github.com/you/app.git --vm-id app -p 8080:3000 --runtime container
```

## 3. Env and secrets

`[service.env]` plus `--env` / `--env-file` set user env. Reserved keys are rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`. Keys must match `^[A-Za-z_][A-Za-z0-9_]*$` (max 64 keys); values reject NUL/`\n`/`\r` and cap at 4096 B.

Secrets live in the host store (`/var/lib/russel/secrets/`, mode `0600`), never in the repo:

```bash
printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD
russel secrets list          # names only — values are never returned
russel secrets delete OLD_KEY
```

Reference them as `secret://NAME` in env maps — the control plane resolves the value at deploy time:

```toml
[service.env]
LOG_LEVEL = "info"
DB_PASSWORD = "secret://DB_PASSWORD"
```

```bash
russel deploy . --vm-id my-app -p 8080:3000 --runtime container --env LOG_LEVEL=debug
```

> **Note (containers):** env (including resolved secrets) is passed as `podman -e` and is visible via `podman inspect`. Prefer microVMs for secret-heavy workloads (`deploy.env` is `0600` in a `0700` dir). Full model: [Env + secrets](../guides/env-secrets.md).

## 4. Container passthrough (`-- …`)

Extra `podman run` flags go after `--` and are validated fail-closed. Russel-owned flags (`--rootfs`, `--name`, `-d`, `-p`) and isolation-weakening flags (`--privileged`, `--cap-add`, `--device`, host namespaces, non-store bind mounts, `--env-file`, `--entrypoint`) are rejected:

```bash
russel deploy . --vm-id my-app -p 8080:3000 --runtime container -- -v /data:/data:ro --network bridge
```

## 5. Verify and inspect

```bash
curl http://127.0.0.1:8080/health
russel status my-app
russel logs my-app
russel ps
```

`status`/`logs` accept an id or (single-service hosts only) no id via the `/status` + `/logs` shims. With multiple services those shims return `400` — pass `/vm/{id}/…`.

## 6. Update

Re-apply desired state from the `repo_url` / `config_path` recorded at the last successful deploy:

```bash
russel update my-app
russel update my-app --repo https://github.com/you/app.git --config Russelfile.toml
```

Same NDJSON stream as `deploy`. Details + rollback: [Update and rollback](../guides/update-rollback.md).

## 7. Stop / destroy

```bash
russel stop my-app      # stop workload, keep metadata/history
russel destroy my-app   # stop + remove Traefik file, ports, rootfs/TAP, metadata
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `relative paths are rejected` | Bare `.` on a remote ctrl without opt-in | Use a git URL; enable `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` only on trusted hosts |
| `--runtime mismatch` | Flag disagrees with `service.type` | Align them; flag is a check, not an override |
| `invalid env key` | Reserved key or bad charset | Rename; see reserved list above |
| `secret not found` | `secret://NAME` with no host secret | `russel secrets set NAME` on the control-plane host |
| `passthrough rejected` | Blocked podman flag | Drop the flag or use a supported mount/network form |

More: [Troubleshooting](../guides/troubleshooting.md).

## Next steps

- [Dashboard](dashboard.md) — inspect the same deploy in the UI.
- [Russelfile reference](../reference/russelfile.md) — every field + validation rule.
- [API reference](../reference/api.md) — drive deploys from scripts.
