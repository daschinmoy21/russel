---
title: Troubleshooting
description: Diagnose connection, auth, deploy, and runtime failures with anchored commands.
sidebar_position: 6
keywords: [troubleshooting, diagnostics, origin, status, 401, offline, cleartext, ports]
---

# Troubleshooting

Start with the two anchored diagnostics, then match your symptom below.

```bash
russel origin                # url, auth source, reachable?
./contrib/install.sh status  # endpoint, listener (:7878), unit, CLI origin
```

Loopback connect errors mean **nothing is listening on this machine**. Check the anchored SSH forward plus `systemctl --user status russel-ctrl` (and `systemctl status russel-ctrl` for a NixOS system unit). Remote errors name VPN/TLS. Do not start a second ctrl on the client. The instance lock names systemd and `ss`.

## Connection and auth

| Symptom | Meaning | Fix |
|---|---|---|
| `refusing to send RUSSEL_API_TOKEN over cleartext HTTP to non-loopback` | CLI cleartext guard (#189) | Use `https://…`, an SSH tunnel to loopback, or (not recommended) `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1` |
| `401` in CLI/dashboard | Unauthorized, **not** offline | Re-enter token; `russel login <url> --token-file …`; dashboard token is `sessionStorage`-scoped |
| `offline` / connection refused on loopback | No listener on this machine | Start ctrl (`systemctl --user status russel-ctrl`), or bring up `install.sh connect user@host` |
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
| `--runtime mismatch` | Flag vs `service.type` | Align them; the flag checks, never overrides |
| `invalid env key / reserved` | Reserved or bad charset | See [Env and secrets](env-secrets.md) reserved list |
| `secret not found` | `secret://NAME` with no host secret | `russel secrets set NAME` on the ctrl host |
| `config path … O_NOFOLLOW / too large` | Symlink or >1 MiB Russelfile | Keep `--config` repo-relative, real file, \<1 MiB |
| `bin name …` | Bad charset/length | `[A-Za-z0-9._+-]`, max 256 |
| `guest = "linux" is not implemented` | Linux guest requested | Omit `guest` or set `busybox` |
| `[database.*] enabled` rejected | Vestigial stub | Remove the section or set `enabled = false`; run DBs separately |
| `repository host blocked` | SSRF guard (link-local/metadata/private) | Use a public git host; do not deploy from metadata IPs |
| `503 … semaphore` | Max 4 concurrent deploys | Retry; tune `RUSSEL_MAX_CONCURRENT_DEPLOYS` |
| Deploy NDJSON stalls behind proxy | Buffered proxy | Caddy `flush_interval -1` / nginx `proxy_buffering off` + long `proxy_read_timeout` |

Repo URLs accept `https://`, `http://`, `ssh://`, `git@host:path` only. `repo_url` userinfo is redacted in logs/metadata.

## Runtime

| Symptom | Meaning | Fix |
|---|---|---|
| `podman info` not rootless | Rootful podman | Configure rootless; when ctrl runs as root set `RUSSEL_PODMAN_USER`/`SUDO_USER`; `loginctl enable-linger $USER` on headless hosts |
| `passthrough rejected` | Blocked `-- …` flag | Drop Russel-owned or isolation-weakening flags |
| MicroVM boot hangs | Missing KVM/TAP/iptables perms or wrong kernel | Check `/dev/kvm`, TAP caps, `RUSSEL_KERNEL_PATH` → `.#microvm-kernel` `bzImage` |
| `port already in use` | Allocator vs external listener | `ss -ltnp`, free the port or let the allocator pick (omit `-p` behind Traefik) |
| Service `failed` after 3 probes | App not listening on `PORT` | Check `russel logs`, guest `PORT` binding, `RUSSEL_HEALTH_INTERVAL_SECS` / `RUSSEL_HEALTH_RESTART=1` |
| Traefik never routes | Wrong dynamic dir/domain | Match `RUSSEL_TRAEFIK_DYNAMIC_DIR` to `providers.file.directory`; check `<id>.json` appears; add `/etc/hosts` or wildcard DNS |

## Collecting evidence

```bash
russel status <id> && russel logs <id> && russel ps
curl http://127.0.0.1:7878/vms
curl http://127.0.0.1:7878/vm/<id>/deployments
ss -ltnp 'sport = :7878'
systemctl --user status russel-ctrl
journalctl --user -u russel-ctrl --since '15 min ago'
ls -l /var/lib/russel/<id>/  # metadata.json, console.log/container.log, deployments.json
```

File issues with the command, full NDJSON tail, `russel origin` output, and the relevant log tail — never paste token values.

## Related

- [Installation](../getting-started/installation.md) · [TLS reverse proxy](tls-reverse-proxy.md) · [API](../reference/api.md)
