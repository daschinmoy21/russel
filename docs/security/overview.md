---
title: Security overview
description: Threat model, auth, transport, input validation, secrets, and residual risks.
sidebar_position: 1
keywords: [security, threat model, auth, bearer, ssrf, secrets, isolation]
---

# Security overview

Russel is a privileged daemon for a **single trusted operator**. It is not multi-tenant. Harden the host, keep ctrl on loopback, and only deploy trusted repos unless `RUSSEL_NIX_RESTRICTED=1`.

## Trust boundaries

| Layer | Mechanism |
|---|---|
| API auth | Bearer on every route when `RUSSEL_API_TOKEN` set (≥32 chars after trim, printable ASCII, constant-time compare). Non-loopback bind refuses to start without a token. `RUSSEL_REQUIRE_AUTH=1` fails closed even on loopback. Loopback without a token is dev mode (warns). |
| Transport | HTTP-only ctrl. Terminate TLS at Caddy/nginx/Traefik or use the anchored SSH tunnel. CLI/dashboard refuse Bearer over `http://` to non-loopback unless `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1`. |
| Input validation | `service_id` `[A-Za-z0-9-_]` max 128; `bin` `[A-Za-z0-9._+-]` max 256; config path `openat` + `O_NOFOLLOW` chain, 1 MiB cap, symlinks rejected; env key/value rules (reserved incl. `IFS`/`PATH`/`LD_*`, no NUL/newline, 4096 B, 64 keys); secret-name charset; podman passthrough allowlist (Russel-owned + isolation-weakening flags rejected). |
| SSRF guard | Repo URLs `https/http/ssh/git@` only; literal-IP hosts checked against link-local + cloud-metadata ranges (all schemes). `repo_url` userinfo redacted in logs/metadata. |
| Secrets | Host store `0600`/`0700`, atomic writes, names-only over API, resolved at deploy, never in argv. microVM `deploy.env` is `0600`; containers get `secret://` values as Podman secrets (not in argv or `podman inspect`); plain `-e` values are visible via `podman inspect`. |
| Workload isolation | microVM: KVM boundary, ro store share, no guest shell. Container: rootless, `--cap-drop ALL`, `no-new-privileges`, `--read-only`, tmpfs `/tmp` + `/run`. |
| Host integrity | Orphan-only TAP cleanup; no host-chain iptables mutation; CH socket + service dirs `0700`; PID ownership via `/proc/<pid>/cmdline` before signals; kill + wait with timeout; deploy semaphore (default 4) + per-service guard; `flock` single-instance. |

## What is still open

- DNS-rebinding around the SSRF guard (no DNS resolution), metadata-IP redirects during clone.
- Plain (non-`secret://`) container env is visible via `podman inspect` (by design; use `secret://` for anything sensitive).
- Nix builds trust source repos (see [Nix builds](nix-builds.md)).
- No native ctrl TLS, no CORS, no deb/OCI packaging, no multi-tenant isolation, no managed DBs.

## Operator rules

1. Bind `127.0.0.1:7878`; expose via proxy/tunnel only.
2. Strong token + `RUSSEL_REQUIRE_AUTH=1`; env file `0600`; `/var/lib/russel` `0700`.
3. Firewall blocks `:7878` directly; proxy strips `/api`, keeps NDJSON unbuffered.
4. Git URLs remotely; local paths only on trusted hosts with the explicit opt-in.
5. `type = "container"` on no-KVM VPS; microVMs only where KVM/TAP/iptables are privileged.

## Related

- [Nix builds](nix-builds.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md) · [Env and secrets](../guides/env-secrets.md) · [Environment](../reference/environment.md)
