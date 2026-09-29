---
title: Env and secrets
description: Configure services with env maps and the host secret store.
sidebar_position: 4
keywords: [env, secrets, secret store, env-file, reserved keys]
---

Environment values are declared in the Russelfile under `[service.env]`. Secrets are stored on the host and referenced as `secret://NAME`.

## Env rules (enforced)

Loading the Russelfile checks `[service.env]`, so a bad key fails before any build. The control plane re-checks the map after resolving secrets.

- Key: `^[A-Za-z_][A-Za-z0-9_]*$`, max 64 keys.
- Reserved keys rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`.
- Value: no NUL / `\n` / `\r`, max 4096 B.
- `service.name` uses the service id rule (`[A-Za-z0-9_-]`, max 128). `service.bin` allows the wider `[A-Za-z0-9._+-]`, max 256; it is injected via shell-quoted `deploy.env`.

```toml
[service.env]
LOG_LEVEL = "info"
FEATURE_X = "1"
```

```bash
russel deploy .
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
| Container | Plain values: `podman -e`. `secret://` values: a Podman secret `russel-<id>.<KEY>` (created over stdin, injected with `--secret ...,type=env`) | Plain values show in `podman inspect`; secret values do not, and never reach argv. Podman keeps them in its secret store (`0600`, podman user). Removed with the container. |

`russel-agent` lifetime RPCs forward the same env contract; agent env passthrough is documented in the agent crate.

## Example

See `examples/env-config/Russelfile.toml`:

```toml
[service]
name = "env-config"
source = "."
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
russel deploy examples/env-config
curl http://127.0.0.1:8080/
```

## Related

- [First deploy](../getting-started/first-deploy.md) · [Russelfile](../reference/russelfile.md) · [API](../reference/api.md) · [Security overview](../security/overview.md)
