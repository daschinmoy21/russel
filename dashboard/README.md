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

## Settings

Configure API base URL, bearer token, and demo mode in the `/settings` page. Stored in `localStorage`.

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
