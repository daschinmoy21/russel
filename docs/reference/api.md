---
title: API reference
description: The control plane's HTTP endpoints, authentication, request bodies, and the progress stream deploys return.
sidebar_position: 2
keywords: [api, http, endpoints, deploy, ndjson, auth, bearer, rollback, secrets]
---

The control plane serves a JSON API on `http://127.0.0.1:7878`. Every path also works under `/api/`, which is what the dashboard uses.

## Authentication

Send the token from `/etc/russel/env` as a bearer token on every request:

```http
Authorization: Bearer <token>
```

| Setup | Behavior |
|---|---|
| Installer or NixOS module | The token is always required |
| Token shorter than 32 characters, or with unprintable characters | `russel-ctrl` refuses to start |
| Listening on a non-loopback address without a token | `russel-ctrl` refuses to start |
| Loopback, no token, `RUSSEL_REQUIRE_AUTH` unset | No authentication, with a warning. Only for local development. |

Over the network, reach the API through HTTPS or an SSH tunnel ([TLS reverse proxy](../guides/tls-reverse-proxy.md)).

## Endpoints

| Method | Path | What it does |
|---|---|---|
| `POST` | `/deploy` | Deploy from a repo. Streams progress. |
| `GET` | `/vms` | List all services |
| `GET` | `/vm/{id}/status` | One service's status |
| `GET` | `/vm/{id}/logs` | One service's recent output |
| `GET` | `/vm/{id}/deployments` | Deployment history, newest first (up to 20) |
| `POST` | `/vm/{id}/update` | Rebuild and redeploy. Streams progress. |
| `POST` | `/vm/{id}/rollback` | Start an earlier deployment again. Streams progress. |
| `POST` | `/vm/{id}/stop` | Stop the app, keep its history |
| `DELETE` | `/vm/{id}` | Destroy the service. Add `?keep_volumes=true` to keep every managed volume or `false` to delete them all; leave it out to follow each volume's `keep` setting. |
| `GET` | `/secrets` | List secret names (never values) |
| `POST` | `/secrets/{name}` | Set a secret. Body: `{"value": "…"}` |
| `DELETE` | `/secrets/{name}` | Delete a secret |
| `GET` | `/status`, `/logs` | Shortcuts for when exactly one service exists. `404` with none, `400` with more than one. |

`{id}` is the service's `service.name`.

## Request bodies

**`POST /deploy`**

```json
{
  "repo_url": "https://github.com/you/app.git",
  "config_path": "Russelfile.toml",
  "vm_id": "api",
  "rev": "0123456789abcdef0123456789abcdef01234567",
  "force": false
}
```

| Field | Required | Meaning |
|---|---|---|
| `repo_url` | yes | A git URL (`https://`, `http://`, `ssh://`, `git@host:path`), or an absolute folder path if the control plane allows local deploys. URLs that point at private or cloud-metadata addresses are refused. |
| `config_path` | no | The Russelfile's path in the repo. Default `Russelfile.toml`. |
| `vm_id` | no | If given, must equal the Russelfile's `service.name`. Use it as a safety check. |
| `rev` | no | Build this commit (the full 40-character id) in place of the latest one |
| `force` | no | Deploy even if the service already runs this commit and Russelfile. Without it, that case returns `unchanged` at once. |

Any other field is rejected with `422`. Ports, env, and runtime come from the Russelfile only.

**`POST /vm/{id}/update`**: `{"refresh": true}` builds the latest commit; `{}` rebuilds the running one. `repo_url` and `config_path` switch to another source and imply `refresh`. All fields are optional.

**`POST /vm/{id}/rollback`**: `{"version": 3}` for a specific deployment, or `{}` for the previous one. Starts the deployment's recorded build again; add `"rebuild": true` to build its commit from source instead. Returns `409` when that deployment has neither.

## Progress stream

`/deploy`, `/update`, and `/rollback` respond with one JSON object per line ([NDJSON](https://github.com/ndjson/ndjson-spec)) while they work:

```json
{"type":"Progress","stage":"resolve","message":"cloned …"}
{"type":"Progress","stage":"build","message":"nix build …"}
{"type":"Progress","stage":"start","message":"…"}
{"type":"Progress","stage":"ready","message":"…"}
{"type":"Complete","status":"deployed","service_id":"api","elapsed_ms":1540}
```

The last line is either `Complete` (with `status` `deployed`, `unchanged`, or `rolled_back`) or an error:

```json
{"type":"Error","message":"…","stage":"build"}
```

Read the stream as it arrives (`curl -N`). A proxy in front must not buffer it.

If 4 deploys are already running, `/deploy` returns `503`; retry later.

## Status

`GET /vm/{id}/status` returns the service's `status` (`deployed`, `failed`, `stopped`, …), runtime, host and app ports, uptime, the Traefik host name if it has one (`route_host`), and `restarts` since the last deploy (left out when zero).

## Examples

```bash
TOKEN="$(sed -n 's/^RUSSEL_API_TOKEN=//p' ~/.config/russel/env)"
H="Authorization: Bearer $TOKEN"

curl -s -H "$H" http://127.0.0.1:7878/vms
curl -s -H "$H" http://127.0.0.1:7878/vm/api/status
curl -s -H "$H" http://127.0.0.1:7878/vm/api/deployments

# Deploy, streaming progress
curl -N -H "$H" -H 'Content-Type: application/json' \
  -d '{"repo_url":"https://github.com/you/app.git"}' \
  http://127.0.0.1:7878/deploy

# Roll back to the previous deployment
curl -N -H "$H" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:7878/vm/api/rollback

# Set a secret; jq builds the JSON so quotes in the value are escaped
printf '%s' "$VALUE" | jq -Rs '{value: .}' | curl -s -H "$H" -H 'Content-Type: application/json' \
  --data-binary @- http://127.0.0.1:7878/secrets/DB_PASSWORD
```

`russel secrets set` is simpler for secrets and does the same thing.

## Related

- [CLI](./cli.md) · [Lifecycle](../concepts/lifecycle.md) · [Update and rollback](../guides/update-rollback.md) · [Environment](./environment.md)
