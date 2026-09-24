---
title: Russelfile reference
description: Canonical Russelfile.toml schema — every field, default, and validation rule.
sidebar_position: 3
keywords: [russelfile, manifest, service type, guest, memory, env, volumes, package, ports, database, ingress]
---

# Russelfile reference

Source of truth is `crates/core/src/config.rs` (`Russelfile`, `deny_unknown_fields`) plus the `russel init` template (`crates/cli/src/init.rs`). This page mirrors both. Unknown fields are rejected.

Scaffold one:

```bash
russel init --type container --with-flake
```

## Full example

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"          # optional — defaults to name
type = "container"   # optional: "microvm" (default) | "container"
guest = "busybox"    # optional: "busybox" (default); "linux" errors until implemented
cpus = 1             # optional: 1..=32, default 1 (microVM vCPUs; ignored for containers)
debug = false        # optional: container-only shell tools, default false
# package = "navidrome"  # nixpkgs attr when no committed flake.nix; container only
# args = ["--loglevel", "info"]
# userns = "keep-id"     # container only
# restart = "unless-stopped"  # container only

[ingress]             # optional
host = "api.example.com" # exact Traefik Host() value
port = 4000            # host-side backend; like -p HOST:guest

[service.env]        # optional
LOG_LEVEL = "info"
FEATURE_X = "1"
DB_PASSWORD = "secret://DB_PASSWORD"

# [[volumes]]            # container only; rootfs stays read-only
# name = "data"          # managed under /var/lib/russel/<id>/volumes/data
# guest = "/data"
# rw = true
# keep = true            # survive destroy unless --delete-volumes

# [[ports]]              # container only; extra publishes besides service.port
# host = 50300
# guest = 50300

# Optional — all disabled. enabled = true is rejected (placeholder, not a feature).
[database.postgres]
enabled = false

[database.redis]
enabled = false
```

## Fields

| Field | Type | Default | Rules |
|---|---|---|---|
| `service.name` | `String` | — | Service id base; `bin_name()` defaults to it. Charset `[A-Za-z0-9._+-]`, max 256 (enforced at deploy/init). Reserved service dirs (`secrets`, `traefik`, `_pool`) rejected as ids. |
| `service.source` | `String` | — | Non-empty; `.` ok; relative only; no `..`, no absolute. (Build uses the repo root; `source` is validated but not yet a subdirectory selector.) |
| `service.port` | `u16` | — | Guest listen port; `!= 0`. Injected as `PORT`. App must listen on it. |
| `service.memory` | `Memory` | — | `"<N>mb\|mib"` case-insensitive, `u16` MiB, `>= 16mb`, max 65535. No `gb`/`gib`. Example: `"256mb"`. (Docs saying "minimum 256mb" is a recommendation, not validation.) |
| `service.type` → `runtime` | `RuntimeKind` | `microvm` | `"microvm"` \| `"container"`. CLI `--runtime` must match — check, not override. |
| `service.guest` | `GuestKind` | `busybox` | `"busybox"` \| `"linux"`. `linux` parses then fails load with "not implemented yet". Orthogonal to `type` (isolation vs userspace). |
| `service.bin` | `String?` | `name` | Binary produced by the build. Same charset/length rule as `name`. |
| `service.cpus` | `u8` | `1` | `1..=32`. MicroVM vCPUs; noted ignored for containers (container `--cpus` only via passthrough where supported). |
| `service.debug` | `bool` | `false` | Container-only: adds bash/curl + `/usr/bin/env`. MicroVMs ignore it. |
| `service.env` | `Map<String,String>` | `{}` | Key `^[A-Za-z_][A-Za-z0-9_]*$`, max 64 keys. Reserved rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`. Value: no NUL/`\n`/`\r`, max 4096 B. `secret://NAME` refs resolved at deploy. Merge: file < env-file < `--env`. |
| `service.package` | `String?` | omitted | nixpkgs attr to wrap when the repo has no committed `flake.nix` (e.g. `navidrome`). Container-oriented; a committed flake always wins. Attr must match the package validator (no path traversal). When the build uses package and `bin` is unset, the binary defaults to the attr's last component. |
| `service.args` | `String[]` | `[]` | Extra argv after the entrypoint. |
| `service.userns` | `String?` | omitted | Container-only. Only `"keep-id"` is accepted. |
| `service.restart` | `String?` | omitted | Container-only. Only `"unless-stopped"` is accepted. |
| `[[volumes]].name` | `String?` | — | Managed dir `/var/lib/russel/<id>/volumes/<name>`. Mutually exclusive with `host`. 1..=64 chars, `[A-Za-z0-9._-]`, must start alphanumeric. Max 16 volume rows. Container only. |
| `[[volumes]].host` | `String?` | — | Absolute host bind. Requires `RUSSEL_VOLUME_ROOTS` at deploy. Never deleted on destroy. Mutually exclusive with `name`. |
| `[[volumes]].guest` | `String` | — | Absolute container path. Not `"/"`. No `..`. Unique per file. |
| `[[volumes]].rw` | `bool` | `false` | Read-write bind. Rootfs stays read-only either way. |
| `[[volumes]].keep` | `bool` | `false` | Only with `name`. Survives `destroy` unless `--delete-volumes` / `keep_volumes=false`. Absolute `host` binds ignore `keep`. |
| `[[ports]].host` / `.guest` | `u16` | — | Extra publishes (container only). Host `>= 1024`, not 0/7878/7946, not the ingress pin; guest not 0 and not `service.port`. Max 8 rows. |
| `[ingress].host` | `String?` | omitted | Exact Traefik `Host()` value for this service, such as `api.example.com` or `example.com`. Russel trims and canonicalizes it to lowercase. DNS labels are ASCII letters, digits, and hyphens; each is 1–63 characters and the full name is at most 253 characters. Single-label names (`localhost`) are allowed. Wildcards (`*.example.com`) and non-ASCII/IDN are rejected; pass punycode (`xn--...`) if you need IDN. |
| `[ingress].port` | `u16?` | omitted | Host-side backend port used by Traefik and publishing, like `-p HOST:guest`. It is not `service.port`; it must be at least 1024 and cannot be 7878 or 7946. |
| `database.postgres.enabled` | `bool` | — | `false` ok; `true` → load error "not yet supported". |
| `database.redis.enabled` | `bool` | — | Same as postgres. |

### Volumes and ports

`[[volumes]]` and `[[ports]]` require `service.type = "container"`. The container rootfs stays read-only; writable state goes through volume binds.

Managed volumes (`name =`) live under `/var/lib/russel/<service_id>/volumes/<name>`. On destroy, `keep = true` keeps that directory unless the operator passes `--delete-volumes` (or `?keep_volumes=false`). `--keep-volumes` keeps every managed dir. Absolute `host =` binds need `RUSSEL_VOLUME_ROOTS` and are never deleted by Russel.

When ctrl runs as root, each managed volume directory is chowned to the rootless podman user (`RUSSEL_PODMAN_USER`/`SUDO_USER`) so Podman can open the bind source; the service directory itself stays root-owned, and absolute `host =` binds are left to the operator.

`service.package` is for nixpkgs apps without a repo flake (`russel init --package navidrome`). Do not combine `--package` with `--with-flake` on init: a committed flake wins at build time and would ignore the package attr.

### Ingress

`service.port` is the guest listen port and is injected as `PORT`. The optional
`[ingress]` table describes the host-facing Traefik route. An empty table is a
no-op, the same as omitting it.

- `ingress.host` is the exact `Host()` name. It is not prefixed with the service
  name. When omitted, Traefik derives `<service_id>.<RUSSEL_TRAEFIK_DOMAIN>`.
- `ingress.port` pins the host-side backend. Omit it for the normal HTTP case;
  Russel allocates a backend port. Pin it when a script needs a stable local
  port, a firewall rule names one port, or another non-HTTP publisher needs a
  stable publish port.
- A file host is the source of truth. `--host` must match it when supplied.
  `-p HOST:guest` may supply a host-side pin when the file omits `ingress.port`,
  but must match the file pin when both are present. A file pin also requires
  the `-p` guest to equal `service.port`.
- HTTPS is not a Russelfile field. `RUSSEL_TRAEFIK_TLS=1` attaches `websecure`
  plus the cert resolver to every router. ACME issues for whatever lands in
  `Host()`.

Host validation happens when the Russelfile loads, and a uniqueness check under
a per-directory write lock happens when Traefik writes the route. DNS records
still need to be created by the operator.

`--config` must be repo-relative (default `Russelfile.toml`), opened via `openat` + `O_NOFOLLOW` (symlinks rejected), 1 MiB cap.

## Type vs guest

| Field | Values | Status |
|---|---|---|
| `type` | `microvm`, `container` | Both ship. `type` is isolation. |
| `guest` | `busybox` (default), `linux` | `linux` is a host-built NixOS userspace — parsed, rejected at load until boot exists. |

## Env + secrets

See [Env and secrets](../guides/env-secrets.md). `secret://NAME` in any env value resolves from `/var/lib/russel/secrets/` at deploy time. Injected runtime vars (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are set by Russel — defining them is an error.

## What is not in the schema

`Russelfile`, `ServiceConfig`, and `IngressConfig` all use `deny_unknown_fields`.
These names fail parse:

- `[ingress]` aliases: `ssl`, `tls`, `tunnel`, `domain`, `host_port`, `hosts`.
  TLS is `RUSSEL_TRAEFIK_TLS` on the control plane. App tunnels are not in the
  file; a host-wide proxy in front of Traefik is an ops choice.
- `[dependencies]` (build/dev/runtime) from early design notes.
- Multi-service files (`[services.api]`, `runtime = "elixir"`, `[service.api]`)
  from `vision.md`. One `[service]` table only.

`russel build/develop/check` verbs do not exist. Builds run inside `deploy`.

## Per-example values

| Example | `name` / `port` / `memory` / `type` / `bin` |
|---|---|
| `basic-http` | `api` / 3000 / 256mb / container / `basic-http` |
| `microvm-http` | `microvm-http` / 3000 / 256mb / microvm / `basic-http` |
| `hello-rust` | `hello-rust` / 3000 / 128mb / container / `hello-rust` |
| `env-config` | `env-config` / 3000 / 128mb / container / `env-config` + `[service.env]` with `secret://DEMO_SECRET` |
| `shortlink` | `shortlink` / 3000 / 128mb / container / `shortlink` |
| `filebrowser` | `filebrowser` / 8080 / 256mb / container / `filebrowser` (requires auth, loopback bind in guest) |
| `static-test` | `static-test` / 8000 / 256mb / container / `app` |
| `navidrome` | `navidrome` / 4533 / 512mb / container / package `navidrome`; `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music` on managed volumes |
| `vaultwarden` | `vaultwarden` / 8000 / 512mb / container / package `vaultwarden`; `DATA_FOLDER=/data` on a managed volume |
| `postgres` | `postgres` / 5432 / 512mb / container / package `postgresql`; needs a prepared `host` PGDATA under `RUSSEL_VOLUME_ROOTS` |
| `redis` | `redis` / 6379 / 256mb / container / package `redis`, bin `redis-server`; `--dir /data` on a managed volume |
| `caddy` | `caddy` / 8080 / 128mb / container / package `caddy`, bin `caddy`; stateless `file-server`, no volume |
| `meilisearch` | `meilisearch` / 7700 / 512mb / container / package `meilisearch`, bin `meilisearch`; `MEILI_DB_PATH=/data` on a managed volume |

None of the examples set `[ingress]`. Traefik then uses `<service_id>.<RUSSEL_TRAEFIK_DOMAIN>`.

## Related

- [First deploy](../getting-started/first-deploy.md) · [Traefik ingress](../guides/traefik-ingress.md) · [Runtimes](../concepts/runtimes.md) · [Env and secrets](../guides/env-secrets.md) · [CLI](cli.md)
