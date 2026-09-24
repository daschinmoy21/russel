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
| `basic-http` | container | Go `/health` + static assets; the default first deploy | `russel deploy examples/basic-http -p 8080:3000 --vm-id api --runtime container` |
| `microvm-http` | microvm | Same app as `basic-http` on the microVM path | `russel deploy examples/microvm-http -p 8080:3000 --vm-id api` |
| `hello-rust` | container | Pure-std Rust HTTP, 128mb | `russel deploy examples/hello-rust -p 8080:3000 --vm-id hello --runtime container` |
| `env-config` | container | `[service.env]` + `secret://DEMO_SECRET` | Set secret first, then deploy (below) |
| `shortlink` | container | In-memory URL shortener | `russel deploy examples/shortlink -p 8080:3000 --vm-id links --runtime container` |
| `filebrowser` | container | nixpkgs filebrowser wrapper; **requires auth, loopback bind in guest** | See filebrowser notes |
| `static-test` | container | Python static site on port 8000 | `russel deploy examples/static-test -p 8080:8000 --vm-id static --runtime container` |
| `navidrome` | container | nixpkgs `navidrome` via `service.package`; `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music` | `russel deploy examples/navidrome -p 8080:4533 --vm-id music --runtime container` |
| `vaultwarden` | container | nixpkgs `vaultwarden` via `service.package`; `DATA_FOLDER=/data` | `russel deploy examples/vaultwarden -p 8080:8000 --vm-id vault --runtime container` |
| `postgres` | container | nixpkgs `postgresql` via `service.package`; absolute `host` bind | See package notes |
| `redis` | container | nixpkgs `redis` via `service.package`; managed `data` volume | `russel deploy examples/redis -p 8080:6379 --vm-id cache --runtime container` |
| `caddy` | container | nixpkgs `caddy` via `service.package`; stateless file server | `russel deploy examples/caddy -p 8080:8080 --vm-id caddy --runtime container` |
| `meilisearch` | container | nixpkgs `meilisearch` via `service.package`; managed `data` volume | `russel deploy examples/meilisearch -p 8080:7700 --vm-id search --runtime container` |

Verify any of them:

```bash
curl http://127.0.0.1:8080/health   # or / for static/env-config
russel status <id> && russel logs <id>
```

## `env-config` (env + secrets)

```bash
printf '%s' "bench-secret" | russel secrets set DEMO_SECRET
russel deploy examples/env-config -p 8080:3000 --vm-id env-demo --runtime container
curl http://127.0.0.1:8080/
```

Russelfile sets `LOG_LEVEL`, `GREETING`, and `DEMO_SECRET = "secret://DEMO_SECRET"`. Override at deploy time with `--env` / `--env-file`.

## `filebrowser` (auth-gated)

- Russelfile: port 8080, 256mb, container, `bin = "filebrowser"`.
- Guest binds loopback and requires auth — do not expose it with `RUSSEL_PUBLISH_BIND=0.0.0.0` without credentials.
- Demo password handling warns loudly; entrypoint + test script live next to the example (`entrypoint.sh`, `entrypoint_test.sh`).

## Package demos (`service.package`)

These wrap a nixpkgs attr directly, so no `flake.nix` and no Dockerfile. All are
`container`. Apps that default to a loopback listener must bind `0.0.0.0`
instead: pasta targets the container IP, so a `127.0.0.1` listener is
unreachable through `-p`.

| Example | Port | Managed volume | Notes |
|---|---|---|---|
| `navidrome` | 4533 | `data`, `music` | `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music`; music volume starts empty; `keep = true` |
| `vaultwarden` | 8000 | `data` | `DATA_FOLDER=/data`; `keep = true` |
| `postgres` | 5432 | no | Russel does not run `initdb`; `-D` points at a prepared `host` PGDATA under `RUSSEL_VOLUME_ROOTS` |
| `redis` | 6379 | `data` | `--dir /data`, `--bind 0.0.0.0`, demo `--requirepass` |
| `caddy` | 8080 | no | stateless `caddy file-server --listen :8080` |
| `meilisearch` | 7700 | `data` | `MEILI_DB_PATH=/data`, `MEILI_MASTER_KEY` demo key |

`redis` and `meilisearch` ship public demo secrets (`demo-only-not-for-production`); override them with `secret://` refs before exposing anything.

## `basic-http` vs `microvm-http`

Same Go app, different `service.type`. Use them as a runtime A/B: identical `/health` + static `index.html`, only the isolation differs. Container flags (`--runtime container`, `-- -v …`) apply to `basic-http`; KVM/TAP/`RUSSEL_KERNEL_PATH` apply to `microvm-http`.

## Dockerfiles

Per-example Dockerfiles exist as a **baseline for benchmarks** (`bench.sh` raw-podman path), not as a Russel input. Russel never consumes Dockerfiles — it consumes the Nix closure.

## Related

- [First deploy](../getting-started/first-deploy.md) · [Russelfile](russelfile.md) · [Benchmarks](../guides/benchmarks.md)
