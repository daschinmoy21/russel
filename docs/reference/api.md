---
title: API reference
description: Control-plane HTTP endpoints, auth, NDJSON streams, and status codes.
sidebar_position: 2
keywords: [api, endpoints, deploy, ndjson, auth, bearer, rollback, secrets]
---

Base URL defaults to `http://127.0.0.1:7878` (override with `RUSSEL_CTRL_ADDR`). All endpoints are HTTP/JSON behind `auth_middleware`. `POST /deploy`, `/update`, `/rollback` return **NDJSON event streams** (`Progress` lines then `Complete` or `Error`). Source: `crates/ctrl/src/api/router.rs`, wire types `crates/core/src/api.rs`.

## Authentication

| Scenario | Behaviour |
|---|---|
| Loopback + no token | Dev mode (warn, no auth) |
| Loopback + `RUSSEL_REQUIRE_AUTH=1` + no token | **Refuses to start** |
| Loopback + `RUSSEL_API_TOKEN` (≥32 chars) | Bearer required on all routes |
| Token set but \<32 chars or non-printable-ASCII | **Refuses to start** |
| Non-loopback + no token | **Refuses to start** |
| Non-loopback + valid token | Bearer required on all routes |

```http
Authorization: Bearer <token>
```

Tokens are compared constant-time. The CLI (and dashboard client) **refuse** to send the token over `http://` to a non-loopback host — terminate TLS at Caddy/nginx/Traefik or use the SSH tunnel. Override only with `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1`. `repo_url` userinfo is redacted in logs/metadata.

## Endpoints

| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/deploy` | Apply a source (NDJSON stream; status `unchanged` when the service already runs its commit and Russelfile; `503` when the deploy semaphore is full) |
| `GET` | `/vm/{id}/status` | Status for one service (agent-proxied when `RUSSEL_AGENT_URL` is set) |
| `GET` | `/vm/{id}/logs` | Logs for one service (appends `podman logs` for containers) |
| `GET` | `/vm/{id}/deployments` | History, newest first (`deployments.json` journal) |
| `POST` | `/vm/{id}/rollback` | Redeploy a prior version's recorded commit (`{"version": N}` optional; NDJSON; `404`/`409`/`500`) |
| `POST` | `/vm/{id}/stop` | Stop (keep metadata/history, remove Traefik file) |
| `POST` | `/vm/{id}/update` | Redeploy the recorded source and commit (`{"repo_url"?, "config_path"?, "refresh"?}`; NDJSON) |
| `DELETE` | `/vm/{id}` | Destroy + clean up resources. Optional `?keep_volumes=true\|false`: `true` keeps every managed `[[volumes]]` dir, `false` deletes them all. Omitted → each volume's `keep` field decides. Absolute `host =` binds are never deleted. Applies to both runtimes. Rejected with `400` in agent mode when the query is set. CLI: `russel destroy <id> [--keep-volumes\|--delete-volumes]` |
| `GET` | `/vms` | List services `{vms[], services[{service_id, runtime, status}]}` |
| `GET` | `/status` | Single-service shim; `404` if none, `400` if >1 (pass `/vm/{id}/status`) |
| `GET` | `/logs` | Single-service shim; same `404`/`400` rule |
| `GET` | `/secrets` | List secret **names** (values never returned) |
| `POST` | `/secrets/{name}` | Set secret (`{"value":"…"}`) |
| `DELETE` | `/secrets/{name}` | Delete secret |

Secret `set` validation failures → `400`; I/O failures → `500`.

## Requests

`DeployRequest` (`crates/core/src/api.rs`):

```json
{
  "repo_url": "https://github.com/you/app.git",
  "config_path": "Russelfile.toml",
  "vm_id": "api",
  "rev": "0123456789abcdef0123456789abcdef01234567",
  "force": false
}
```

- `rev` is optional: build this commit (full 40-character lowercase id) instead of the source's current HEAD. A local path is cloned for it, never checked out in place. Update and rollback send the recorded commit.
- `force` is optional (default `false`). Without it, a request whose source resolves to the commit and Russelfile the service already runs (a clean tree, service deployed and running) completes at once with status `unchanged` and changes nothing.
- `vm_id` is optional. When present, it must equal Russelfile `service.name`; it checks the name and does not override it. The response `service_id` is `service.name`. Any other body field (including `port`, `host`, `runtime`, `env`, or `podman_args`) is rejected as unknown with HTTP `422`.
- `--config` in the CLI corresponds to `config_path`; it defaults to `Russelfile.toml`.

- Remote deploys accept `https://`, `http://`, `ssh://`, `git@host:path` only. Link-local / private / metadata hosts (e.g. `169.254.169.254`, RFC1918) are blocked. Local absolute paths need `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane and resolve on the **ctrl host**.

`RollbackRequest`: `{"version": 3}` or `{}` (latest `previous` with `rollback_ready`); it rebuilds that generation's recorded commit. `UpdateRequest`: `{"repo_url": "…", "config_path": "…", "refresh": true}` (all optional). Update rebuilds the recorded commit unless `refresh` is true or `repo_url` is given.

`DeployResponse` carries `rev`, the commit the generation was built from (omitted outside git). Each deployment history row's `desired_state` records `rev` and `dirty` (the tree had uncommitted changes).

## NDJSON streams

```
{"type":"Progress","stage":"resolve","message":"cloned …"}
{"type":"Progress","stage":"build","message":"nix build …"}
{"type":"Progress","stage":"create","message":"…"}
{"type":"Progress","stage":"start","message":"…"}
{"type":"Progress","stage":"ready","message":"…"}
{"type":"Complete","status":"deployed","service_id":"api","elapsed_ms":1540}
```

or:

```
{"type":"Error","message":"…","stage":"build"}
```

Stages: `resolve → build → create → start → ready → complete`. `rolled_back` arrives as a terminal status with a **non-zero CLI exit** so CI notices. Keep proxies unbuffered (Caddy `flush_interval -1`, nginx `proxy_buffering off`) or streams stall.

`DeployEvent`: `Progress | Complete | Error`. Timing fields are `u64` ms on the wire (JS-safe).

## Status shapes

`StatusResponse` includes `service_id`, `status`, `vm_state`, `uptime_seconds`, runtime, ports, optional `route_host`, and `restarts` (microVM relaunches under `restart = "unless-stopped"` since the last deploy; omitted when zero). In-process status takes `route_host` from desired state first, then the service's Traefik JSON. Agent-proxied status leaves it `null` because the agent wire type does not carry a route name. `VmsResponse` aggregates memory + disk + podman. `DeploymentsResponse` returns the newest-first journal (cap 20).

Agent lifecycle (experimental, not in the ctrl router): live `GET /agent/v1/health|heartbeat`, `POST /agent/v1/stop|destroy/{id}`, `GET /agent/v1/status/{id}`; `POST /agent/v1/deploy` and `GET /agent/v1/logs/{id}` return `501`.

## cURL cookbook

```bash
TOKEN="$(cat ~/.config/russel/env | sed -n 's/^RUSSEL_API_TOKEN=//p')"
H="Authorization: Bearer $TOKEN"

curl -s -H "$H" http://127.0.0.1:7878/vms
curl -s -H "$H" http://127.0.0.1:7878/vm/api/status
curl -s -H "$H" http://127.0.0.1:7878/vm/api/deployments

curl -N -H "$H" -H 'Content-Type: application/json' \
  -d '{"repo_url":"https://github.com/you/app.git","config_path":"Russelfile.toml","vm_id":"api"}' \
  http://127.0.0.1:7878/deploy

curl -N -H "$H" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:7878/vm/api/rollback

printf '%s' "$VAL" | curl -s -H "$H" -H 'Content-Type: application/json' \
  --data-binary @- http://127.0.0.1:7878/secrets/DB_PASSWORD  # shell: build JSON safely
```

Prefer `russel secrets set` over hand-built JSON (stdin, never argv).

## Related

- [CLI](./cli.md) · [Lifecycle](../concepts/lifecycle.md) · [Update and rollback](../guides/update-rollback.md) · [Environment](./environment.md)
