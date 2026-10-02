---
title: Russelfile reference
description: Every field in Russelfile.toml, with its default and the rules Russel checks when it loads the file.
sidebar_position: 3
keywords: [russelfile, manifest, service type, guest, memory, env, volumes, package, ports, ingress]
---

A `Russelfile.toml` describes one service. Russel checks it when it loads, before any build, and rejects unknown fields, so a typo fails loudly. `russel init` writes a commented one for you.

## Full example

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"                  # optional, defaults to name
type = "container"           # optional: "container" (default) or "microvm" (experimental)
cpus = 1                     # optional, 1 to 32
# package = "navidrome"      # run a nixpkgs package when the repo has no flake
# args = ["--loglevel", "info"]
# user = "root"
# restart = "unless-stopped"
# debug = false

[service.env]
LOG_LEVEL = "info"
DB_PASSWORD = "secret://DB_PASSWORD"

[ingress]                    # optional
host = "api.example.com"
port = 4000

# [[volumes]]          # optional: folders that persist across restarts
# name = "data"
# guest = "/data"
# rw = true
# keep = true

# [[ports]]            # optional: extra ports besides service.port
# host = 50300
# guest = 50300
```

## `[service]`

| Field | Type | Default | Rules |
|---|---|---|---|
| `name` | string | required | The service's id, used by every command. Letters, digits, `_`, and `-`, 1 to 128 characters. `secrets`, `traefik`, `_pool`, and `_checkouts` are reserved. |
| `source` | string | required | Folder to build, relative to the Russelfile. Usually `"."`. Must stay inside the repo: no `..`, no absolute path. |
| `port` | integer | required | The port the app listens on. Russel passes it to the app as `PORT`. |
| `memory` | string | required | Memory limit in megabytes: `"256mb"` or `"256mib"`. At least `16mb`, at most `65535mb`. `gb` isn't accepted. A microVM boots with at least `256mb`: a smaller value is raised to 256 and `russel status` shows both numbers. |
| `type` | string | `"container"` | `"container"` or `"microvm"`. MicroVMs are experimental and need `/dev/kvm`, `cloud-hypervisor` v52+, `virtiofsd`, and `passt`; without them the deploy fails before building. See [Runtimes](../concepts/runtimes.md). |
| `bin` | string | `name` | The binary the build produces, run as `$out/bin/<bin>`. Letters, digits, `.`, `_`, `+`, and `-`, up to 256 characters. |
| `cpus` | integer | `1` | 1 to 32. The vCPU count for a microVM, or a CPU-time limit (`podman run --cpus`) for a container. A container limit needs the `cpu` cgroup controller delegated to the `russel` account; without it the container runs unlimited and Russel logs a warning. `russel status` and the dashboard list the limit as not applied in that case. |
| `package` | string | none | A nixpkgs attribute to run when the repo has no `flake.nix`, such as `"navidrome"`. A committed flake wins. If `bin` is unset, it defaults to the attribute's last part. |
| `args` | list of strings | `[]` | Arguments passed to the app after its name. Up to 32, each up to 256 bytes, one line each. |
| `podman_args` | list of strings | `[]` | Extra `podman run` flags, one flag or value per entry. Containers only. Up to 32. Flags that Russel sets itself or that weaken isolation are rejected. |
| `user` | string | none | Only `"root"` is accepted. Without it the app runs as an unprivileged user that owns its volumes (and can still bind ports below 1024). |
| `restart` | string | none | Only `"unless-stopped"` is accepted. Restarts the app when it exits; see [Restart on exit](../concepts/lifecycle.md#restart-on-exit). `russel stop` keeps it down until the next deploy. |
| `debug` | boolean | `false` | Containers only: adds a shell, curl, and `/usr/bin/env` for troubleshooting. Don't leave it on. |
| `guest` | string | `"busybox"` | What runs inside the sandbox. `"linux"` (a full NixOS userspace) is planned and is rejected for now. |

## `[service.env]`

Environment variables for the app, as `KEY = "value"` pairs.

- Names start with a letter or `_`, followed by letters, digits, or `_`. Up to 64 variables.
- Values are one line, up to 4096 bytes.
- `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, and `SHELL` are reserved.
- `"secret://NAME"` is replaced with the stored secret at deploy time.

See [Env and secrets](../guides/env-secrets.md).

## `[ingress]`

How the app is reached from outside. Optional; an empty table is the same as none.

| Field | Type | Default | Rules |
|---|---|---|---|
| `host` | string | `<name>.<RUSSEL_TRAEFIK_DOMAIN>` | The exact host name Traefik routes to this service, such as `api.example.com` or `example.com`. Lowercased. Up to 253 characters, in labels of up to 63 letters, digits, and hyphens. No wildcards. For international names, use the `xn--` form. |
| `port` | integer | picked from 3100 up | Pins the host port the app is published on. At least 1024, and not 7878 or 7946. |

Pin `port` when a script, firewall rule, or non-HTTP client needs a fixed port. The pin holds across updates, rollbacks, and restarts. Two versions can't share it, so updates stop the old version first and have a short gap, as with `[[ports]]`. Apps reached through Traefik don't need it. HTTPS is set on the control plane with `RUSSEL_TRAEFIK_TLS=1`; see [Traefik ingress](../guides/traefik-ingress.md).

## `[[volumes]]`

Folders that keep data across restarts and updates. Up to 16. They work on both runtimes; on a container the root stays read-only either way.

| Field | Type | Default | Rules |
|---|---|---|---|
| `name` | string | | A folder Russel manages, at `/var/lib/russel/<service>/volumes/<name>`. Letters, digits, `.`, `_`, and `-`, starting with a letter or digit, up to 64 characters. Use either `name` or `host`. |
| `host` | string | | An absolute folder on the server. Allowed only under a prefix listed in `RUSSEL_VOLUME_ROOTS`, and it must be owned by the `russel` account. Russel never deletes it. |
| `guest` | string | required | Where the folder appears inside the app. Absolute, not `/`, no `..`, unique. MicroVMs also reject `/nix/store`, `/config`, `/run/russel`, `/proc`, `/sys`, and `/dev`. |
| `rw` | boolean | `false` | Mount it writable. A service with a writable volume is replaced cold on update: the old version stops before the new one starts, so two versions never write to it at once. Rolling back the code does not undo schema or data changes the new version made. |
| `keep` | boolean | `false` | Keep a managed folder when the service is destroyed. `russel destroy --delete-volumes` deletes it anyway. |

## `[[ports]]`

Extra host ports, next to the main one. Up to 8.

| Field | Type | Rules |
|---|---|---|
| `host` | integer | At least 1024; not 7878, 7946, or the `[ingress].port` |
| `guest` | integer | Not 0, and not `service.port` |

A service with `[[ports]]` can't run two versions at once, so updates stop the old version first and have a short gap. Leave it out unless you need it.

## Fields that don't exist

These are rejected when the Russelfile loads:

- `[database]`: run Postgres or Redis as their own service with `package` and a kept volume. See `examples/postgres` and `examples/redis`.
- `tls`, `ssl`, `domain`, `tunnel`, `hosts`, or `host_port` under `[ingress]`: HTTPS is a control-plane setting, `RUSSEL_TRAEFIK_TLS`.
- More than one service per file: use one Russelfile per service, and `--config` to pick one.

## The examples

| Example | `name` | `port` | `memory` | Notes |
|---|---|---|---|---|
| `basic-http` | `api` | 3000 | 256mb | Go server, `bin = "basic-http"` |
| `microvm-http` | `microvm-http` | 3000 | 256mb | The same app as a microVM |
| `hello-rust` | `hello-rust` | 3000 | 128mb | Rust |
| `env-config` | `env-config` | 3000 | 128mb | Uses `secret://DEMO_SECRET` |
| `shortlink` | `shortlink` | 3000 | 128mb | In-memory URL shortener |
| `filebrowser` | `filebrowser` | 8080 | 256mb | Requires a login |
| `static-test` | `static-test` | 8000 | 256mb | `bin = "app"` |
| `navidrome` | `navidrome` | 4533 | 512mb | `package`, volumes at `/data` and `/music` |
| `vaultwarden` | `vaultwarden` | 8000 | 512mb | `package`, volume at `/data` |
| `postgres` | `postgres` | 5432 | 512mb | `package`, data in a `host` folder you prepare |
| `redis` | `redis` | 6379 | 256mb | `package`, `bin = "redis-server"`, volume at `/data` |
| `caddy` | `caddy` | 8080 | 128mb | `package`, no volume |
| `meilisearch` | `meilisearch` | 7700 | 512mb | `package`, volume at `/data` |

None of them set `[ingress]`. More: [Examples](./examples.md).

## Related

- [First deploy](../getting-started/first-deploy.md) · [Env and secrets](../guides/env-secrets.md) · [Traefik ingress](../guides/traefik-ingress.md) · [Runtimes](../concepts/runtimes.md) · [CLI](./cli.md)
