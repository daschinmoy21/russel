---
title: Troubleshooting
description: Find the cause of connection, login, deploy, and runtime errors, matched by the message you see.
sidebar_position: 6
keywords: [troubleshooting, errors, 401, offline, connection refused, deploy failed, podman]
---

Start with these two commands. They show which control plane the CLI talks to, whether it answers, and whether the service is running:

```bash
russel origin
systemctl status russel-ctrl     # on NixOS: systemctl status russel
```

Then find your message below.

## Connecting and logging in

| You see | Cause | Fix |
|---|---|---|
| `connection refused` to `127.0.0.1:7878` | Nothing is listening on this machine. On a laptop, the SSH tunnel is down; on the server, `russel-ctrl` isn't running. | Laptop: run `install.sh connect user@server` again. Server: `sudo systemctl start russel-ctrl`, and `journalctl -u russel-ctrl` for why it stopped. Don't start a second `russel-ctrl` on the laptop. |
| `401` in the CLI or dashboard | The control plane is up but rejected the token | `russel login <url> --token-file …` again. In the dashboard, paste the token into Settings again. |
| Dashboard says **Offline** | The tunnel or proxy isn't reachable | Check the tunnel, or the proxy's config and logs ([TLS reverse proxy](./tls-reverse-proxy.md)). |
| `refusing to send RUSSEL_API_TOKEN` … | You pointed the CLI at `http://` on another host | Use an `https://` address or the SSH tunnel. `--insecure` overrides this, but sends the token in the clear. |
| `russel-ctrl` won't start, and the journal mentions the token | The token is missing, shorter than 32 characters, or has unprintable characters | Fix `RUSSEL_API_TOKEN` in `/etc/russel/env` and restart. |
| Errors when you `source` the env file in fish | fish can't read `KEY=VALUE` files | Use `russel login … --token-file …`, which works in every shell. |

`russel login` saves the address and token to `~/.config/russel/config.toml`. A `RUSSEL_API_TOKEN` environment variable overrides it.

## Deploying

| You see | Cause | Fix |
|---|---|---|
| `local absolute path deploys are disabled` | You deployed a folder path | Deploy a git URL. To allow folders on a server only you use, set `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` in `/etc/russel/env` and restart. |
| `local path '…' is relative; use an absolute path` | You passed a relative folder path | Use a git URL, or an absolute path with local deploys enabled. |
| `repository URL host … is a private (RFC1918) address` or `… is a cloud metadata service` | Russel refuses to clone from internal addresses | Use a public git host. |
| `env key '…' is reserved and cannot be set by user` | A name Russel sets itself, like `PORT` | Rename it. The list is in [Env and secrets](./env-secrets.md#env-rules). |
| `secret "NAME" not found` | `secret://NAME` with nothing stored | `russel secrets set NAME` |
| `too many concurrent deploys (max 4); retry later` | Four deploys are already running | Wait and retry, or raise `RUSSEL_MAX_CONCURRENT_DEPLOYS`. |
| The build fails with `nix: not found` | Nix was installed for one user, so the `russel` account can't use it | Reinstall Nix with `--daemon`. `install.sh check` tells you which install you have. |
| ``unknown field `database` `` | `[database]` tables were removed | Delete the table, and run Postgres or Redis as their own service (`examples/postgres`, `examples/redis`). |
| `guest = "linux"` … `is not implemented yet` | Only the default guest works today | Remove `guest`, or set `guest = "busybox"`. |
| `podman passthrough arg denied for security` | A blocked flag in `service.podman_args` | Remove it. Use `[[volumes]]` for host folders and `[ingress]` for ports. |
| Deploy progress hangs behind a proxy | The proxy buffers the response | Caddy: `flush_interval -1`. nginx: `proxy_buffering off`. |
| The app is on a different port after an update | Each update publishes the new version on a fresh port | Check `russel ps`, or reach the app by name through Traefik. To keep one port, pin it with `[ingress].port`. A pinned service updated with a pre-release build may have left its pin; one `russel update` puts it back. |

## Running apps

| You see | Cause | Fix |
|---|---|---|
| `app did not accept connections on port N within 30s` | The container is up, but nothing answers on `service.port`: the app is still starting, uses another port, or listens on `127.0.0.1` | Listen on `0.0.0.0` and the port in `$PORT`. Check `service.port` matches. |
| `app exited during startup (exit code N)` or `container crashed during startup (…)` | The app exited before it answered, or within 2 s after, on an update | Read the output quoted with the error, or `russel logs <id>`. Fix the config, args, or env. On an update, the previous version keeps serving. |
| `new version crashed within 2s of answering` | An update's new version answered, then died | Same fix as above. Nothing to recover: the previous version never stopped. |
| `deploy` said `deployed`, but `russel ps` shows `failed` soon after | A first deploy's app crashed right after it answered | `russel logs <id>`, fix it, push, and `russel update <id> --refresh`. See [When a deploy counts as ready](../concepts/lifecycle.md#when-a-deploy-counts-as-ready). |
| A service turns `failed` later | It exited, or failed 3 health checks in a row | `russel logs <id>`. Set `RUSSEL_HEALTH_RESTART=1` to redeploy automatically. |
| `Read-only file system` in the app's output | The app writes outside `/tmp`, `/run`, or a volume | Add a `[[volumes]]` entry with `rw = true` for that path. |
| Podman errors about `/run/user`, cgroups, or `newuidmap` | Rootless Podman isn't fully set up for the `russel` account | Run `install.sh check`. Then, as that account: `sudo -u russel XDG_RUNTIME_DIR=/run/user/$(id -u russel) podman info`. `loginctl show-user russel -p Linger` should say `yes`. |
| `crun: error creating systemd unit libpod-….scope` | A control plane from a pre-release build | Upgrade: re-run `install.sh host` with `RUSSEL_VERSION=v0.1.0` or newer. |
| `Failed to mount empty tmpfs for pivot_root()`, or `mkdir /var/tmp/…: no such file or directory` | A pre-release service unit (with `PrivateTmp=yes`) left Podman in a broken state | Re-run `install.sh host` with `RUSSEL_VERSION=v0.1.0` or newer. See [systemd and NixOS](../operations/systemd-nixos.md). |
| Traefik doesn't route | Traefik reads a different folder, can't read it, or DNS is wrong | Check that `<id>.json` appears in the folder set in `providers.file.directory`, that Traefik can read it, and that the name resolves to the server. See [Traefik ingress](./traefik-ingress.md). |

## MicroVMs

| You see | Cause | Fix |
|---|---|---|
| `microVMs are experimental and need …` | No read-write `/dev/kvm`, or `passt` is missing | Use containers (the default), or add `russel` to the `kvm` group and install `passt`. |
| `passt exited … before creating …` although the port is free | The socket path is longer than Linux allows (107 bytes) | Use a shorter `RUSSEL_DATA_DIR`. |
| The VM hangs at boot with `cpus` > 1 | `cloud-hypervisor` older than v52 | Upgrade `cloud-hypervisor`. |
| The VM doesn't boot | Wrong kernel | Use Russel's kernel: `/var/lib/russel/_pool/kernel/bzImage`, or `RUSSEL_KERNEL_PATH`. |

## Collecting details for a bug report

```bash
russel origin
russel ps
russel status <id>
russel logs <id>
journalctl -u russel-ctrl --since '15 min ago'
sudo ls -l /var/lib/russel/<id>/
```

Include the command you ran and the full error output. Never paste your token.

## Related

- [Installation](../getting-started/installation.md) · [TLS reverse proxy](./tls-reverse-proxy.md) · [Lifecycle](../concepts/lifecycle.md)
