---
title: Env and secrets
description: Configure services with env maps and the host secret store.
sidebar_position: 4
keywords: [env, secrets, secret store, env-file, reserved keys]
---

# Env and secrets

User config is layered: `Russelfile [service.env]` < `--env-file` < `--env`. Secrets are stored on the host and referenced as `secret://NAME`.

## Env rules (enforced)

- Key: `^[A-Za-z_][A-Za-z0-9_]*$`, max 64 keys.
- Reserved keys rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`.
- Value: no NUL / `\n` / `\r`, max 4096 B.
- Binary name (`bin` or `name`) must match `[A-Za-z0-9._+-]`, max 256 chars; injected via shell-quoted `deploy.env`.
- Merge order: Russelfile < env-file < `--env` (later wins). Env-file format is `KEY=VALUE` per line, `#` comments and blanks skipped.

```toml
[service.env]
LOG_LEVEL = "info"
FEATURE_X = "1"
```

```bash
russel deploy . --vm-id api -p 8080:3000 --env LOG_LEVEL=debug --env FEATURE_X=1
russel deploy . --vm-id api -p 8080:3000 --env-file ./prod.env
```

Injected runtime vars (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are set by Russel — do not define them.

## Secrets

Host store: `/var/lib/russel/secrets/` (dir `0700`, files `0600`, atomic create-new + `O_NOFOLLOW` + fsync + rename). Override dir with `RUSSEL_SECRETS_DIR` (tests only).

```bash
printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD   # value from stdin, never argv
russel secrets list        # names only — values are never returned
russel secrets delete OLD_KEY
```

HTTP (Bearer when configured): `GET /secrets`, `POST /secrets/{name}` (`{"value":"…"}`), `DELETE /secrets/{name}`. Validation failures → `400`; I/O failures → `500`.

Reference in any env map:

```toml
[service.env]
DB_PASSWORD = "secret://DB_PASSWORD"
```

The control plane resolves the value at deploy time and re-validates. Missing secret fails the deploy fail-closed.

## Runtime differences

| Runtime | Delivery | Visibility |
|---|---|---|
| microVM | `deploy.env` (`0600` in a `0700` dir) via read-only `russelcfg` virtiofs share | Host root + guest init only |
| Container | `podman -e` | Visible via `podman inspect` — prefer microVMs for secret-heavy workloads |

`russel-agent` lifetime RPCs forward the same env contract; agent env passthrough is documented in the agent crate.

## Example

See `examples/env-config/Russelfile.toml`:

```toml
[service]
name = "env-config"
port = 3000
memory = "128mb"
type = "container"

[service.env]
LOG_LEVEL = "info"
GREETING = "hello"
DEMO_SECRET = "secret://DEMO_SECRET"
```

```bash
printf '%s' "bench-secret" | russel secrets set DEMO_SECRET
russel deploy examples/env-config -p 8080:3000 --vm-id env-demo --runtime container
curl http://127.0.0.1:8080/
```

## Related

- [First deploy](../getting-started/first-deploy.md) · [Russelfile](../reference/russelfile.md) · [API](../reference/api.md) · [Security overview](../security/overview.md)
