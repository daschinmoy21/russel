# Russel Control Plane Web Dashboard

Web dashboard for the **Russel** self-hosted deploy platform (microVMs + containers). Built with **Astro**, **TypeScript**, and **Bun**, styled in a dark Zed-like aesthetic.

## Quick Start

```bash
cd dashboard
nix develop          # enter bun/dev shell
bun install
bun run dev          # http://127.0.0.1:4321 (loopback only)
bun run build        # production static build → dist/
```

## Network bind (security)

**Default is loopback only** (`127.0.0.1:4321`). `bun run dev`, `start`, and `preview` do **not** expose the dashboard on the LAN.

The Vite dev proxy forwards `/api/*` to the control plane on loopback (`http://127.0.0.1:7878`). If the dashboard listens on `0.0.0.0`, any host on your network can reach that proxy and obtain full control-plane access. **This proxy is not a production front door.**

| Script | Bind | When to use |
|--------|------|-------------|
| `bun run dev` / `start` / `preview` | `127.0.0.1` | Default / safe |
| `bun run dev:lan` / `preview:lan` | `0.0.0.0` (all interfaces) | Only when you intentionally need LAN access |

**Never use `--host` / `*:lan` scripts on a shared network without `RUSSEL_API_TOKEN` set on the control plane.** Prefer SSH port-forwarding or VPN over LAN bind.

## Dev Proxy

The dev server proxies `/api/*` → `http://127.0.0.1:7878/*` (override via `RUSSEL_API_PROXY`). This avoids CORS issues when the browser talks to the control plane. It is a **local development convenience only** — not an authenticated production gateway.

## Demo Mode

Enable in **Settings** → Demo Mode toggle, or set `localStorage.RUSSEL_DEMO_MODE=1`. All API calls return mock data — no control plane needed.

## Settings & API token (security)

Configure API base URL, bearer token, and demo mode on the `/settings` page.

| Key | Storage | Purpose |
|-----|---------|---------|
| `RUSSEL_API_URL` | `localStorage` | API base URL (default `/api`) |
| `RUSSEL_API_TOKEN` | **`sessionStorage`** | Bearer token (tab-scoped; cleared when the tab closes) |
| `RUSSEL_DEMO_MODE` | `localStorage` | `"1"` = mock data, no control plane |

### Never bake the token into the client bundle

**Do not set `PUBLIC_RUSSEL_API_TOKEN`.** Astro/`PUBLIC_*` vars are inlined into the built JS and would ship the secret to every browser that loads the dashboard. The client **ignores** that env var (and logs a one-time console warning if it is present). Enter the token only via **Settings** at runtime.

Also avoid any other `PUBLIC_*` secret: only non-sensitive config (e.g. a public API base URL) belongs there.

### XSS / token exfiltration note

The bearer token is readable by any script that runs in the dashboard origin. Prefer:

- Loopback bind (default) or an authenticated reverse proxy
- Short-lived tab sessions (`sessionStorage`)
- Escaping user/control-plane strings when building HTML (service IDs, statuses, etc.)

If you serve the static `dist/` behind a reverse proxy, consider a Content-Security-Policy that restricts `script-src` (note: Astro may use inline scripts for FOUC/theme — adjust CSP carefully, e.g. hashes or nonces).

## Documentation

See [`docs/`](docs/) for:

- [Architecture](docs/architecture.md)
- [API Mapping](docs/api-mapping.md)
- [Realtime Roadmap](docs/realtime-roadmap.md)

## Project Structure

```
dashboard/
├── flake.nix
├── package.json
├── astro.config.mjs
├── tsconfig.json
├── public/               # favicon
├── docs/                 # architecture, API mapping, roadmap
└── src/
    ├── components/       # Header, Sidebar, MetricCard, ServicesTable, ActivityChart
    ├── layouts/          # Layout.astro
    ├── lib/              # api.ts — RusselClient + types
    ├── pages/            # index, services, service-detail, deploy, settings
    └── styles/           # global.css
```
