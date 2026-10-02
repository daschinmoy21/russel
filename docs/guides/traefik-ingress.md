---
title: Traefik ingress
description: Give each app its own host name, with HTTPS from Let's Encrypt, using Traefik and the route files Russel writes.
sidebar_position: 3
keywords: [traefik, ingress, host routing, tls, acme, lets encrypt, dns]
---

Russel writes a Traefik route file for every service it deploys. Point Traefik's file provider at that folder and each app is reachable by name, such as `api.example.com`, with no reloads or API calls.

## What you'll need

- Traefik v2 or v3, installed on the same server and running as a service.
- A DNS record for each app name, pointing at the server. Russel doesn't manage DNS.
- Ports 80 and 443 open.

## 1. Give Traefik a folder it can read

By default Russel writes routes to `/var/lib/russel/traefik/dynamic/`. `/var/lib/russel` is private to the `russel` account (mode `0700`), so a Traefik running as its own user can't read it. Give the routes their own folder, writable by `russel` and readable by Traefik's group:

```bash
sudo install -d -o russel -g traefik -m 750 /var/lib/russel-routes
echo 'RUSSEL_TRAEFIK_DYNAMIC_DIR=/var/lib/russel-routes' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

Replace `traefik` with the group your Traefik runs as. Russel writes each file with mode `0644`.

## 2. Configure Traefik

A minimal static config (`/etc/traefik/traefik.yml`) for plain HTTP:

```yaml
entryPoints:
  web:
    address: ":80"

providers:
  file:
    directory: /var/lib/russel-routes
    watch: true
```

Restart Traefik, then deploy or update any service. A file named after the service appears in the folder within a second, and Traefik starts routing.

## 3. Pick host names

With no `[ingress]` table, a service is reachable at `<service name>.<RUSSEL_TRAEFIK_DOMAIN>`. The domain defaults to `russel.local`, so the example `api` service is `api.russel.local`.

Set your own domain:

```bash
echo 'RUSSEL_TRAEFIK_DOMAIN=example.com' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

Or give one service an exact name in its Russelfile:

```toml
[ingress]
host = "api.example.com"
```

`host` is used exactly as written (lowercased), and can be a bare domain like `example.com`. Two services can't claim the same name. To go back to the default name, delete the `[ingress]` table and run `russel update <id> --refresh`.

Test it once DNS points at the server:

```bash
curl http://api.example.com/
```

## 4. Add HTTPS

Add a `websecure` entry point and a Let's Encrypt resolver to the static config:

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
    directory: /var/lib/russel-routes
    watch: true
```

Create `acme.json` with mode `0600`, owned by Traefik's user, and on storage that survives reboots:

```bash
sudo install -m 600 -o traefik -g traefik /dev/null /var/lib/traefik/acme.json
```

Then tell Russel to request certificates:

```bash
echo 'RUSSEL_TRAEFIK_TLS=1' | sudo tee -a /etc/russel/env
echo 'RUSSEL_TRAEFIK_CERT_RESOLVER=letsencrypt' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

Every route now listens on both `web` and `websecure`, and Traefik gets a certificate for each host name on first use. The name must be public (not `russel.local`), resolve to this server, and be reachable on port 80.

## Settings

All of these go in `/etc/russel/env`, followed by `sudo systemctl restart russel-ctrl`.

| Variable | Default | Meaning |
|---|---|---|
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `/var/lib/russel/traefik/dynamic` | Where route files are written. Must match Traefik's `providers.file.directory`. |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Suffix for services without `[ingress].host`. |
| `RUSSEL_TRAEFIK_TLS` | off | `1` adds HTTPS to every route. |
| `RUSSEL_TRAEFIK_CERT_RESOLVER` | `letsencrypt` | Must match the resolver name in Traefik's static config. |
| `RUSSEL_TRAEFIK_BACKEND` | `127.0.0.1` | The address Traefik uses to reach apps. Change it only when Traefik runs in its own network namespace, such as a rootless Podman container, where the host is `10.89.0.1`. |
| `RUSSEL_TRAEFIK_ENTRYPOINT` | `http://127.0.0.1:80`, best effort | Where Russel checks, during an update, that Traefik serves the new version. Set it when Traefik's `web` entry point is elsewhere (`http://127.0.0.1:8080`, or `https://…` for a TLS-only one); a set value makes the check strict. `off` skips it. |

## What Russel writes

For a service named `api` on port 3100, the file `api.yaml` looks like this. It is JSON, which Traefik reads as YAML; Traefik skips files with other extensions, such as the `.json` files earlier Russel releases wrote. Russel removes an old `.json` file the next time it writes the route.

```json
{
  "http": {
    "routers": {
      "russel-api": {
        "rule": "Host(`api.russel.local`)",
        "entryPoints": ["web"],
        "service": "russel-api",
        "middlewares": ["russel-api"]
      }
    },
    "middlewares": {
      "russel-api": {
        "headers": {
          "customResponseHeaders": { "X-Russel-Route": "cbfac658a2deb196" }
        }
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

Russel writes each file in one step, so Traefik never sees half a file. On an update the file switches to the new version's port once it answers; on `stop` or `destroy` the file is deleted. [Networking](../concepts/networking.md#during-an-update) explains the switch.

The `X-Russel-Route` header names the backend the route points at. During an update Russel sends requests for the host to `RUSSEL_TRAEFIK_ENTRYPOINT` until the new version's value comes back, and only then retires the old version. [What zero downtime covers](../concepts/lifecycle.md#what-zero-downtime-covers) explains what happens when Traefik doesn't switch, or isn't there.

## Related

- [Networking](../concepts/networking.md) · [TLS reverse proxy](./tls-reverse-proxy.md) · [Environment reference](../reference/environment.md)
