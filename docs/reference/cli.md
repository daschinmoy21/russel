---
title: CLI reference
description: Every russel command, flag, env var, and exit behavior.
sidebar_position: 1
keywords: [cli, russel init, login, deploy, ps, status, logs, secrets, update]
---

# CLI reference

Binaries are `russel` (client) and `russel-ctrl` (control plane). Crate name stays `russel-cli`; the **command** is `russel`. Install: [Installation](../getting-started/installation.md). Building from source is covered in [Contributing](../project/development.md).

## `russel-ctrl`

```bash
russel-ctrl
russel-ctrl --debug
russel-ctrl --no-dashboard
russel-ctrl --dashboard-dir /usr/local/share/russel/dashboard
russel-ctrl --log-file /var/lib/russel/ctrl.log
```

| Flag / env | Default | Meaning |
|---|---|---|
| `--debug` | off | Stream logs to stderr. The log file always gets `info,russel_ctrl=debug` (or `RUST_LOG`) |
| `--no-dashboard` | off | API only |
| `--dashboard-dir PATH` / `RUSSEL_DASHBOARD_DIR` | search (share path, checkout `dashboard/dist`) | Built Astro dist (`index.html` at the root). Missing explicit dir fails start; missing search result starts without UI |
| `--log-file PATH` / `RUSSEL_CTRL_LOG` | `/var/lib/russel/ctrl.log` if writable, else `$XDG_STATE_HOME/russel/ctrl.log` | Append, mode `0600`. Quiet stderr is warnings/errors unless `--debug` |

`russel-ctrl` serves the dashboard on GET `/` (and `/services`, `/deploy`, `/settings`, …) and keeps the CLI API on `/vms`, POST `/deploy`, and the same routes under `/api/` for the UI. Bind is still `RUSSEL_CTRL_ADDR` (`127.0.0.1:7878`).

## Global options

| Flag / env | Default | Meaning |
|---|---|---|
| `--version` | — | Print the crate version (`0.1.0` on this release) and exit |
| `--control-plane URL` / `RUSSEL_CONTROL_PLANE` | `russel login` config, else `http://127.0.0.1:7878` | Which ctrl to hit |
| `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1\|true\|yes` | off | Allow Bearer over plain HTTP to non-loopback (not recommended; prefer HTTPS) |
| `RUSSEL_API_TOKEN` | — | Bearer token; **wins** over the login file |
| `RUSSEL_CONFIG_DIR` | `~/.config/russel` | Override config dir (tests) |

Auth resolution: `RUSSEL_API_TOKEN` env → `~/.config/russel/config.toml` (mode `0600`) from `russel login` → unauthenticated (loopback dev mode only).

## Commands

```bash
russel init [DIR] [--name NAME] [--port PORT] [--memory MEM] [--type|--runtime RUNTIME]
  [--bin BIN] [--package ATTR] [--with-flake] [--force]
russel login [<url>] [--token-file PATH]
russel logout
russel origin
russel deploy <repo> [-p HOST:GUEST] [--config PATH] [--vm-id ID]
  [--host HOST] [--runtime microvm|container] [--env KEY=VALUE...] [--env-file PATH]
  [-- <podman-args...>]
russel status [<service_id>]
russel logs [<service_id>]
russel ps                      # aliases: list, vms
russel stop <service_id>
russel destroy <service_id> [--keep-volumes | --delete-volumes]
russel update <service_id> [--repo REPO] [--config PATH]
russel secrets set <name>      # value from stdin
russel secrets list
russel secrets delete <name>
```

### `russel init [DIR=. ]`

Scaffold `Russelfile.toml` (and optionally `flake.nix`). Documents every field + flag in comments. Never overwrites without `--force`.

| Flag | Default | Meaning |
|---|---|---|
| `DIR` | `.` | Directory to init (created if missing) |
| `--name` | Inferred from `Cargo.toml` / `go.mod` / dir | `service.name` |
| `--port` | `3000` | `service.port` |
| `--memory` | `256mb` | `service.memory` |
| `--type` / `--runtime` | `microvm` | `service.type` |
| `--bin` | `service.name` | `service.bin` |
| `--package ATTR` | — | nixpkgs attr for containers without a `flake.nix` (e.g. `--package navidrome`); implies `--type container`. Written as `service.package`. Cannot combine with `--with-flake`. |
| `--with-flake` | off | Also write starter `flake.nix` (Rust/Go/static). Cannot combine with `--package`. |
| `--force` | off | Overwrite existing files |

Reserved dir names (`secrets`, `traefik`, `_pool`) are rejected as service names.

### `russel login [<url>] [--token-file PATH]`

Store URL + token in `~/.config/russel/config.toml` (`0600`). Token from file (bare token or `RUSSEL_API_TOKEN=…` line), env, or stdin prompt. No token printed.

```bash
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel login https://russel.example.com --token-file ~/.config/russel/env
```

### `russel logout` / `russel origin`

`logout` removes the login file. `origin` prints the resolved URL, auth source (env vs file vs none), and reachability — the first diagnostic alongside `install.sh status`.

### `russel deploy <repo>`

Deploy or redeploy a service. Streams NDJSON; exit `0` on `deployed`, non-zero on `failed` **or** `rolled_back` (so CI notices rollbacks).

| Flag | Meaning |
|---|---|
| `REPO` | `https://…`, `http://…`, `ssh://…`, `git@host:path`, or local absolute path (ctrl opt-in only). Relative paths always rejected. |
| `-p` / `--publish HOST:GUEST` | Publish a host port (repeatable in API; CLI takes one). Optional behind Traefik. |
| `--host HOST` | Exact Traefik `Host()` name. Must match `[ingress].host`; omitting it lets the Russelfile supply the name. A file with no host rejects a CLI-only `--host`. |
| `--vm-id ID` | `[A-Za-z0-9-_]` max 128. Derived from repo when omitted; **required by the API**. |
| `--config PATH` | Repo-relative Russelfile (default `Russelfile.toml`). `openat` + `O_NOFOLLOW`, 1 MiB cap, symlinks rejected. |
| `--runtime` | Must match `service.type` when both set. Check, not override. |
| `--env KEY=VALUE` | Repeatable; overrides file + env-file. |
| `--env-file PATH` | `KEY=VALUE` file (`#` comments, blanks skipped). |
| `-- …` | Container-only podman passthrough (validated, fail-closed). |

Remote ctrls need git URLs — local paths resolve on the **ctrl host** and need `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` there (trusted single-tenant only).

`service.port` is the guest listen port. `[ingress].port` is the optional
host-side backend pin used by Traefik. `-p HOST:GUEST` can provide that pin
when the file omits it; when both are present, its host number must match
`ingress.port`, and its guest number must match `service.port`. A deploy with
neither pin uses an allocated backend port.

### `russel status [<id>]` / `russel logs [<id>]`

`GET /vm/{id}/status` + `/logs`, or the single-service shims `/status` + `/logs` when no id is given (`404` when empty, `400` when >1 service). Logs append `podman logs` for containers.

### `russel ps` (aliases `list`, `vms`)

`GET /vms` → `{vms[], services[{service_id, runtime, status}]}` from memory + disk + podman. Human table; empty state names the tunnel/proxy when unreachable.

### `russel stop <id>` / `russel destroy <id>`

`POST /vm/{id}/stop` (keep metadata/history, remove Traefik file) vs `DELETE /vm/{id}` (stop + remove ports, rootfs/TAP, metadata). Both are idempotent-ish; destroying an unknown id errors clearly.

`destroy` volume flags (container `[[volumes]]` only; microVMs ignore them):

| Flag | Meaning |
|---|---|
| `--keep-volumes` | Keep every managed volume dir (`/var/lib/russel/<id>/volumes/*`) |
| `--delete-volumes` | Delete every managed volume dir, even `keep = true` ones |

With neither flag, each volume's `keep` field decides. Absolute `host =` binds are never deleted. See [API](api.md) (`DELETE /vm/{id}?keep_volumes=`) and [Russelfile](russelfile.md).

### `russel update <id>`

Re-apply recorded `repo_url` / `config_path` (+ overrides). Same NDJSON stream as deploy. See [Update and rollback](../guides/update-rollback.md).

### `russel secrets …`

```bash
printf '%s' "$VAL" | russel secrets set NAME
russel secrets list      # names only
russel secrets delete NAME
```

Value from stdin, never argv. Use `secret://NAME` in env maps.

## Fish

Fish does not `export` `KEY=VALUE` files. Do not `source` an env file:

```fish
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

## Exit codes and output

- `deploy`/`update` stream `Progress` lines then `Complete{status: deployed}` or `Error{…}`. `rolled_back` still exits non-zero.
- `truncate_for_error` caps error strings at 256 chars (multibyte-safe).
- There is no `russel build/develop/check/exec/scale/node/sandbox` verb — builds run inside `deploy`.

## Related

- [First deploy](../getting-started/first-deploy.md) · [API](api.md) · [Russelfile](russelfile.md) · [Troubleshooting](../guides/troubleshooting.md)
