---
title: Troubleshooting
description: Diagnose connection, auth, deploy, and runtime failures with anchored commands.
sidebar_position: 6
keywords: [troubleshooting, diagnostics, origin, status, 401, offline, cleartext, ports]
---

Start with the two anchored diagnostics, then match your symptom below.

```bash
russel origin                # url, auth source, reachable?
./contrib/install.sh status  # endpoint, listener (:7878), unit, CLI origin
```

Loopback connect errors mean **nothing is listening on this machine**. Check the anchored SSH forward plus `systemctl status russel-ctrl` (`systemctl status russel` on NixOS). Remote errors name VPN/TLS. Do not start a second ctrl on the client. The instance lock names systemd and `ss`.

## Connection and auth

| Symptom | Meaning | Fix |
|---|---|---|
| `refusing to send RUSSEL_API_TOKEN over cleartext HTTP to non-loopback` | CLI cleartext guard (#189) | Use `https://…`, an SSH tunnel to loopback, or (not recommended) `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1` |
| `401` in CLI/dashboard | Unauthorized, **not** offline | Re-enter token; `russel login <url> --token-file …`; dashboard token is `sessionStorage`-scoped |
| `offline` / connection refused on loopback | No listener on this machine | Start ctrl (`sudo systemctl start russel-ctrl`; `journalctl -u russel-ctrl` for why it stopped), or bring up `install.sh connect user@host` |
| `offline` on remote | Tunnel/proxy down | Check SSH forward (`-L 127.0.0.1:7878:…`, `BatchMode`, `ExitOnForwardFailure`) or Caddy/nginx (TLS, `/api` strip, unbuffered NDJSON) |
| `Non-loopback bind requires RUSSEL_API_TOKEN` | Ctrl refuses to start | Set a ≥32-char printable-ASCII token; prefer `RUSSEL_REQUIRE_AUTH=1` even on loopback |
| `RUSSEL_REQUIRE_AUTH is set but … missing` | Fail-closed on loopback | Provide the token env/file |
| Fish `source` errors | Fish does not `export` KEY=VALUE files | `russel login … --token-file …` instead of `source` |

`russel login` writes `~/.config/russel/config.toml` (mode `0600`). Env wins over the file. `--token-file` accepts a bare token or `RUSSEL_API_TOKEN=…`.

## Deploy

| Symptom | Meaning | Fix |
|---|---|---|
| `relative paths are rejected` | CLI sent a relative path | Use an absolute trusted-host path (with opt-in) or a git URL |
| `local path deploys … disabled` | Remote ctrl without opt-in | Use `https://…`/`ssh://…`/`git@…`; enable `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` only on single-tenant trusted hosts |
| `invalid env key / reserved` | Reserved or bad charset | See [Env and secrets](./env-secrets.md) reserved list |
| `secret not found` | `secret://NAME` with no host secret | `russel secrets set NAME` on the ctrl host |
| Build fails with `nix` not found | The service only looks in the Nix daemon profile, `/nix/var/nix/profiles/default/bin` | Install Nix with the daemon installer (`--daemon`); a single-user install lives in one user's home. `install.sh check` tells you which one you have |
| `passt exited … before creating …/passt.sock (is host port … free?)` with a free port (microVMs) | The socket path is longer than the 107-byte limit for Unix sockets | Use a shorter `RUSSEL_DATA_DIR` |
| App answers on a different port after a redeploy | Unpinned host ports are picked fresh | `russel ps` shows the current port; pin one with `[ingress].port`, or route by name with Traefik |
| `config path … O_NOFOLLOW / too large` | Symlink or >1 MiB Russelfile | Keep `--config` repo-relative, real file, \<1 MiB |
| `bin name …` | Bad charset/length | `[A-Za-z0-9._+-]`, max 256 |
| `guest = "linux" is not implemented` | Linux guest requested | Omit `guest` or set `busybox` |
| ``unknown field `database` `` | `[database.*]` was removed | Delete the table; run Postgres/Redis as their own `service.package` service (`examples/postgres`, `examples/redis`) |
| `repository host blocked` | SSRF guard (link-local/metadata/private) | Use a public git host; do not deploy from metadata IPs |
| `503 … semaphore` | Max 4 concurrent deploys | Retry; tune `RUSSEL_MAX_CONCURRENT_DEPLOYS` |
| Deploy NDJSON stalls behind proxy | Buffered proxy | Caddy `flush_interval -1` / nginx `proxy_buffering off` + long `proxy_read_timeout` |

Repo URLs accept `https://`, `http://`, `ssh://`, `git@host:path` only. `repo_url` userinfo is redacted in logs/metadata.

## Runtime

| Symptom | Meaning | Fix |
|---|---|---|
| Podman errors about `/run/user`, cgroups, or `newuidmap` | The `russel` account's rootless Podman is missing a piece | Run `install.sh check`. Then look as the account itself: `sudo -u russel XDG_RUNTIME_DIR=/run/user/$(id -u russel) podman info`, and `loginctl show-user russel -p Linger` should say `yes` |
| `podman_args rejected` | Blocked Podman flag in `service.podman_args` | Drop Russel-owned or isolation-weakening flags |

| `container exited during startup (…, exit code N)`, `container crashed during startup`, or (microVMs) `app exited during startup (exit code N)` | The app exited (or crash-looped under a restart policy) before it accepted connections, or, on an update, within 2 s after it did | Read the quoted container output or guest console; fix config, args, or env. A live previous generation keeps serving (or is rolled back) |
| `new version crashed within 2s of answering; traffic is back on the previous version` | An update's new version answered, then died while the previous one was still running | Same fix as above. Nothing to recover: the previous version never stopped. See [When a deploy counts as ready](../concepts/lifecycle.md#when-a-deploy-counts-as-ready) |
| `deploy` said `deployed`, but `russel ps` shows `failed` soon after | A first deploy's app crashed right after answering. First deploys don't wait (nothing older to protect) | `russel logs <id>`; fix, commit, and `russel update <id> --refresh`. [When a deploy counts as ready](../concepts/lifecycle.md#when-a-deploy-counts-as-ready) |
| `app did not accept connections on port N within 30s` | The container is up but nothing answers on `service.port`: still starting, wrong port, or bound to `127.0.0.1` | Listen on `0.0.0.0:$PORT`; check `service.port` matches the app |
| `microVMs are experimental and need …` | MicroVM deploy on a ctrl without read-write `/dev/kvm`, or without `passt` on PATH (fails before the build) | Use `type = "container"` (the default), or add the ctrl user to the `kvm` group and install `passt` |
| `service.args requires service.type = "container"` | `args` on a microVM | Remove `args` or switch to `type = "container"`; the microVM guest does not pass argv yet |
| MicroVM boot hangs | Missing KVM/TAP/iptables perms or wrong kernel | Check `/dev/kvm`, TAP caps, `RUSSEL_KERNEL_PATH` → `.#microvm-kernel` `bzImage` |
| `port already in use` | Allocator vs external listener | `ss -ltnp`, free the port or let the allocator pick (omit `[ingress].port` behind Traefik) |
| Service `failed` after 3 probes | App not listening on `PORT` | Check `russel logs`, guest `PORT` binding, `RUSSEL_HEALTH_INTERVAL_SECS` / `RUSSEL_HEALTH_RESTART=1` |
| Traefik never routes | Wrong dynamic dir/domain | Match `RUSSEL_TRAEFIK_DYNAMIC_DIR` to `providers.file.directory`; check `<id>.json` appears; add `/etc/hosts` or wildcard DNS |

## Collecting evidence

```bash
russel status <id> && russel logs <id> && russel ps
curl http://127.0.0.1:7878/vms
curl http://127.0.0.1:7878/vm/<id>/deployments
ss -ltnp 'sport = :7878'
systemctl status russel-ctrl
journalctl -u russel-ctrl --since '15 min ago'
sudo ls -l /var/lib/russel/<id>/  # metadata.json, console.log/container.log, deployments.json (owned by russel)
```

File issues with the command, full NDJSON tail, `russel origin` output, and the relevant log tail — never paste token values.

## Related

- [Installation](../getting-started/installation.md) · [TLS reverse proxy](./tls-reverse-proxy.md) · [API](../reference/api.md)
