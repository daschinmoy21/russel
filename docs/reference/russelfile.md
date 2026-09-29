---
title: Russelfile reference
description: Canonical Russelfile.toml schema — every field, default, and validation rule.
sidebar_position: 3
keywords: [russelfile, manifest, service type, guest, memory, env, volumes, package, ports, ingress]
---

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
type = "container"   # optional: "container" (default) | "microvm" (experimental)
guest = "busybox"    # optional: "busybox" (default); "linux" errors until implemented
cpus = 1             # optional: 1..=32, default 1 (microVM vCPUs; container --cpus)
debug = false        # optional: container-only shell tools, default false
# package = "navidrome"  # nixpkgs attr when no committed flake.nix
# args = ["--loglevel", "info"]  # process argv (both runtimes)
# user = "root"          # both runtimes; omitted runs the app unprivileged
# restart = "unless-stopped"  # both runtimes

[ingress]             # optional
host = "api.example.com" # exact Traefik Host() value
port = 4000            # optional host-side backend pin

[service.env]        # optional
LOG_LEVEL = "info"
FEATURE_X = "1"
DB_PASSWORD = "secret://DB_PASSWORD"

# [[volumes]]            # both runtimes (microVMs: one virtiofs share each)
# name = "data"          # managed under /var/lib/russel/<id>/volumes/data
# guest = "/data"
# rw = true
# keep = true            # survive destroy unless --delete-volumes

# [[ports]]              # both runtimes; extra publishes besides service.port
# host = 50300
# guest = 50300
```

There is no `[database]` section. Run Postgres or Redis as their own service with `service.package` and a kept `[[volumes]]` row; see `examples/postgres` and `examples/redis`.

## Fields

| Field | Type | Default | Rules |
|---|---|---|---|
| `service.name` | `String` | — | Same rule as a service id, checked at load: ASCII `[A-Za-z0-9_-]`, 1–128 chars, not a reserved data-root dir (`secrets`, `traefik`, `_pool`, `_checkouts`). `bin` defaults to it; `russel init` suggests it as the service id. `service.name` is the service id used by deploy, status, logs, update, and destroy. |
| `service.source` | `String` | — | Non-empty; `.` ok; relative only; no `..`, no absolute. (Build uses the repo root; `source` is validated but not yet a subdirectory selector.) |
| `service.port` | `u16` | — | Guest listen port; `!= 0`. Injected as `PORT`. App must listen on it. |
| `service.memory` | `Memory` | — | `"<N>mb\|mib"` case-insensitive, `u16` MiB, `>= 16mb`, max 65535. No `gb`/`gib`. Example: `"256mb"`. (Docs saying "minimum 256mb" is a recommendation, not validation.) |
| `service.type` → `runtime` | `RuntimeKind` | `container` | `"container"` \| `"microvm"`. `microvm` is experimental in v0.1: it needs read-write `/dev/kvm` and `passt` (or `CAP_NET_ADMIN` for TAP networking), and a deploy fails before the build otherwise. No root needed. Set the runtime here; deploy reads it from the Russelfile. |
| `service.guest` | `GuestKind` | `busybox` | `"busybox"` \| `"linux"`. `linux` parses then fails load with "not implemented yet". Orthogonal to `type` (isolation vs userspace). |
| `service.bin` | `String?` | `name` | Binary produced by the build, run as `$out/bin/<bin>`. Wider rule than `name`, checked at load: `[A-Za-z0-9._+-]`, 1–256 chars, at least one letter or digit, not `.`/`..`. |
| `service.cpus` | `u8` | `1` | `1..=32`. Both runtimes: microVM vCPUs, container `podman run --cpus N` (a CPU-time limit). Rootless Podman needs the `cpu` cgroup controller delegated to the podman user (`podman info` → `cgroupControllers`); without it the container runs unlimited and ctrl logs a warning. |
| `service.debug` | `bool` | `false` | Container-only: adds bash/curl + `/usr/bin/env`. MicroVMs ignore it. |
| `service.env` | `Map<String,String>` | `{}` | Checked at load. Key `^[A-Za-z_][A-Za-z0-9_]*$`, max 64 keys. Reserved rejected: `PORT`, `VM_IP`, `HOST_IP`, `APP`, `IFS`, `PATH`, `LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELL`. Value: no NUL/`\n`/`\r`, max 4096 B. `secret://NAME` refs resolved at deploy. |
| `service.package` | `String?` | omitted | nixpkgs attr to wrap when the repo has no committed `flake.nix` (e.g. `navidrome`). Works on both runtimes; a committed flake always wins. Attr must match the package validator (no path traversal). When the build uses package and `bin` is unset, the binary defaults to the attr's last component. |
| `service.args` | `String[]` | `[]` | Process argv after the entrypoint (`$out/bin/<bin> <args…>`). Max 32 entries, 256 B each, no NUL/newline. Both runtimes: containers get it after the entrypoint, microVMs through `/config/argv` (one entry per line, read verbatim by the guest agent, never through `eval`). Separate from Podman flags, which never reach the process. |
| `service.podman_args` | `String[]` | `[]` | Extra `podman run` flags, one Podman token per entry (not process argv). Container only. Max 32 entries, 256 B each, no NUL/newline. Russel-owned and isolation-weakening flags are validated fail-closed and rejected by the control plane. |
| `service.user` | `String?` | omitted | Who the app runs as, on both runtimes. Omitted: an unprivileged user that owns the managed volumes, so files the app writes belong to the same user on the host: the rootless Podman user for a container (Podman `--userns=keep-id`), and the control plane's own uid and gid for a microVM (`nobody` under a root ctrl). The app may still bind ports below 1024, and `HOME` defaults to `/tmp`. `"root"` is the only value, for apps that need it. `service.userns` was removed: `keep-id` is now the default. |
| `service.restart` | `String?` | omitted | Only `"unless-stopped"` is accepted. Containers get Podman's restart policy. MicroVMs are relaunched by ctrl from the recorded generation (same build and config, nothing rebuilt) when the VM exits without a `stop` or `destroy`, and at ctrl start when the VM is not running. Crash loops back off 1s, 2s, 4s, … up to 30s and read `failed` meanwhile; a run of 60s resets the backoff. `russel stop` keeps it down until the next deploy. `status` reports `restarts`. |
| `[[volumes]].name` | `String?` | — | Managed dir `/var/lib/russel/<id>/volumes/<name>`. Mutually exclusive with `host`. 1..=64 chars, `[A-Za-z0-9._-]`, must start alphanumeric. Max 16 volume rows. Both runtimes. |
| `[[volumes]].host` | `String?` | — | Absolute host bind. Requires `RUSSEL_VOLUME_ROOTS` at deploy, and the directory must be owned by the account that runs the control plane (`russel`). Under the installer's unit, also add the root to `ReadWritePaths=` with `sudo systemctl edit russel-ctrl`, or the app sees it read-only. Never deleted on destroy. Mutually exclusive with `name`. |
| `[[volumes]].guest` | `String` | — | Absolute path inside the container or microVM. Not `"/"`. No `..`. Unique per file. MicroVMs also reject paths at, above, or inside the guest agent's own mounts (`/nix/store`, `/config`, `/run/russel`, `/proc`, `/sys`, `/dev`), and do not start the app if a volume fails to mount. |
| `[[volumes]].rw` | `bool` | `false` | Read-write bind. A container rootfs stays read-only either way. |
| `[[volumes]].keep` | `bool` | `false` | Only with `name`. Survives `destroy` unless `--delete-volumes` / `keep_volumes=false`. Absolute `host` binds ignore `keep`. |
| `[[ports]].host` / `.guest` | `u16` | — | Extra publishes (both runtimes). A service with `[[ports]]` redeploys stop-then-start, with a brief gap, because both generations cannot hold the same host port. Additional listeners only; the primary mapping is `service.port` plus `[ingress].port`. Host `>= 1024`, not 0/7878/7946, not the ingress pin; guest not 0 and not `service.port`. Max 8 rows. |
| `[ingress].host` | `String?` | omitted | Exact Traefik `Host()` value for this service, such as `api.example.com` or `example.com`. Russel trims and canonicalizes it to lowercase. DNS labels are ASCII letters, digits, and hyphens; each is 1–63 characters and the full name is at most 253 characters. Single-label names (`localhost`) are allowed. Wildcards (`*.example.com`) and non-ASCII/IDN are rejected; pass punycode (`xn--...`) if you need IDN. |
| `[ingress].port` | `u16?` | omitted | Host-side backend port used by Traefik and publishing, like a host-side port pin. It is not `service.port`; it must be at least 1024 and cannot be 7878 or 7946. |

### Volumes and ports

`[[volumes]]` and `[[ports]]` work on both runtimes. The container rootfs stays read-only; writable state goes through volume binds. On a microVM each volume is its own virtiofs share (read-only unless `rw = true`), mounted by the guest agent before the app starts; `[[ports]]` rows are published by `passt` (unprivileged ctrl) or one `socat` per row (TAP networking).

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
- The Russelfile is the source of truth for ingress. `service.port` is the guest
  listen port and `PORT`; `[ingress].port` optionally pins the host-side port.
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
| `type` | `container` (default), `microvm` | `container` ships; `microvm` is experimental (needs `/dev/kvm`, `cloud-hypervisor` v52+, `virtiofsd`, `passt`; no root). `type` is isolation. |
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

- [First deploy](../getting-started/first-deploy.md) · [Traefik ingress](../guides/traefik-ingress.md) · [Runtimes](../concepts/runtimes.md) · [Env and secrets](../guides/env-secrets.md) · [CLI](./cli.md)
