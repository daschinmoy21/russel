---
title: Examples
description: Thirteen deployable services — what each proves and how to run it.
sidebar_position: 5
keywords: [examples, basic-http, microvm-http, hello-rust, env-config, shortlink, filebrowser, static-test, navidrome, vaultwarden, postgres, redis, caddy, meilisearch]
---

# Examples

All examples live in `examples/` and deploy with the same verbs. VPS smoke tests should use `type = "container"` examples (`microvm-http` needs KVM).

## Index

| Example | Runtime | What it proves | Deploy |
|---|---|---|---|
| `basic-http` | container | Go `/health` + static assets; the default first deploy | `russel apply examples/basic-http` |
| `microvm-http` | microvm | Same app as `basic-http` on the microVM path | `russel apply examples/microvm-http` |
| `hello-rust` | container | Pure-std Rust HTTP, 128mb | `russel apply examples/hello-rust` |
| `env-config` | container | `[service.env]` + `secret://DEMO_SECRET` | Set secret first, then deploy (below) |
| `shortlink` | container | In-memory URL shortener | `russel apply examples/shortlink` |
| `filebrowser` | container | nixpkgs filebrowser wrapper; **requires auth, loopback bind in guest** | See filebrowser notes |
| `static-test` | container | Python static site on port 8000 | `russel apply examples/static-test` |
| `navidrome` | container | nixpkgs `navidrome` via `service.package`; `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music` | `russel apply examples/navidrome` |
| `vaultwarden` | container | nixpkgs `vaultwarden` via `service.package`; `DATA_FOLDER=/data` | `russel apply examples/vaultwarden` |
| `postgres` | container | nixpkgs `postgresql` via `service.package`; absolute `host` bind | See package notes |
| `redis` | container | nixpkgs `redis` via `service.package`; managed `data` volume | `russel apply examples/redis` |
| `caddy` | container | nixpkgs `caddy` via `service.package`; stateless file server | `russel apply examples/caddy` |
| `meilisearch` | container | nixpkgs `meilisearch` via `service.package`; managed `data` volume | `russel apply examples/meilisearch` |

For `basic-http` (whose `service.name` is `api`):

```bash
russel ps                                   # find the host port
curl http://127.0.0.1:<host-port>/health
russel status api && russel logs api
```

For another example, use the `service.name` in that example's Russelfile with `russel status` and `russel logs`.

## `env-config` (env + secrets)

Add this to `examples/env-config/Russelfile.toml` to pin the direct host port:

```toml
[ingress]
port = 8080
```

```bash
printf '%s' "bench-secret" | russel secrets set DEMO_SECRET
russel apply examples/env-config
curl http://127.0.0.1:8080/
```

Russelfile sets `LOG_LEVEL`, `GREETING`, and `DEMO_SECRET = "secret://DEMO_SECRET"`. Change these values in `[service.env]` to configure the service.

## `filebrowser` (auth-gated)

- Russelfile: port 8080, 256mb, container, `bin = "filebrowser"`.
- Guest binds loopback and requires auth — do not expose it with `RUSSEL_PUBLISH_BIND=0.0.0.0` without credentials.
- Demo password handling warns loudly; entrypoint + test script live next to the example (`entrypoint.sh`, `entrypoint_test.sh`).

## Package demos (`service.package`)

These wrap a nixpkgs attr directly, so no `flake.nix`. Their Dockerfiles use the
upstream image at the same version, only as the bench baseline. All are
`container`. Apps that default to a loopback listener must bind `0.0.0.0`
instead: pasta targets the container IP, so a `127.0.0.1` listener is
unreachable through a published host port.

| Example | Port | Managed volume | Notes |
|---|---|---|---|
| `navidrome` | 4533 | `data`, `music` | `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music`; music volume starts empty; `keep = true` |
| `vaultwarden` | 8000 | `data` | `DATA_FOLDER=/data`; API only (`WEB_VAULT_ENABLED=false`: nixpkgs ships no web vault); `ROCKET_ADDRESS=0.0.0.0`; `keep = true` |
| `postgres` | 5432 | no | Russel does not run `initdb`; `-D` points at a prepared `host` PGDATA under `RUSSEL_VOLUME_ROOTS` (preparation steps in its Russelfile); runs unprivileged by default, which postgres requires (it refuses root) |
| `redis` | 6379 | `data` | `--dir /data`, `--bind 0.0.0.0`, demo `--requirepass` |
| `caddy` | 8080 | no | stateless `caddy file-server --listen :8080` |
| `meilisearch` | 7700 | `data` | `MEILI_DB_PATH`, `MEILI_DUMP_DIR`, `MEILI_SNAPSHOT_DIR` on `/data` (the rootfs is read-only); `MEILI_MASTER_KEY` demo key |

`redis` and `meilisearch` ship public demo secrets (`demo-only-not-for-production`); override them with `secret://` refs before exposing anything.

## `basic-http` vs `microvm-http`

Same Go app, different `service.type`. Use them as a runtime A/B: identical `/health` + static `index.html`, only the isolation differs. Container settings in the `basic-http` Russelfile apply there; KVM/TAP/`RUSSEL_KERNEL_PATH` apply to `microvm-http`.

## Dockerfiles

Per-example Dockerfiles exist as a **baseline for benchmarks** (`bench.sh` raw-podman path), not as a Russel input. Russel never consumes Dockerfiles — it consumes the Nix closure.

## Related

- [First deploy](../getting-started/first-deploy.md) · [Russelfile](russelfile.md) · [Benchmarks](../guides/benchmarks.md)
