---
title: Russel documentation
description: Self-hosted platform that builds your services with Nix and runs them as rootless containers, with experimental microVMs.
sidebar_position: 1
sidebarTitle: Introduction
keywords: [russel, nix, podman, rootless, containers, self-hosted, deployment, microvm]
---

Russel deploys your services to your own Linux server. You add a `Russelfile.toml` to a git repo, and Russel builds it with Nix and runs the result as a rootless Podman container. There are no images to build and no registry to push to: the container runs straight from the Nix store. MicroVMs (Cloud Hypervisor + KVM) are available as an experimental second runtime.

```bash
russel init       # write a Russelfile.toml for this project
russel deploy https://github.com/you/app.git   # build it with Nix and run it
russel ps         # see it running, and on which port
```

## When to use Russel

| Use Russel when | Look elsewhere when |
|---|---|
| You run your own services on one Linux server | You need several servers, autoscaling, or isolation between teams |
| You like Nix and want a deploy to take a second or two | You need managed databases (Russel has none) |
| You have a cheap VPS without KVM | You need hosted CI/CD (Russel builds on your server) |
| You want each app on its own host name without writing Dockerfiles | |

## How a deploy works

1. **Fetch.** The server clones your repo and reads `Russelfile.toml`.
2. **Build.** It runs `nix build` on the repo's flake. If there is no `flake.nix`, Russel writes one for Rust, Go, and static sites.
3. **Run.** It starts the build output as a rootless container, with `/nix/store` mounted read-only, and publishes the app on a `127.0.0.1` port.
4. **Route.** If you run Traefik, Russel writes a route so `app.example.com` reaches the app.

Updates start the new version next to the old one and switch traffic once it answers. Russel keeps the last 20 deployments, so you can roll back.

More: [Architecture](./concepts/architecture.md) and [Runtimes](./concepts/runtimes.md).

## Choose your path

| I want to… | Start here |
|---|---|
| Install Russel on a server | [Installation](./getting-started/installation.md) |
| Deploy an example app | [Quickstart](./quickstart.md) |
| Run one VPS as its only operator | [Single-VPS checklist](./guides/vps-one-dev.md) |
| Put apps or the API behind HTTPS | [Traefik ingress](./guides/traefik-ingress.md) · [TLS reverse proxy](./guides/tls-reverse-proxy.md) |
| Write a `Russelfile.toml` | [Russelfile reference](./reference/russelfile.md) |
| Call the API from scripts | [API reference](./reference/api.md) |
| Harden a server | [Security overview](./security/overview.md) |
| Upgrade or back up | [Upgrades and backups](./operations/upgrades-backup.md) |

## The two programs

| Program | Runs on | What it does |
|---|---|---|
| `russel-ctrl` | Your Linux server (the control plane) | Builds and runs apps. Serves the API and the dashboard on `127.0.0.1:7878`. |
| `russel` | Your laptop, or the server itself | The CLI: `deploy`, `update`, `ps`, `logs`, `rollback`, … |

The control plane only speaks plain HTTP on loopback. Reach it from your laptop through an SSH tunnel or an HTTPS reverse proxy; [Installation](./getting-started/installation.md) covers both.
