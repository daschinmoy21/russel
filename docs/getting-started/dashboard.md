---
title: Dashboard
description: Open the Russel dashboard, sign in with your API token, and tell an expired token from a lost connection.
sidebar_position: 3
keywords: [dashboard, web ui, token, settings, 401, offline]
---

The control plane serves a web dashboard on the same address as the API. It shows your services, their status and logs, and lets you deploy.

## What you'll need

- A working install ([Installation](./installation.md)). The installer puts the dashboard in `/usr/local/share/russel/dashboard`.
- Your API token, from `/etc/russel/env` on the server.

## Open it

| Where you are | Open |
|---|---|
| On the server | `http://127.0.0.1:7878/` |
| On your laptop, over SSH | `http://127.0.0.1:7878/`, with the `install.sh connect` tunnel running |
| Behind an HTTPS proxy | Your HTTPS address, such as `https://russel.example.com/` |

Then go to **Settings** and paste the token (the part after `RUSSEL_API_TOKEN=`). Leave the API address at `/api` in all three cases.

The dashboard keeps the token for that browser tab only, and forgets it when you close the tab. `russel login` doesn't sign in the dashboard; each has its own copy of the token.

## "Unauthorized" or "Offline"?

The dashboard tells these apart:

- **Unauthorized (HTTP 401):** the control plane is reachable but rejected the token. Paste the token again. The dashboard keeps what you entered so you can fix it.
- **Offline:** nothing answered. The SSH tunnel is down, the proxy is down, or `russel-ctrl` isn't running. Check `systemctl status russel-ctrl` on the server and restart the tunnel.

## Behind a proxy

The dashboard calls the API on its own origin, under `/api`. The control plane doesn't send CORS headers, so the dashboard can't be hosted on one address and call the API on another. Proxy the whole origin to `127.0.0.1:7878`; [TLS reverse proxy](../guides/tls-reverse-proxy.md) has working configs.

## Without the dashboard

`russel-ctrl --no-dashboard` serves the API only. `--dashboard-dir PATH` (or `RUSSEL_DASHBOARD_DIR`) loads it from another folder. To work on the dashboard itself, see [Contributing](../project/development.md#dashboard).

## Related

- [Installation](./installation.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md) · [Troubleshooting](../guides/troubleshooting.md)
