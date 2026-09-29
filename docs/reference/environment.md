---
title: Environment reference
description: Every RUSSEL_* variable — control plane, CLI, networking, builds, and tuning.
sidebar_position: 4
keywords: [environment, env vars, RUSSEL_CTRL_ADDR, RUSSEL_API_TOKEN, traefik, health]
---

# Environment reference

All `RUSSEL_*` variables in one table. Booleans accept the unified truthy set (`1|true|yes`, case-insensitive) unless noted. Sources: `crates/ctrl/src/main.rs`, `network/ports.rs`, `traefik.rs`, `health.rs`, `build.rs`, `git/`, `warm_pool.rs`, `secrets.rs`, `agent/*`, `microvm/runner.rs`, `cli/commands.rs` + `config.rs`.

## Control plane core

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_CTRL_ADDR` | `127.0.0.1:7878` | Bind address. Non-loopback **requires** a valid token or ctrl refuses to start. Never bind publicly — use a proxy/tunnel. |
| `RUSSEL_CTRL_LOG` | `/var/lib/russel/ctrl.log` if writable, else `$XDG_STATE_HOME/russel/ctrl.log` | Ctrl log file (`--log-file`). Stderr stays at warn unless `russel-ctrl --debug`. |
| `RUSSEL_DASHBOARD_DIR` | search | Built dashboard dist (`--dashboard-dir`). Installer copies to `/usr/local/share/russel/dashboard`. |
| `RUSSEL_API_TOKEN` | — | Bearer token (≥32 chars after trim, printable ASCII, constant-time compare). Required on non-loopback; `RUSSEL_REQUIRE_AUTH=1` requires it even on loopback. |
| `RUSSEL_REQUIRE_AUTH` | off | `1\|true\|yes` → fail closed without a valid token, even on loopback. Recommended for production. |
| `RUSSEL_MAX_CONCURRENT_DEPLOYS` | `4` | Deploy semaphore size. Full → `503`. |
| `RUSSEL_NODE_ID` | hostname → `local` | Stable node identity recorded in metadata. |
| `RUSSEL_NODE_LABELS` | — | `key=val,key2=val2` operator labels (invalid pairs skipped). |
| `RUSSEL_DATA_DIR` | `/var/lib/russel` | Absolute state root for ctrl and the agent (service dirs, catalog, secrets default, warm pool). Relative values are ignored. Keep mode `0700`. |
| `RUSSEL_SECRETS_DIR` | `$RUSSEL_DATA_DIR/secrets` (`/var/lib/russel/secrets` when unset) | Secret store override (mainly tests). Files `0600`. |
| `RUSSEL_MICROVMS_DIR` | `$RUSSEL_DATA_DIR/_microvms`, or `/var/lib/microvms` when `RUSSEL_DATA_DIR` is unset | MicroVM marker dirs (legacy discovery). Creating them is best-effort. |

## CLI

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_CONTROL_PLANE` | login file → `http://127.0.0.1:7878` | Ctrl URL (`--control-plane` wins). |
| `RUSSEL_API_TOKEN` | — | Token; wins over the login file. |
| `RUSSEL_INSECURE_CLEARTEXT` | off | `1\|true\|yes` (+ `--insecure`) allows Bearer over plain HTTP to non-loopback. Not recommended. |
| `RUSSEL_CONFIG_DIR` | `~/.config/russel` | Config dir override (tests). |
| `RUSSEL_ALLOW_PODMAN_ARGS` | — | Gate for podman passthrough variants (where wired). Default posture is allowlist-deny. |

## Deploys and source

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` | off (`1` = on) | Allow local absolute repo paths (resolved on the **ctrl host**). Trusted single-tenant hosts only; prefer git URLs remotely. Relative paths always rejected. |
| `RUSSEL_GIT_HOST_ALLOWLIST` | built-in SSRF set | Extra allowed git hosts (where wired). Literal-IP hosts checked against link-local + metadata ranges regardless. |
| `RUSSEL_NIX_RESTRICTED` | off (`1` = on) | Sandboxed `nix build` + no auto-flake. Required posture for untrusted sources. See [Nix builds](../security/nix-builds.md). |
| `RUSSEL_KERNEL_PATH` | `.#microvm-kernel` build | MicroVM kernel `bzImage` (virtio built-in). Highest-priority explicit pin, checked before the kernel pool and the flake build. |
| `RUSSEL_KERNEL_POOL` | `/var/lib/russel/_pool/kernel/bzImage` | Kernel pool `bzImage` (virtio built-in), checked when `RUSSEL_KERNEL_PATH` is unset or missing, before `nix build .#microvm-kernel`. No kernel from any source is a hard error — ctrl never falls back to the stock nixpkgs linux kernel (drivers `=m` breaks microVM boot). |
| `RUSSEL_VIRTIOFS_SANDBOX` | `chroot` as root, else `namespace` | virtiofsd sandbox mode override (`none` is the escape hatch). |
| `RUSSEL_MICROVM_NET` | `tap` with `CAP_NET_ADMIN`, else `passt` | MicroVM networking: `passt` (unprivileged, vhost-user NIC + port publish) or `tap` (TAP + socat + `RUSSEL-FORWARD`, needs `CAP_NET_ADMIN`). Recorded per generation in metadata as `net`. |
| `RUSSEL_TEST_BUSYBOX` | — | Test-only busybox path. |

## Networking and ingress

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_PUBLISH_BIND` | `127.0.0.1` | Default bind for `-p` publishes. `0.0.0.0` for wildcard; Tailscale IP for tailnet-only. Health probes are bind-aware. |
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `$RUSSEL_DATA_DIR/traefik/dynamic` (`/var/lib/russel/traefik/dynamic`) | Must match Traefik `providers.file.directory`. |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Default suffix for `Host()` rules when `[ingress].host` is omitted (validated DNS name). A file host takes precedence. |
| `RUSSEL_TRAEFIK_BACKEND` | publish bind (`RUSSEL_PUBLISH_BIND`) | Host Traefik dials for a published backend port. Needed when Traefik runs in another netns than the backend (e.g. rootless Traefik → `10.89.0.1`). Wildcard binds map to loopback for a same-netns Traefik. See [Traefik ingress](../guides/traefik-ingress.md). |
| `RUSSEL_VOLUME_ROOTS` | — (unset) | Colon-separated absolute prefixes allowlisting absolute `host =` volume binds. Without it, absolute host binds are rejected at deploy. Example: `/srv/data:/mnt/media`. See [Russelfile](russelfile.md). |
| `RUSSEL_TRAEFIK_TLS` | off | `1` (+ truthy set incl. `on`) adds `websecure` + `tls.certResolver`. |
| `RUSSEL_TRAEFIK_CERT_RESOLVER` | `letsencrypt` fallback | Must match the static-config resolver name. |
| `RUSSEL_FORWARD` | filter on | `allow` skips the `RUSSEL-FORWARD` guest filter (single-tenant debug only — guests can pivot via host routing). |
| `RUSSEL_DISABLE_FORWARD_FILTER` | off | `1` — same escape hatch; best-effort removes previously installed Russel-owned rules. |
| `RUSSEL_FORWARD_CHAIN` | `RUSSEL-FORWARD` | iptables chain name override. |

## Health

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_HEALTH_INTERVAL_SECS` | `30` | TCP probe interval per service. |
| `RUSSEL_HEALTH_RESTART` | off (`1` = on) | Auto-redeploy from `desired_state` after 3 consecutive probe failures. |

## Runtime users and agents

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_PODMAN_USER` | `SUDO_USER` | User for rootless podman when ctrl runs as root. Must be rootless (`podman info`). |
| `RUSSEL_AGENT_URL` | — | When set, ctrl proxies stop/destroy/status to the node agent. |
| `RUSSEL_AGENT_TOKEN` | → `RUSSEL_API_TOKEN` | Agent bearer (preferred over the shared API token). |
| `RUSSEL_AGENT_ADDR` | `127.0.0.1:7946` | Agent bind. Same loopback/token discipline as ctrl. |

## Experimental tuning

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_WARM_POOL` | off (`1` = on) | Golden-VM snapshot/restore pool (known races, issue #101). |
| `RUSSEL_CPU_MAX` | `8` | CPU hotplug topology upper bound (warm pool). |
| `RUSSEL_MEM_HOTPLUG_MB` | `2048` | Memory hotplug headroom (warm pool). |

## Related

- [Installation](../getting-started/installation.md) · [Networking](../concepts/networking.md) · [API](api.md) · [Security overview](../security/overview.md)
