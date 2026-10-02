---
title: CLI reference
description: Every russel command and flag, how the CLI finds the control plane, and what it exits with.
sidebar_position: 1
keywords: [cli, russel init, login, deploy, ps, status, logs, secrets, update, rollback]
---

`russel` is the command-line client. It talks to a control plane, `russel-ctrl`, over HTTP. Install: [Installation](../getting-started/installation.md).

## Commands

```bash
russel init [DIR] [--name NAME] [--port PORT] [--memory MEM] [--type RUNTIME]
            [--bin BIN] [--package ATTR] [--with-flake] [--force]
russel login [URL] [--token-file PATH]
russel logout
russel origin
russel deploy REPO [--config PATH] [--force]     # also: russel apply
russel ps                                        # also: russel list
russel status [ID]
russel logs [ID]
russel update ID [--refresh] [--repo REPO] [--config PATH]
russel rollback ID [--version N] [--rebuild]
russel stop ID
russel destroy ID [--keep-volumes | --delete-volumes]
russel secrets set NAME                          # reads the value from stdin
russel secrets list
russel secrets delete NAME
```

`ID` is the service's `service.name` from its Russelfile.

## Options for every command

| Flag or variable | Default | Meaning |
|---|---|---|
| `--control-plane URL` / `RUSSEL_CONTROL_PLANE` | The `russel login` address, else `http://127.0.0.1:7878` | Which control plane to talk to |
| `RUSSEL_API_TOKEN` | none | Token to send. Overrides the one saved by `russel login`. |
| `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1` | off | Send the token over plain `http://` to a host other than loopback. Avoid this; use HTTPS or the SSH tunnel. |
| `--version` | | Print the version and exit |

## `russel init`

Writes a `Russelfile.toml` in `DIR` (default: the current folder), with every field explained in comments. It won't overwrite an existing file without `--force`.

| Flag | Default | Meaning |
|---|---|---|
| `--name` | From `Cargo.toml`, `go.mod`, or the folder name | `service.name`: letters, digits, `_`, and `-`, up to 128 characters |
| `--port` | `3000` | `service.port` |
| `--memory` | `256mb` | `service.memory` |
| `--type` | `container` | `service.type`. `microvm` is experimental and needs `/dev/kvm` and `passt`. |
| `--bin` | The name | `service.bin` |
| `--package ATTR` | | Run a nixpkgs package with no flake, such as `--package navidrome`. Can't be combined with `--with-flake`. |
| `--with-flake` | off | Also write a starter `flake.nix` for Rust, Go, or a static site |
| `--force` | off | Overwrite existing files |

## `russel login`, `logout`, `origin`

`login` saves the control plane's address and token to `~/.config/russel/config.toml` (mode `0600`). It reads the token from `--token-file` (a bare token or a `RUSSEL_API_TOKEN=…` line), from `RUSSEL_API_TOKEN`, or by prompting. It never prints the token.

```bash
russel login http://127.0.0.1:7878 --token-file /etc/russel/env
russel login https://russel.example.com --token-file ~/.config/russel/env
```

`logout` deletes the saved file. `origin` shows the address in use, where the token comes from, and whether the control plane answers. It's the first thing to run when something doesn't connect.

## `russel deploy`

Deploys the service named in the Russelfile. Use it for the first deploy, and `russel update` after that.

| Argument | Meaning |
|---|---|
| `REPO` | A git URL: `https://…`, `http://…`, `ssh://…`, or `git@host:path`. A folder path works only when the control plane allows it (`RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1`), and is read on the server. |
| `--config PATH` | The Russelfile's path inside the repo (default `Russelfile.toml`). It must be a real file, not a symlink, and under 1 MiB. The build runs in its folder. |
| `--force` | Deploy even when the service already runs this commit and Russelfile |

Deploying the commit and Russelfile a service already runs does nothing and reports `unchanged`. A folder with uncommitted changes, or one outside git, always deploys.

**Renaming a service.** `service.name` is the service's identity. To rename one, change the name, deploy it, then `russel destroy` the old name.

## `russel ps`, `status`, `logs`

`ps` lists every service with its runtime, status, host port, and uptime. `status` shows one service in detail, including its restart count. `logs` prints its recent output. With only one service deployed, `status` and `logs` work without an `ID`.

## `russel update`

Rebuilds and redeploys a running service.

- Without flags, it rebuilds the commit that's already running. Secrets are read again, so this is how you apply a changed secret.
- `--refresh` builds the latest commit: the default branch of a git URL, or `HEAD` of a folder. This is how you ship changes.
- `--repo` and `--config` deploy from a different source, and imply `--refresh`. The Russelfile's `service.name` must still match `ID`.

See [Update and rollback](../guides/update-rollback.md).

## `russel rollback`

Starts an earlier deployment's recorded build again, with the Russelfile it ran. Without `--version`, it picks the previous deployment. `--rebuild` builds that deployment's commit from source instead; use it when Nix has collected an old build. With `--rebuild`, a deployment made from uncommitted changes is rebuilt from what its folder holds now.

## `russel stop` and `destroy`

`stop` stops the app and removes its route, but keeps its history, so `update` or `rollback` can start it again. `destroy` also removes its ports, files, and history. Destroying an unknown service is an error.

`[[volumes]]` data on `destroy`:

| Flag | What happens to managed volumes |
|---|---|
| (none) | Each volume's `keep` setting decides |
| `--keep-volumes` | All are kept |
| `--delete-volumes` | All are deleted, even `keep = true` ones |

Absolute `host =` folders are never deleted.

## `russel secrets`

```bash
printf '%s' "$VALUE" | russel secrets set NAME
russel secrets list
russel secrets delete NAME
```

The value is read from stdin so it never appears on a command line. `list` shows names only. Use a secret with `secret://NAME` in `[service.env]`.

## Output and exit codes

`deploy`, `update`, and `rollback` print each stage as it happens. They exit `0` when the result is `deployed` or `unchanged`, and non-zero on failure. A deploy that failed and was rolled back (`rolled_back`) also exits non-zero, so scripts notice.

## `russel-ctrl`

The control plane. The installer runs it as a systemd service, so you rarely start it by hand.

| Flag or variable | Default | Meaning |
|---|---|---|
| `RUSSEL_CTRL_ADDR` | `127.0.0.1:7878` | Address to listen on |
| `--debug` | off | Also print logs to the terminal |
| `--log-file PATH` / `RUSSEL_CTRL_LOG` | `/var/lib/russel/ctrl.log` | Log file |
| `--no-dashboard` | off | Serve the API only |
| `--dashboard-dir PATH` / `RUSSEL_DASHBOARD_DIR` | `/usr/local/share/russel/dashboard` | Where the dashboard files are |

All settings: [Environment reference](./environment.md).

## Related

- [First deploy](../getting-started/first-deploy.md) · [API](./api.md) · [Russelfile](./russelfile.md) · [Troubleshooting](../guides/troubleshooting.md)
