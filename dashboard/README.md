# Russel Control Plane Web Dashboard

Web dashboard for the **Russel** self-hosted deploy platform (microVMs + containers). Built with **Astro**, **TypeScript**, and **Bun**, styled in a dark Zed-like aesthetic.

## Quick Start

```bash
cd dashboard
nix develop          # enter bun/dev shell
bun install
bun run dev          # http://localhost:4321
bun run build        # production static build → dist/
```

## Dev Proxy

The dev server proxies `/api/*` → `http://127.0.0.1:7878/*` (override via `RUSSEL_API_PROXY`). This avoids CORS issues when the browser talks to the control plane.

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
