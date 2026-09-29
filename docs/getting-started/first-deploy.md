---
title: First deploy
description: Add a Russelfile to your own app, deploy it from git, and give it env vars, secrets, and extra Podman flags.
sidebar_position: 2
keywords: [deploy, russel init, env, secrets, podman args, update]
---

This page takes your own app from `russel init` to a running, verified deploy.

## What you'll need

- A working install: `russel ps` answers ([Installation](./installation.md)).
- Your app in a git repo the server can clone.

## 1. Write a Russelfile

```bash
cd my-app
russel init
```

`init` guesses the name from `Cargo.toml`, `go.mod`, or the folder name, and writes a `Russelfile.toml` with every field explained in comments. It won't overwrite an existing file unless you pass `--force`. Useful flags:

```bash
russel init --port 8080 --memory 512mb   # set the port and memory limit
russel init --with-flake                 # also write a starter flake.nix you can edit
russel init --type microvm               # experimental: needs /dev/kvm and passt
```

A minimal Russelfile looks like this:

```toml
[service]
name = "my-app"        # the service id you use with every other command
source = "."           # folder to build, relative to this file
port = 3000            # the port your app listens on
memory = "256mb"
bin = "my-app"         # the binary the build produces
```

Your app must listen on `0.0.0.0` and the port in `$PORT`. If the repo has no `flake.nix`, Russel generates one for Rust, Go, and static sites ([Builds](../concepts/builds.md)). Every field is in the [Russelfile reference](../reference/russelfile.md).

Commit the Russelfile and push.

## 2. Deploy

```bash
russel deploy https://github.com/you/my-app.git
```

The CLI shows each phase (`resolve`, `build`, `create`, `start`, `ready`) and prints the host port when it finishes.

| Option | Meaning |
|---|---|
| `REPO` | A git URL: `https://…`, `ssh://…`, or `git@host:path`. |
| `--config PATH` | The Russelfile's path inside the repo (default `Russelfile.toml`). The build runs in that file's folder, so one repo can hold several services: `--config services/api/Russelfile.toml`. |
| `--force` | Redeploy even when the service already runs this commit and Russelfile. |

Running `deploy` again with the same commit and Russelfile does nothing. After the first deploy, use [`russel update`](#6-update).

**Deploying a folder on the server.** `REPO` can also be a folder path, but the control plane refuses it unless `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` is set in `/etc/russel/env` (then `sudo systemctl restart russel-ctrl`). The path is read on the server, never on your laptop. Only turn this on for a server you alone use.

## 3. Env vars and secrets

Plain settings go in `[service.env]`:

```toml
[service.env]
LOG_LEVEL = "debug"
DB_PASSWORD = "secret://DB_PASSWORD"
```

Secrets stay on the server, never in the repo. Store one by piping it in, so it never shows up in your shell history or process list:

```bash
printf '%s' "$DB_PASSWORD" | russel secrets set DB_PASSWORD
russel secrets list          # shows names only
```

`secret://DB_PASSWORD` is replaced with the stored value at deploy time. Russel sets `PORT` itself and rejects a few other names; the full list and limits are in [Env and secrets](../guides/env-secrets.md).

After changing env or a secret, redeploy:

```bash
russel update my-app --refresh
```

## 4. Program arguments and Podman flags

These are two separate lists:

- **`service.args`** are passed to your program, after its name: `args = ["--dir", "/data"]`. Works on both runtimes.
- **`service.podman_args`** are extra flags for `podman run`, one flag or value per entry. Containers only. Your program never sees them.

```toml
[service]
podman_args = ["--tmpfs", "/cache"]
```

Russel rejects Podman flags that it sets itself or that would weaken isolation (such as `--privileged` or `--network=host`), and `-v` may only mount a `/nix/store` path read-only. For host directories, use [`[[volumes]]`](../reference/russelfile.md#volumes).

## 5. Check it

```bash
russel ps                       # PORTS shows host port → app port
curl http://127.0.0.1:<host-port>/health
russel status my-app
russel logs my-app
```

`status` and `logs` take the service name. With only one service deployed you can leave it out.

## 6. Update

```bash
russel update my-app --refresh   # build and ship the latest commit
russel update my-app             # rebuild the running commit, for example after changing a secret
```

The new version starts next to the old one, and traffic switches once it answers. More, including rollback: [Update and rollback](../guides/update-rollback.md).

## 7. Stop or remove

```bash
russel stop my-app      # stop the app, keep its history so update or rollback can bring it back
russel destroy my-app   # stop it and remove its route, ports, files, and history
```

## Troubleshooting

| Error | Cause | Fix |
|---|---|---|
| `local absolute path deploys are disabled` | You deployed a folder path | Deploy a git URL, or enable local paths as described in step 2 |
| `unknown variant` in `service.type` | A value other than `container` or `microvm` | Use `container` or `microvm` |
| `env key '…' is reserved and cannot be set by user` | A name Russel sets itself, like `PORT` | Rename the variable ([reserved names](../guides/env-secrets.md#env-rules)) |
| `secret "NAME" not found` | `secret://NAME` with nothing stored | `russel secrets set NAME` |
| `podman passthrough arg denied for security` | A blocked flag in `podman_args` | Remove the flag; use `[[volumes]]` or `[ingress]` instead |

More: [Troubleshooting](../guides/troubleshooting.md).

## Next steps

- [Dashboard](./dashboard.md): see the same deploy in the browser.
- [Russelfile reference](../reference/russelfile.md): every field and rule.
- [Traefik ingress](../guides/traefik-ingress.md): give the app a host name.
