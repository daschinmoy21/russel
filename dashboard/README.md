# Russel control plane web dashboard

Web dashboard for the Russel self-hosted deploy platform. It is built with Astro, TypeScript, and Bun.

## Quick start

```bash
cd dashboard
nix develop
bun install
bun run dev
bun run build
```

The development server is available at `http://127.0.0.1:4321` and binds to loopback by default. Operators do not need this: `russel-ctrl` serves `dist/` on the control-plane listener (`http://127.0.0.1:7878/`). Use Vite when editing the dashboard.

## Network bind and proxy

`bun run dev`, `bun run start`, and `bun run preview` bind to `127.0.0.1`. The optional `dev:lan` and `preview:lan` scripts bind all interfaces and should only be used intentionally on a trusted network.

The dashboard API base is `/api` by default. Vite proxies `/api/*` to the local forwarded control-plane port at `127.0.0.1:7878` during development. The browser request remains same-origin, so the dashboard does not need CORS.

Do not fetch `http://127.0.0.1:7878` from the dashboard origin. In particular, tunneled `bun run dev` uses `/api` in Settings, not an absolute `:7878` URL. Do not add CORS to russel-ctrl or bind its HTTP API publicly.

## Development topology

For an SSH-tunneled VPS, run the tunnel and dashboard in separate terminals:

```bash
./contrib/install.sh connect <user@host>
cd dashboard
bun run dev
```

Keep the Settings API base at `/api`. Vite sends that path to the SSH-forwarded local port.

For production, serve the dashboard over HTTPS and configure the same-origin reverse proxy to map `/api/` to the loopback russel-ctrl API. The production dashboard also keeps `/api` as its API base. It does not contact `:7878` directly and does not require CORS.

## Demo mode

Enable Demo Mode in Settings, or set `localStorage.RUSSEL_DEMO_MODE=1` in the browser. Demo mode uses mock data and does not contact russel-ctrl.

## Settings and API token security

| Key | Storage | Purpose |
|-----|---------|---------|
| `RUSSEL_API_URL` | `localStorage` | API base URL, default `/api` |
| `RUSSEL_API_TOKEN` | `sessionStorage` | Bearer token for the current browser tab |
| `RUSSEL_DEMO_MODE` | `localStorage` | `1` enables mock data |

The token is runtime-only and tab-scoped. The CLI login and CLI `config.toml` do not populate the dashboard. Paste the same token used in `~/.config/russel/env` into Settings. The dashboard never reads `PUBLIC_RUSSEL_API_TOKEN`.

### Never bake the token into the client bundle

Do not set `PUBLIC_RUSSEL_API_TOKEN`. Astro and Vite inline `PUBLIC_*` values into browser JavaScript. Enter the token in Settings at runtime instead.

The bearer token is readable by scripts running on the dashboard origin. Prefer loopback binding or an authenticated HTTPS reverse proxy, and consider a Content Security Policy for a deployed static `dist/` directory.

## Documentation

Operator docs: [Installation](../docs/getting-started/installation.md) and [Dashboard](../docs/getting-started/dashboard.md).

## Project structure

```text
dashboard/
├── flake.nix
├── package.json
├── astro.config.mjs
├── tsconfig.json
├── public/               # favicon
└── src/
    ├── components/       # Header, Sidebar, MetricCard, ServicesTable, ActivityChart
    ├── layouts/          # Layout.astro
    ├── lib/              # api.ts and connection helpers
    ├── pages/            # index, services, service-detail, deploy, settings
    └── styles/           # global.css
```
