# API and CLI

The control plane listens on `127.0.0.1:7878` by default (`RUSSEL_CTRL_ADDR`).
JSON over HTTP. `POST /deploy` (and update/rollback) return an NDJSON event stream.

## Authentication

When `RUSSEL_API_TOKEN` is set on the control plane, **every** route requires
`Authorization: Bearer <token>`. The token must be **at least 32 characters**
after trim (`openssl rand -hex 32`). The CLI sends `RUSSEL_API_TOKEN` when set,
otherwise a token saved by `russel login` for that origin.

- Loopback + no token: dev mode (warning).
- Non-loopback: token required or ctrl **refuses to start**.
- `RUSSEL_REQUIRE_AUTH=1` (or `true`/`yes`): refuse startup without a token even on loopback.

`russel-ctrl` is HTTP-only. The CLI and dashboard **refuse** to send the token
over `http://` to a non-loopback host. Terminate TLS at Caddy/nginx/Traefik in
front of loopback ctrl — [security-tls.md](security-tls.md). Override only with
`--insecure` or `RUSSEL_INSECURE_CLEARTEXT=1`.

| Scenario | Behaviour |
|----------|-----------|
| Loopback + no token | Dev mode (warn, no auth) |
| Loopback + `RUSSEL_REQUIRE_AUTH` + no token | **Refuses to start** |
| Loopback + `RUSSEL_API_TOKEN` (≥32 chars) | Bearer on all routes |
| Any bind + token set but &lt;32 chars | **Refuses to start** |
| Non-loopback + no token | **Refuses to start** |
| Non-loopback + `RUSSEL_API_TOKEN` (≥32 chars) | Bearer on all routes |

## Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/deploy` | Deploy or re-deploy (`vm_id` required; NDJSON) |
| `GET` | `/secrets` | List secret names (values never returned) |
| `POST` | `/secrets/{name}` | Set secret (`{"value":"..."}`); store mode `0600` |
| `DELETE` | `/secrets/{name}` | Delete a secret |
| `GET` | `/status` | Single registered service; **400** if several exist |
| `GET` | `/logs` | Single registered service; **400** if several exist |
| `GET` | `/vm/{service_id}/status` | Status |
| `GET` | `/vm/{service_id}/logs` | Logs |
| `GET` | `/vm/{service_id}/deployments` | History (newest first) |
| `POST` | `/vm/{service_id}/rollback` | Rollback (`{"version":N}` optional; NDJSON) |
| `GET` | `/vms` | List services |
| `POST` | `/vm/{service_id}/stop` | Stop |
| `POST` | `/vm/{service_id}/update` | Redeploy from recorded source (NDJSON) |
| `DELETE` | `/vm/{service_id}` | Destroy and clean up |

Successful deploys append a row to `/var/lib/russel/<id>/deployments.json` (cap 20).
Statuses: `active`, `previous`, `superseded`, `rolled_back`.
`POST /vm/{id}/rollback` without `version` picks the latest `previous` with
`rollback_ready`. Missing source → **409**.

## CLI

Binaries: `russel` (crate `russel-cli`) and `russel-ctrl`.

**Global options:**

- `--control-plane URL` (`RUSSEL_CONTROL_PLANE`, then `russel login` config, default `http://127.0.0.1:7878`)
- `--insecure` — allow Bearer over plain HTTP to non-loopback hosts (`RUSSEL_INSECURE_CLEARTEXT=1`)

```bash
russel login [<url>] [--token-file PATH]
russel logout
russel origin
russel deploy <repo> [-p HOST:GUEST] [--config PATH] [--vm-id ID] \
  [--runtime microvm|container] [--env KEY=VALUE...] [--env-file PATH] \
  [-- <podman-run-args...>]          # container only, after --
russel init [--type microvm|container] [--with-flake]
russel status [<service_id>]
russel logs [<service_id>]
russel ps                            # aliases: list, vms
russel stop <service_id>
russel destroy <service_id>
russel update <service_id> [--repo REPO] [--config PATH]
russel secrets set <name>            # value from stdin
russel secrets list
russel secrets delete <name>
```

`russel login` writes `~/.config/russel/config.toml` (mode 0600). The saved token
is sent only to that origin. `RUSSEL_API_TOKEN` still wins and is not origin-bound.

- `--env` / `--env-file` merge over `[service.env]` (later wins). Reserved keys
  (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are rejected.
- Secrets: `printf '%s' "$VAL" | russel secrets set NAME`. In env maps use
  `secret://NAME` — resolved at deploy time from `/var/lib/russel/secrets/` (`0600`).
- `--runtime` is **not** an override. If set, it must match `service.type` (default `microvm`).
- `-p HOST:GUEST` publishes a host port. Traefik is the primary HTTP gateway; `-p` is optional for HTTP apps.
- Remote deploys: `https://`, `http://`, `ssh://`, `git@host:path` only. Link-local / private / metadata hosts are blocked.
- Local absolute path deploys are **off** unless `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane (paths resolve on the **ctrl host**). Prefer a git URL for a remote ctrl.

`--config` must be a relative path under the repo root (`openat` + `O_NOFOLLOW`, 1 MiB cap).
The binary name (`bin` or `name`) must match `[A-Za-z0-9._+-]` (max 256).

`russel update <id>` re-applies the last successful `repo_url` / `config_path`.
A failed redeploy after a prior success attempts rollback; CLI exit is non-zero.

Health: TCP probe of `127.0.0.1:<host_port>` every `RUSSEL_HEALTH_INTERVAL_SECS` (default 30).
Three failures → `failed`. `RUSSEL_HEALTH_RESTART=1` auto-redeploys from recorded source.
