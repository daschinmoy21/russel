# Russel

Self-hosted deploys for Nix-built services on your own Linux box. Point Russel at a git repo with a `Russelfile.toml`; it builds the app with Nix and runs it as a **rootless Podman container**, or, experimentally, as a **Cloud Hypervisor microVM**. Redeploys switch traffic without downtime, and every generation can be rolled back.

> **Status: v0.1, first public release.** Built for one trusted operator on one host. Containers are the default and the tested path. MicroVMs accept the same Russelfile and are marked experimental. Not multi-tenant, no clustering yet.

## Why this repo's history looks the way it does

Development happens in a private repo (`russel-dev`), where each change lands as its own reviewed PR with CI: about 480 commits and 210 merged PRs since May 2026. This public repo gets that work as periodic `chore: sync …` commits, which is why a single commit here can add thousands of lines. Internal plans and audits stay private; code, docs, and open issues are mirrored. Issues and PRs are welcome here.

## Quick look

```toml
# Russelfile.toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "basic-http"
type = "container"   # or "microvm" (experimental, needs KVM)

[service.env]
API_TOKEN = "secret://API_TOKEN"   # stored on the control plane, never in the file
```

```bash
russel apply https://github.com/you/app.git   # build with Nix, start, switch traffic
russel ps                                     # services, ports, state
russel logs api
russel update api --refresh                   # redeploy the source's latest commit
russel rollback api                           # back to the previous generation
```

Services packaged in nixpkgs need no code at all: `package = "redis"` is a whole service.

## Features

- **Nix builds.** Uses your `flake.nix`, or generates one for Rust, Go, and static sites. Builds are pinned to a commit, and old generations are GC-rooted, so rollback always has something to roll back to.
- **Two runtimes, one Russelfile.** Rootless Podman on any VPS; Cloud Hypervisor microVMs where there's KVM. Both run unprivileged, and apps run as a non-root user by default.
- **Zero-downtime redeploys.** The new generation must answer before traffic moves; the old one is held briefly, then stopped. A deploy whose app crashes on start is reported as failed.
- **Ports, volumes, secrets, restart policy.** `[[ports]]`, `[[volumes]]` with kept data, `secret://` references delivered as Podman secrets, `restart = "unless-stopped"`.
- **Ingress.** Traefik file provider out of the box (`<name>.russel.local`), or put Caddy/nginx in front.
- **Dashboard.** Services, logs, deploys, and destroy from the browser, served by the control plane.
- **Installer.** `install.sh host` sets up the control plane as a dedicated `russel` system account with a token; `install.sh cli` puts `russel` on your laptop. NixOS users get a `services.russel` module.

## Install

On a Debian 12 / Ubuntu 22.04+ server with Nix (daemon install, flakes on) and rootless Podman:

```bash
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | bash -s -- check
curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | sudo RUSSEL_VERSION=v0.1.0 bash -s -- host
```

Then install the CLI where you work and log in. The [installation guide](docs/getting-started/installation.md) covers the laptop side, SSH tunnels vs HTTPS, and NixOS.

## Examples

[`examples/`](examples/) has 13 services. CI deploys each one as a container and checks it answers: Go and Rust HTTP apps, a static site, env/secrets, a URL shortener, and nixpkgs packages (Caddy, Meilisearch, Navidrome, PostgreSQL, Redis, Vaultwarden, Filebrowser). All 13 also pass as microVMs (run locally; that job needs KVM).

## Documentation

- [Quickstart](docs/quickstart.md) and [first deploy](docs/getting-started/first-deploy.md)
- [Russelfile reference](docs/reference/russelfile.md), [CLI](docs/reference/cli.md), [HTTP API](docs/reference/api.md)
- [Architecture](docs/concepts/architecture.md), [runtimes](docs/concepts/runtimes.md), [networking](docs/concepts/networking.md)
- [Security model](docs/security/overview.md) and [benchmarks](docs/guides/benchmarks.md)
- [v0.1 status](docs/project/features/v0.1.md)

## Repo layout

| Path | What |
|---|---|
| `crates/cli` | the `russel` CLI |
| `crates/ctrl` | `russel-ctrl`: control plane, builds, runtimes, networking, HTTP API |
| `crates/core` | Russelfile schema and API types shared by both |
| `crates/agent` | node agent (groundwork for multi-host) |
| `dashboard/` | Astro dashboard, served by the control plane |
| `nix/` | microVM kernel and the NixOS `services.russel` module |
| `contrib/` | installer, systemd unit, release and test scripts |

About 42k lines of Rust and 870+ tests. `nix develop` gives you the toolchain; see [development](docs/project/development.md).

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Commits need a DCO sign-off (`git commit -s`).

## License

Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). The microVM kernel shipped with releases is GPL-2.0, with its source attached to each release.
