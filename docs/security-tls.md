# Control-plane TLS (reverse proxy runbook)

Russel’s control plane (`russel-ctrl`) speaks **plain HTTP**. There is no
built-in TLS. For any non-local use, **terminate TLS at a reverse proxy** and
keep `russel-ctrl` on loopback.

The CLI **refuses** to send `RUSSEL_API_TOKEN` over `http://` to a non-loopback
host. Escape hatch (not recommended): `--insecure` or `RUSSEL_INSECURE_CLEARTEXT=1`.

## Recommended layout

```
Client (CLI / dashboard)
    │  HTTPS + Bearer
    ▼
Reverse proxy (Caddy / nginx / Traefik)  :443
    │  HTTP (loopback only)
    ▼
russel-ctrl  127.0.0.1:7878
```

```bash
# Server
export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"   # ≥32 chars
export RUSSEL_CTRL_ADDR=127.0.0.1:7878
export RUSSEL_REQUIRE_AUTH=1                        # optional, fail-closed on loopback
./russel-ctrl

# Client
export RUSSEL_CONTROL_PLANE=https://russel.example.com
export RUSSEL_API_TOKEN='…same token…'
russel ps
```

## Caddy

```caddyfile
russel.example.com {
    reverse_proxy 127.0.0.1:7878
}
```

Caddy obtains and renews certificates automatically when DNS points at the host.

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
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        # Deploy streams NDJSON — disable buffering for progress events
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
```

## Traefik (file / Docker labels)

Point a router at the loopback control plane. Example dynamic file provider:

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

If Traefik already runs for app ingress, add a separate router for the control
plane API (do not expose `:7878` on a public interface).

## Alternatives without a public cert

- **SSH tunnel:** `ssh -L 7878:127.0.0.1:7878 user@host` then
  `RUSSEL_CONTROL_PLANE=http://127.0.0.1:7878` (loopback; CLI allows cleartext).
- **Private network only:** still prefer HTTPS; if you must use plain HTTP on a
  private IP, the CLI requires `--insecure` / `RUSSEL_INSECURE_CLEARTEXT=1`.

## Checklist

- [ ] `russel-ctrl` bound to `127.0.0.1` (not `0.0.0.0`)
- [ ] Strong `RUSSEL_API_TOKEN` (≥32 chars); prefer `RUSSEL_REQUIRE_AUTH=1`
- [ ] Proxy terminates TLS; firewall blocks direct access to `:7878`
- [ ] Clients use `https://…` for `RUSSEL_CONTROL_PLANE` / dashboard API URL
- [ ] Dashboard / CLI never store tokens in public env that ships to browsers
      (`PUBLIC_RUSSEL_API_TOKEN` is discouraged)
