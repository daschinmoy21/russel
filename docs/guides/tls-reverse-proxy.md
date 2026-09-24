---
title: TLS reverse proxy
description: Terminate TLS for russel-ctrl with Caddy or nginx — loopback ctrl, /api stripping, unbuffered NDJSON.
sidebar_position: 2
keywords: [tls, caddy, nginx, reverse proxy, https, api stripping, ndjson]
---

# Control-plane TLS

`russel-ctrl` speaks plain HTTP and has no built-in TLS. For split access, keep it on `127.0.0.1:7878` and terminate TLS at Caddy or nginx. The CLI refuses to send a Bearer token over cleartext HTTP to a non-loopback host.

## Recommended layout

```text
CLI / dashboard
      | HTTPS + Bearer
      v
Reverse proxy :443
      | HTTP on loopback
      v
russel-ctrl 127.0.0.1:7878
```

The dashboard and CLI use the same HTTPS origin in topology C. The dashboard token is entered at runtime and stored in tab-scoped `sessionStorage`; CLI login writes the CLI config and does not fill the dashboard.

On other Linux, install the loopback control plane with `./contrib/install.sh host`. On NixOS use `services.russel`. Keep the env file mode `0600` and fail-closed auth on.

## Caddy

Ctrl already serves the dashboard at `/` and the API at both `/vms` and `/api/vms`. A proxy can forward the whole origin to loopback:

```caddyfile
russel.example.com {
    reverse_proxy 127.0.0.1:7878 {
        flush_interval -1
    }
}
```

Serving `dashboard/dist` yourself and stripping `/api` still works:

```caddyfile
russel.example.com {
    root * /srv/russel/dashboard/dist

    handle_path /api/* {
        reverse_proxy 127.0.0.1:7878 {
            flush_interval -1
        }
    }

    handle {
        try_files {path} {path}/ /index.html
        file_server
    }
}
```

`flush_interval -1` keeps deploy NDJSON progress streams unbuffered. Caddy obtains and renews certificates when DNS points at the host.

## nginx

The rewrite changes `/api/vms` to `/vms` before proxying to `127.0.0.1:7878/`. Static dashboard stays on the same HTTPS origin:

```nginx
server {
    listen 443 ssl http2;
    server_name russel.example.com;

    ssl_certificate     /etc/letsencrypt/live/russel.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/russel.example.com/privkey.pem;
    root /srv/russel/dashboard/dist;

    location /api/ {
        rewrite ^/api/(.*)$ /$1 break;
        proxy_pass http://127.0.0.1:7878;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Authorization $http_authorization;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }

    location / {
        try_files $uri $uri/ /index.html;
    }
}
```

`/api/` must keep `proxy_buffering off` so deploy NDJSON events arrive as emitted. If the dashboard uses the bare `/api` path, add a matching exact-location rewrite to `/`.

Minimal ctrl-only proxy (no dashboard) is the same `location /` variant with `proxy_buffering off` + long `proxy_read_timeout` — see the pre-stack `security-tls.md` history if needed.

## Traefik for the control plane

Point a separate router at loopback ctrl (do not expose `:7878` publicly). Do not confuse this with app ingress:

```yaml
http:
  routers:
    russel-ctrl:
      rule: "Host(`russel.example.com`)"
      entryPoints: ["websecure"]
      service: russel-ctrl
      tls:
        certResolver: letsencrypt
  services:
    russel-ctrl:
      loadBalancer:
        servers:
          - url: "http://127.0.0.1:7878"
```

If Traefik already runs for app ingress, add this router alongside the per-service `Host(<id>.<domain>)` files Russel writes.

## SSH alternative

Without a public cert, use the installer-managed tunnel:

```bash
./contrib/install.sh connect user@host
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
```

Manual equivalent (loopback-anchored, fail if the forward fails):

```bash
ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:7878:127.0.0.1:7878 user@host
```

Keep the tunnel open for CLI/Vite dashboard. Dashboard Settings `/api`.

## Checklist

- [ ] `russel-ctrl` binds `127.0.0.1:7878`
- [ ] Strong `RUSSEL_API_TOKEN` from a private mode-`0600` env file
- [ ] Proxy terminates TLS; firewall blocks direct `:7878`
- [ ] `/api/` strips its prefix before forwarding to ctrl
- [ ] Caddy low-latency flushing or nginx `proxy_buffering off` for deploy streams
- [ ] CLI + dashboard use the HTTPS origin in topology C
- [ ] Dashboard token entered at runtime; CLI login does not populate it

## Related

- [Installation](../getting-started/installation.md) · [Single-VPS checklist](vps-one-dev.md) · [Security overview](../security/overview.md)
