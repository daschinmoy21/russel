---
title: Traefik ingress
description: Give each app its own host name, with HTTPS from Let's Encrypt, using Traefik and the route files Russel writes.
sidebar_position: 3
keywords: [traefik, ingress, host routing, tls, acme, lets encrypt, dns]
---

Russel writes a Traefik route file for every service it deploys. Point Traefik's file provider at that folder and each app is reachable by name, such as `api.example.com`, with no reloads or API calls.

## Who owns Traefik?

Traefik is an external service that you install, configure, and run. Russel does not start a private Traefik instance or take ownership of ports 80 and 443. Russel publishes each app on a host port and writes that app's dynamic route file. Traefik reads the files, forwards requests, and handles certificates when configured for HTTPS. You manage DNS, Traefik's static configuration, and the public listeners.

The setup below uses one shared Traefik instance on the VPS. It can serve both Russel apps and applications you run outside Russel. Keep the other applications' routes in configuration you own; Russel manages only its service route files. Its router, service, and middleware names start with `russel-`.

## Choose how traffic reaches Russel apps

| Setup | Who handles public traffic? | What to configure |
|---|---|---|
| Shared Traefik | One Traefik on the VPS serves Russel and other apps. | Make its file provider read Russel's route directory, using the setup below. |
| Dedicated Traefik behind an existing proxy | Your existing Caddy, Nginx, or Traefik keeps ports 80 and 443; a separate Traefik handles Russel routes on an internal port. | Forward the Russel hostnames to the internal Traefik, preserving `Host`. Keep TLS and the public listeners on the outer proxy. |
| Manual routing to app ports | Your existing proxy routes directly to each Russel app. | Pin each app's host port and maintain its proxy configuration yourself. Updates have a short gap. See [Manual routing](#manual-routing). |

Traefik is currently the only implemented automatic ingress adapter. Caddy and Nginx can route directly to published app ports, but Russel does not generate or update their configuration.

### Using an existing shared Traefik

Merge the settings below into your existing static configuration. Preserve its other entry points, providers, routes, and certificate resolvers. Russel's generated routes expect entry points named `web` and, when `RUSSEL_TRAEFIK_TLS=1`, `websecure`. The certificate resolver name must match `RUSSEL_TRAEFIK_CERT_RESOLVER`.

If Traefik already has a [file-provider directory](https://doc.traefik.io/traefik/providers/file/), preserve the existing files when adding Russel's routes. Set `RUSSEL_TRAEFIK_DYNAMIC_DIR` to a writable directory that the file provider reads. Do not replace the existing watched directory with an empty Russel directory and drop your other routes. Reserve Russel's service filenames for Russel; do not edit its generated files by hand.

Russel checks hostname uniqueness among its service route files in its configured directory. It cannot detect every conflicting router from another directory or provider. Avoid claiming the same hostname in both Russel and your other proxy configuration, and reserve the `russel-` names for Russel.

### Dedicated Traefik behind another proxy

Run the dedicated Traefik with its `web` entry point on an unused internal address, such as `127.0.0.1:8080`, and its file provider watching Russel's route directory. Keep `RUSSEL_TRAEFIK_TLS` off if the outer proxy terminates HTTPS. Configure the outer proxy to forward the Russel hostnames to that address, preserving the original `Host` header.

If Traefik runs in a separate container network namespace, configure an address it can reach for `RUSSEL_TRAEFIK_BACKEND`; its own loopback is not the VPS host's loopback.

### Manual routing

For an existing proxy that you want to configure yourself, give the app a fixed host port:

```toml
[service]
name = "api"
source = "."
port = 3000

[ingress]
port = 4100
```

The app listens on port 3000 inside its sandbox; your proxy forwards to `http://127.0.0.1:4100` on the VPS. Set the hostname, TLS, and other routing options in that proxy's configuration. A proxy in another network namespace needs a reachable host address instead of its own loopback.

The pinned port holds across updates, rollbacks, and restarts. Russel stops the old generation before starting the new one because they cannot share port 4100, so updates have a short outage. An unpinned port can change during an update; a manually configured proxy would keep pointing at the retired port unless you update its route too. A manually configured Traefik route has the same limitation.

There is currently no switch that disables Russel's ingress adapter entirely. Even without an `[ingress].host`, Russel writes a default Traefik route. If no Traefik reads those files, they do not configure your existing proxy, but route-directory access and route validation still run during deploy. Keep those files outside any proxy configuration you intend to manage manually. A live Traefik watching the directory will still consume the generated routes; simply leaving out `[ingress]` does not opt out.

## Known limitations and related work

- Russel's generated routes expect Traefik entry points named `web` and, for HTTPS, `websecure`. Existing installations with other names need compatible static configuration.
- Manual proxy configuration needs a pinned backend port. Turning off a future adapter alone would not make an unpinned port stable or provide automatic zero-downtime routing.
- If your Russel release writes route files with a `.json` extension, Traefik's file provider does not load them. This was fixed in [PR #569](https://github.com/daschinmoy21/russel-dev/pull/569), which changes these files to `.yaml` and adds proxy-confirmed cutover and drain handling.
- `RUSSEL_TRAEFIK_ENTRYPOINT` selects the address for cutover confirmation. A dedicated Traefik on an internal port needs this variable set to its internal address, for example `http://127.0.0.1:8080`. `off` disables only the probe, not route-file creation or hostname checks. Confirmation at an internal Traefik does not validate DNS or the outer proxy.

An explicit optional ingress adapter and its stable-port/migration behavior are being assessed in [issue #570](https://github.com/daschinmoy21/russel-dev/issues/570). There is no supported ingress-off mode or plugin system yet.

## What you'll need

- An externally managed Traefik v2 or v3 instance on the server, either shared or dedicated.
- A DNS record for each app name, pointing at the server. Russel doesn't manage DNS.
- Ports 80 and 443 open on the public proxy. The direct setup below gives those listeners to Traefik.

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

Restart Traefik, then deploy or update any service. A file named after the service appears in the folder and Traefik picks it up through the file provider.

## 3. Pick host names

With Traefik configured to load Russel's routes and no `[ingress]` table, a service is reachable at `<service name>.<RUSSEL_TRAEFIK_DOMAIN>`. The domain defaults to `russel.local`, so the example `api` service is `api.russel.local`.

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
| `RUSSEL_TRAEFIK_ENTRYPOINT` | `http://127.0.0.1:80`, best effort | Where Russel checks, during an update, that Traefik serves the new version. Set it when Traefik's `web` entry point is elsewhere (`http://127.0.0.1:8080`, or `https://…` for a TLS-only one); a set value makes the check strict. `off` skips only the probe; route writing and hostname checks remain enabled. |

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
