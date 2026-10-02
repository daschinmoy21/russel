---
title: Environment reference
description: Every RUSSEL_* setting for the control plane and the CLI, where to put it, and what it does.
sidebar_position: 4
keywords: [environment, settings, env vars, RUSSEL_CTRL_ADDR, RUSSEL_API_TOKEN, traefik, health]
---

## Where settings go

The control plane runs as a systemd service, so setting a variable in your shell doesn't reach it. Put control-plane settings in `/etc/russel/env`, one `KEY=value` per line, then restart:

```bash
echo 'RUSSEL_HEALTH_RESTART=1' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

On NixOS, use `services.russel.extraEnvironment` (or the file named by `services.russel.environmentFile`) and rebuild.

CLI settings are ordinary environment variables in your shell.

Yes/no settings accept `1`, `true`, or `yes` (any case) unless the table says otherwise.

## Control plane

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_API_TOKEN` | none | The API token. At least 32 printable characters. The installer generates it. |
| `RUSSEL_REQUIRE_AUTH` | on in the installed service | Refuse to start without a valid token, even on loopback. |
| `RUSSEL_CTRL_ADDR` | `127.0.0.1:7878` | Address to listen on. A non-loopback address requires a token. Keep it on loopback and use a proxy or tunnel. |
| `RUSSEL_DATA_DIR` | `/var/lib/russel` | Where all state lives. Must be an absolute path; keep it mode `0700`. |
| `RUSSEL_CTRL_LOG` | `/var/lib/russel/ctrl.log` | Log file. |
| `RUSSEL_DASHBOARD_DIR` | `/usr/local/share/russel/dashboard` | Where the dashboard files are. |
| `RUSSEL_MAX_CONCURRENT_DEPLOYS` | `4` | How many deploys can run at once. More return `503`. |
| `RUSSEL_SECRETS_DIR` | `$RUSSEL_DATA_DIR/secrets` | Where secrets are stored. |
| `RUSSEL_NODE_ID` | The hostname | Name for this server, recorded with each deployment. |

## Deploys

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` | off (`1` turns it on) | Allow deploying an absolute folder path on the server. Only for a server you alone use. |
| `RUSSEL_NIX_RESTRICTED` | off (`1` turns it on) | Force Nix's sandbox and stop generating flakes. Use it when you build repos you don't fully trust. See [Nix build security](../security/nix-builds.md). |
| `RUSSEL_GIT_HOST_ALLOWLIST` | none | Comma-separated git host names that may resolve to private addresses, for an internal git server. Literal private IP addresses stay blocked. |
| `RUSSEL_ALLOW_PODMAN_ARGS` | on | Set to `0` to reject every `service.podman_args` entry. |
| `RUSSEL_VOLUME_ROOTS` | none | Colon-separated folders under which `[[volumes]]` may use `host =`, such as `/srv/data:/mnt/media`. Without it, `host =` volumes are rejected. |

## Networking and Traefik

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_PUBLISH_BIND` | `127.0.0.1` | Address app ports are published on. `0.0.0.0` for all interfaces, or a Tailscale address for tailnet-only access. |
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `$RUSSEL_DATA_DIR/traefik/dynamic` | Where Traefik route files are written. Must match Traefik's `providers.file.directory`, and Traefik must be able to read it. |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Host name suffix for services without `[ingress].host`. |
| `RUSSEL_TRAEFIK_TLS` | off | Add HTTPS and a certificate resolver to every route. |
| `RUSSEL_TRAEFIK_CERT_RESOLVER` | `letsencrypt` | Must match the resolver name in Traefik's config. |
| `RUSSEL_TRAEFIK_BACKEND` | `RUSSEL_PUBLISH_BIND` | Address Traefik uses to reach apps. Only needed when Traefik runs in its own network namespace. |
| `RUSSEL_TRAEFIK_ENTRYPOINT` | `http://127.0.0.1:80` (best effort) | Traefik entry point an update sends requests through to check that Traefik serves the new version before the old one is retired. A set value makes the check strict; `off` skips it. See [What zero downtime covers](../concepts/lifecycle.md#what-zero-downtime-covers). |

See [Traefik ingress](../guides/traefik-ingress.md).

## Health

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_HEALTH_INTERVAL_SECS` | `30` | Seconds between health checks of each service. |
| `RUSSEL_HEALTH_RESTART` | off (`1` turns it on) | Redeploy a service after 3 failed checks in a row. |

## MicroVMs

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_KERNEL_PATH` | none | Kernel to boot. Checked first. |
| `RUSSEL_KERNEL_POOL` | `/var/lib/russel/_pool/kernel/bzImage` | Kernel used when `RUSSEL_KERNEL_PATH` is unset. If neither exists, Russel builds `.#microvm-kernel`, and fails if it can't. It never uses a stock kernel. |
| `RUSSEL_MICROVM_NET` | `passt`, or `tap` when running as root | `passt` for unprivileged networking, `tap` for the root-only TAP setup. |
| `RUSSEL_VIRTIOFS_SANDBOX` | `namespace`, or `chroot` as root | Sandbox mode for `virtiofsd`. |
| `RUSSEL_MICROVMS_DIR` | `$RUSSEL_DATA_DIR/_microvms` | Folder for microVM marker files. |
| `RUSSEL_FORWARD` | filter on | Root/TAP mode only: `allow` turns off the firewall chain that keeps VMs from reaching each other and other hosts. For debugging only. |
| `RUSSEL_DISABLE_FORWARD_FILTER` | off | Same as `RUSSEL_FORWARD=allow`, and removes rules Russel installed earlier. |

## Running as root

These only matter when `russel-ctrl` runs as root, which is for development and benchmarks:

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_PODMAN_USER` | `SUDO_USER` | The account whose rootless Podman runs containers. |

## CLI

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_CONTROL_PLANE` | The `russel login` address, else `http://127.0.0.1:7878` | Control plane to talk to. `--control-plane` overrides it. |
| `RUSSEL_API_TOKEN` | none | Token to send. Overrides the one saved by `russel login`. |
| `RUSSEL_INSECURE_CLEARTEXT` | off | Allow sending the token over plain HTTP to another host. Avoid it. |
| `RUSSEL_CONFIG_DIR` | `~/.config/russel` | Where `russel login` saves its file. |

## Experimental

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_WARM_POOL` | off | Keep a paused microVM ready to speed up boots. Has known bugs; the installed service forces it off. |
| `RUSSEL_CPU_MAX` | `8` | Most vCPUs a warm-pool VM can grow to. |
| `RUSSEL_MEM_HOTPLUG_MB` | `2048` | Extra memory a warm-pool VM can grow by. |
| `RUSSEL_AGENT_URL` | none | Send stop, destroy, and status to a node agent at this address. For multi-host experiments. |
| `RUSSEL_AGENT_TOKEN` | `RUSSEL_API_TOKEN` | The agent's token. |
| `RUSSEL_AGENT_ADDR` | `127.0.0.1:7946` | Address the agent listens on. |
| `RUSSEL_NODE_LABELS` | none | `key=value,key2=value2` labels the agent reports. |

## Related

- [Installation](../getting-started/installation.md) · [Networking](../concepts/networking.md) · [API](./api.md) · [Security overview](../security/overview.md)
