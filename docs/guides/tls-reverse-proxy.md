---
title: TLS reverse proxy
description: Reach the control plane and dashboard over HTTPS by putting Caddy, nginx, or Traefik in front of it.
sidebar_position: 2
keywords: [tls, https, caddy, nginx, traefik, reverse proxy]
---

`russel-ctrl` only speaks plain HTTP on `127.0.0.1:7878`. To reach it from elsewhere without an SSH tunnel, put a reverse proxy on the same server that handles HTTPS and forwards to it.

```text
CLI or browser ──HTTPS──> proxy :443 ──HTTP──> russel-ctrl 127.0.0.1:7878
```

The control plane serves the dashboard at `/` and the API at both `/` and `/api/`, so the proxy forwards everything as-is. The only special setting: turn off response buffering, because deploys stream their progress line by line, and a buffering proxy holds it all back until the deploy ends.

## What you'll need

- A working install ([Installation](../getting-started/installation.md)).
- A DNS name for the server, such as `russel.example.com`.
- Ports 80 and 443 open. Keep 7878 closed.

## Caddy

Caddy gets and renews the certificate on its own once DNS points at the server.

```caddyfile
russel.example.com {
    reverse_proxy 127.0.0.1:7878 {
        flush_interval -1
    }
}
```

`flush_interval -1` sends deploy progress through as it happens.

## nginx

```nginx
server {
    listen 443 ssl http2;
    server_name russel.example.com;

    ssl_certificate     /etc/letsencrypt/live/russel.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/russel.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:7878;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
```

`proxy_buffering off` streams deploy progress, and the long `proxy_read_timeout` keeps slow first builds from being cut off.

## Traefik

If you already run Traefik for your apps, add a router for the control plane to its dynamic config. Put it in its own file, outside the folder Russel writes to:

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

Traefik streams responses by default, so no extra setting is needed.

## Log in

```bash
russel login https://russel.example.com --token-file ~/.config/russel/env
russel ps
```

For the dashboard, open `https://russel.example.com/`, go to **Settings**, paste the token, and leave the API address at `/api`.

## Checklist

- [ ] `russel-ctrl` listens on `127.0.0.1:7878` only, and the firewall blocks 7878.
- [ ] The token file stays private: the installer's `/etc/russel/env` is `root:russel`, mode `0640`.
- [ ] The proxy doesn't buffer responses.
- [ ] `russel origin` shows the HTTPS address and says it's reachable.

## Related

- [Installation](../getting-started/installation.md) · [Dashboard](../getting-started/dashboard.md) · [Single-VPS checklist](./vps-one-dev.md) · [Security overview](../security/overview.md)
