# Traefik Gateway Setup

Russel integrates with [Traefik](https://traefik.io/) v2 as the primary HTTP reverse proxy
(file provider). No API calls — Russel writes dynamic configuration files; Traefik watches the
directory and picks up changes automatically.

> **Architecture note:** Deploy uses the `Ingress` trait; `TraefikFileIngress` is the default
> implementation. Future proxies implement the same trait — the deploy pipeline never imports
> Traefik types directly.

## Static Traefik Configuration

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

Start Traefik:

```bash
traefik --configFile=traefik.yml
```

## Russel Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `/var/lib/russel/traefik/dynamic` | Must match Traefik's `providers.file.directory` |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Domain suffix for `Host()` rules |

## DNS / Hosts

Traefik routes by `Host` header. Add entries to `/etc/hosts`:

```text
127.0.0.1  api.russel.local  demo.russel.local
```

Or configure a wildcard DNS record for `*.russel.local` → your host IP.

## How It Works

1. **Deploy**: `russel deploy` → control plane writes `/var/lib/russel/traefik/dynamic/<service_id>.json`
2. **Traefik picks up** the new router + service (file watch, sub-second)
3. **Access**: `curl http://<service_id>.russel.local`
4. **Stop/Destroy**: control plane removes the file → Traefik stops routing

No reload, no API calls, no Traefik binary dependency in Russel.

## Example Dynamic Config (generated)

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
          "servers": [
            { "url": "http://127.0.0.1:3100" }
          ]
        }
      }
    }
  }
}
```

## TLS / ACME

Enable TLS on dynamic routers written by Russel:

```bash
export RUSSEL_TRAEFIK_TLS=1
export RUSSEL_TRAEFIK_CERT_RESOLVER=letsencrypt   # must match Traefik static config
```

Static Traefik config (example):

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
      storage: /var/lib/traefik/acme.json
      httpChallenge:
        entryPoint: web

providers:
  file:
    directory: /var/lib/russel/traefik/dynamic
    watch: true
```

When `RUSSEL_TRAEFIK_TLS=1`, each service router uses entryPoints `web` +
`websecure` and sets `tls.certResolver` to `RUSSEL_TRAEFIK_CERT_RESOLVER`.
