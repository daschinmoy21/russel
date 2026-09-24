---
title: Dashboard
description: Use the Russel dashboard locally or behind HTTPS — API base, 401 vs offline, and tokens.
sidebar_position: 3
keywords: [dashboard, astro, vite, api base, 401, offline, sessionStorage]
---

# Dashboard

`russel-ctrl` serves the dashboard on the same listener as the API (`http://127.0.0.1:7878/` by default). Open that URL and keep Settings at `/api`. CLI login does not fill the dashboard token.

The UI is the Astro app in `dashboard/`. Ctrl looks for a built `dist/` next to the binary, in `/usr/local/share/russel/dashboard`, or in `dashboard/dist` of a checkout. `./contrib/install.sh ctrl` and `host` copy `dashboard/dist` into that share path. `--no-dashboard` skips it. `--dashboard-dir` / `RUSSEL_DASHBOARD_DIR` pin a dist.

## What you'll need

- A running `russel-ctrl` ([Installation](installation.md)) with a built dashboard dist.
- `bun` only if you are changing the dashboard source (`bun run dev` / `bun run build`).

## Run it

| Topology | Open | Dashboard Settings |
|---|---|---|
| A. Same host | `http://127.0.0.1:7878/` on the host | `/api` (ctrl nests the API under `/api`) |
| B. Split + SSH | `http://127.0.0.1:7878/` on the laptop with `install.sh connect` up | `/api` (same path through the tunnel) |
| C. Split + HTTPS | the HTTPS origin (proxy can forward `/` and `/api/` to loopback ctrl) | `/api` (same-origin HTTPS) |

```bash
russel-ctrl                 # serves API + dashboard; logs go to the log file
russel-ctrl --debug         # also stream logs to stderr
russel-ctrl --no-dashboard  # API only
```

Dashboard development still uses Vite:

```bash
cd dashboard
bun install
bun run dev     # http://127.0.0.1:4321, Vite proxies `/api` to loopback ctrl
bun run build   # refresh dist/ that ctrl and the installer copy
```

Presets in Settings are: same-origin `/api`, tunneled `/api`, and custom HTTPS. The sidebar never rewrites `/api` into `127.0.0.1:7878`. The in-app Docs link points at the public install doc.

## 401 vs offline

- **HTTP 401 = unauthorized**, not offline. The dashboard does not clear the token and tells you to re-enter it.
- **Offline = unreachable tunnel/proxy.** The copy names the tunnel or reverse proxy and suggests checking `install.sh status` / the proxy.

The control plane has **no CORS**. Always go through same-origin `/api` or the Vite proxy, never cross-origin directly.

## Tokens

- Dashboard token is entered at runtime in Settings and kept in tab-scoped `sessionStorage`.
- CLI login (`~/.config/russel/config.toml`) does **not** populate the dashboard.
- Never bake an API token into the frontend env or ship it to browsers.

## Verify

```bash
cd dashboard && bun test     # 60+ tests (api_base, deploy stream, connection state)
cd dashboard && bun run build  # astro check + build, 0 diagnostics
```

CI runs `bun test` before the dashboard build.

## Related

- [Installation](installation.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md) · [Troubleshooting](../guides/troubleshooting.md)
