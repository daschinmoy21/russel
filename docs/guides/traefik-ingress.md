---
title: Traefik ingress
description: Route per-service Host names to Russel backends with the file provider.
sidebar_position: 3
keywords: [traefik, ingress, file provider, host routing, tls, acme]
---

Russel integrates with Traefik v2 as the primary HTTP reverse proxy (file provider). No API calls — Russel writes dynamic config files; Traefik watches the directory.

> **Architecture note:** Deploy uses the `Ingress` trait; `TraefikFileIngress` is the default implementation. Future proxies implement the same trait — the pipeline never imports Traefik types directly.

## What you'll need

- Traefik v2 binary.
- Russel writing to `/var/lib/russel/traefik/dynamic` (or `RUSSEL_TRAEFIK_DYNAMIC_DIR`).
- A DNS record mapping each service name to Traefik's IP. DNS remains the
  operator's job; Russel does not create records.

## Static Traefik config

Save as `traefik.yml`:

```yaml
entryPoints:
  web:
    address: ":80"

providers:
  file:
    directory: /var/lib/russel/traefik/dynamic
    watch: true
```

```bash
traefik --configFile=traefik.yml
```

## Russel environment

| Variable | Default | Description |
|---|---|---|
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `$RUSSEL_DATA_DIR/traefik/dynamic` (`/var/lib/russel/traefik/dynamic`) | Must match `providers.file.directory` |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Default suffix for `Host()` rules when `[ingress].host` is omitted (validated as a DNS name) |
| `RUSSEL_TRAEFIK_BACKEND` | publish bind (`RUSSEL_PUBLISH_BIND`) | Host Traefik dials for a published backend port. Set it when Traefik runs in a different netns than the backend — e.g. rootless Podman Traefik reaching a host-published port via `10.89.0.1`. Leave unset when Traefik shares the backend's netns (wildcard binds map to loopback). |
| `RUSSEL_TRAEFIK_TLS` | off | `1` adds `websecure` + `tls.certResolver` to each router |
| `RUSSEL_TRAEFIK_CERT_RESOLVER` | `letsencrypt` (fallback when blank) | Must match the static-config resolver name |

## Per-service host and backend

Each Russelfile may choose the exact Traefik name for one service:

```toml
[ingress]
host = "api.example.com"
port = 4000 # optional; host-side backend, like -p HOST:guest
```

`ingress.host` is used exactly in `Host()` (after canonical lowercase). It can
be an apex name or a subdomain; it is not combined with the service id. If it
is omitted, the route uses `<service_id>.<RUSSEL_TRAEFIK_DOMAIN>`. `service.port`
still names the guest listen port and remains distinct from `ingress.port`.

Use the host name alone for normal browser or `curl https://api.example.com`
traffic through Traefik. Pin `ingress.port` when a local script needs a stable
backend such as `curl localhost:4000`, a firewall rule needs one port, or a
non-HTTP publisher needs a fixed publish port. Traefik continues to listen on
its configured 80/443 entrypoints; `ingress.port` is not a Traefik listener.

## DNS

Traefik routes by `Host` header:

```text
api.example.com  →  Traefik's IP
```

Create the DNS records before public traffic or ACME validation. DNS is still
the operator's job.

## How it works

1. **Deploy**: `russel apply` → ctrl writes `/var/lib/russel/traefik/dynamic/<service_id>.json` (atomic temp + rename).
2. **Traefik picks up** the router + service (file watch, sub-second).
3. **Access**: `curl http://<service-host>`.
4. **Stop/destroy**: ctrl removes the file → Traefik stops routing. Redeploy re-registers the new backend port.

No reload, no API calls, no Traefik binary dependency in Russel.

## Example dynamic config (generated)

```json
{
  "http": {
    "routers": {
      "russel-api": {
        "rule": "Host(`api.russel.local`)",
        "entryPoints": ["web"],
        "service": "russel-api"
      }
    },
    "services": {
      "russel-api": {
        "loadBalancer": {
          "servers": [{ "url": "http://127.0.0.1:3100" }]
        }
      }
    }
  }
}
```

## TLS / ACME

Use a **public DNS name** (not `russel.local`); ports 80/443 must be reachable for HTTP-01:

```bash
export RUSSEL_TRAEFIK_DOMAIN=example.com
export RUSSEL_TRAEFIK_TLS=1
export RUSSEL_TRAEFIK_CERT_RESOLVER=letsencrypt
```

```yaml
entryPoints:
  web:
    address: ":80"
  websecure:
    address: ":443"

certificatesResolvers:
  letsencrypt:
    acme:
      email: you@example.com
      # mode 0600, owned by the Traefik user, persistent storage:
      #   install -m 600 -o traefik -g traefik /dev/null /var/lib/traefik/acme.json
      storage: /var/lib/traefik/acme.json
      httpChallenge:
        entryPoint: web

providers:
  file:
    directory: /var/lib/russel/traefik/dynamic
    watch: true
```

When `RUSSEL_TRAEFIK_TLS=1`, each service router uses `web` + `websecure` with `tls.certResolver` set. ACME HTTP-01 requests a certificate for the exact name in `Host()`; that name must resolve to this Traefik instance and reach port 80.

## Redeploys, pins, and uniqueness

During a dual-live redeploy, the candidate always gets an ephemeral backend.
Traefik switches to that backend while keeping the configured `Host()` stable.
If a host-side port is pinned, Russel tries to reclaim it for a microVM after
cutover; containers keep the candidate publish port. This container reclaim
limit does not change the stable Host route.

The metadata desired-state record stores the operator's pin, if any. A journal
row records the live backend that actually served that generation, so a dual-
live row may show an ephemeral port while desired state still names the pin.

Russel rejects a route when another service's Traefik JSON already claims the
same name. Register and deregister take a per-directory write lock around the
uniqueness scan and the file update. Two concurrent deploys cannot both pass
the scan.

To drop a custom name, delete the `[ingress]` table from the Russelfile and run
`russel apply .`; the next write returns the service to the configured default
suffix.

App tunnels are not configured in the Russelfile yet; putting host-wide
cloudflared in front of Traefik is an operator-level choice.

## Related

- [Networking](../concepts/networking.md) · [TLS reverse proxy](./tls-reverse-proxy.md) · [Environment](../reference/environment.md)
