---
title: Env and secrets
description: Set environment variables in the Russelfile and keep secrets on the server, out of your repo.
sidebar_position: 4
keywords: [env, environment variables, secrets, secret store, reserved keys]
---

Environment variables go in the Russelfile under `[service.env]`. Secrets are stored on the server and referenced from the Russelfile as `secret://NAME`, so the repo never contains them.

## Env vars

```toml
[service.env]
LOG_LEVEL = "info"
FEATURE_X = "1"
```

Russel checks these when it loads the Russelfile, before any build, and again after filling in secrets.

### Env rules

- Names start with a letter or `_`, followed by letters, digits, or `_`.
- At most 64 variables. Each value is at most 4096 bytes, on one line.
- These names are reserved and rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`.

Russel sets `PORT` to `service.port` for you. `VM_IP`, `HOST_IP`, and `APP` are set inside microVMs.

## Secrets

Store a secret by piping its value in. It never appears in your shell history or on a command line:

```bash
printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD
russel secrets list            # names only; values are never shown
russel secrets delete OLD_KEY
```

Use it in any env value:

```toml
[service.env]
DB_PASSWORD = "secret://DB_PASSWORD"
```

Russel fills in the value at deploy time. If the secret doesn't exist, the deploy fails.

Secret names use letters, digits, `_`, and `-`, up to 64 characters. They're stored in `/var/lib/russel/secrets/`, one file per secret with mode `0600`.

### Changing a secret

Set the new value, then rebuild the running version so it picks it up:

```bash
printf '%s' "$NEW_PASSWORD" | russel secrets set DB_PASSWORD
russel update my-app
```

## Who can see what

| Runtime | Plain env vars | `secret://` values |
|---|---|---|
| Container | Visible to `podman inspect` as the `russel` account | Passed as Podman secrets. Not visible to `podman inspect`, never on a command line, removed with the container. |
| microVM | Written to a private config file shared read-only with the VM | Same as plain vars |

Either way, put anything sensitive behind `secret://`.

## Example

`examples/env-config` reads two plain variables and one secret:

```toml
[service]
name = "env-config"
source = "."
port = 3000
memory = "128mb"

[service.env]
LOG_LEVEL = "info"
GREETING = "hello"
DEMO_SECRET = "secret://DEMO_SECRET"
```

```bash
printf '%s' "not-a-real-secret" | russel secrets set DEMO_SECRET
russel deploy https://github.com/daschinmoy21/russel.git --config examples/env-config/Russelfile.toml
russel ps                                # find its host port
curl http://127.0.0.1:<host-port>/
```

## Related

- [First deploy](../getting-started/first-deploy.md) · [Russelfile](../reference/russelfile.md) · [API: secrets](../reference/api.md#endpoints) · [Security overview](../security/overview.md)
