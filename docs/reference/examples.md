---
title: Examples
description: Thirteen ready-to-deploy example services, from a minimal web server to Postgres and Vaultwarden, and what each one shows.
sidebar_position: 5
keywords: [examples, basic-http, microvm-http, hello-rust, env-config, shortlink, filebrowser, static-test, navidrome, vaultwarden, postgres, redis, caddy, meilisearch]
---

The `examples/` folder of the Russel repo holds thirteen services. Deploy any of them straight from GitHub by pointing `--config` at its Russelfile:

```bash
russel deploy https://github.com/daschinmoy21/russel.git --config examples/<name>/Russelfile.toml
```

Then find its port and try it:

```bash
russel ps
curl http://127.0.0.1:<host-port>/
```

Use the example's `service.name` with `russel status` and `russel logs`. For `basic-http` that's `api`; for the rest it's the folder name.

## The list

All run as containers, and so work on a VPS without KVM, except `microvm-http`.

| Example | What it shows |
|---|---|
| `basic-http` | A small Go server with `/health` and a static page. The Quickstart uses it. |
| `microvm-http` | The same Go server as a microVM. Needs KVM. |
| `hello-rust` | A minimal Rust HTTP server |
| `env-config` | Env vars and a `secret://` value. Set the secret first (below). |
| `shortlink` | An in-memory URL shortener |
| `filebrowser` | A web file manager with a login |
| `static-test` | A static site on port 8000 |
| `navidrome` | A music server from nixpkgs, with two volumes |
| `vaultwarden` | A Bitwarden-compatible password server from nixpkgs |
| `postgres` | PostgreSQL from nixpkgs, with its data in a folder you prepare |
| `redis` | Redis from nixpkgs, with a data volume |
| `caddy` | Caddy serving files, with no state |
| `meilisearch` | A search engine from nixpkgs, with a data volume |

## `env-config`

Store the secret before deploying, or the deploy fails:

```bash
printf '%s' "not-a-real-secret" | russel secrets set DEMO_SECRET
russel deploy https://github.com/daschinmoy21/russel.git --config examples/env-config/Russelfile.toml
```

Its `/` page returns `GREETING` and `LOG_LEVEL` as JSON, plus whether `DEMO_SECRET` is set and how long it is. It never returns the secret itself.

## `filebrowser`

Filebrowser requires a login. The example sets a demo password in its Russelfile; change it to a `secret://` value before exposing the app anywhere. Its start script is `entrypoint.sh`, next to the Russelfile.

## Apps from nixpkgs

`navidrome`, `vaultwarden`, `postgres`, `redis`, `caddy`, and `meilisearch` have no `flake.nix`. Each sets `service.package` to a nixpkgs attribute and runs it directly.

Apps that listen on `127.0.0.1` by default are told to use `0.0.0.0`, because a container app listening only on loopback can't be reached through its published port.

| Example | Port | Volumes | Notes |
|---|---|---|---|
| `navidrome` | 4533 | `data`, `music` | `ND_DATAFOLDER=/data`, `ND_MUSICFOLDER=/music`. The music volume starts empty. Both are kept on destroy. |
| `vaultwarden` | 8000 | `data` | API only: nixpkgs doesn't ship the web vault. Kept on destroy. |
| `postgres` | 5432 | none | Data lives in a `host` folder under `RUSSEL_VOLUME_ROOTS`. Russel doesn't run `initdb`; the Russelfile's comments list the setup steps. |
| `redis` | 6379 | `data` | Uses a demo password |
| `caddy` | 8080 | none | `caddy file-server --listen :8080` |
| `meilisearch` | 7700 | `data` | Uses a demo master key. All its data folders point into `/data`, because the root is read-only. |

`redis` and `meilisearch` ship public demo passwords (`demo-only-not-for-production`). Replace them with `secret://` values before exposing either.

## Dockerfiles

Some examples include a Dockerfile. Russel ignores them; they exist so [Benchmarks](../guides/benchmarks.md) can compare against plain Podman.

## Related

- [Quickstart](../quickstart.md) · [Russelfile](./russelfile.md) · [Benchmarks](../guides/benchmarks.md)
