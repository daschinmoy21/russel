---
title: Benchmarks
description: How long a Russel deploy takes compared with building and running the same app with plain Podman, and how to measure it yourself.
sidebar_position: 7
keywords: [benchmark, performance, boot time, deploy time, bench.sh]
---

These numbers come from one run on 2026-07-24, on a NixOS workstation with Podman 5.8.2 and warm caches. They predate v0.1.0, which moved the control plane to an unprivileged account. Treat them as a rough guide.

Each example app was deployed three ways:

- **Russel container:** `russel deploy`, rootless container.
- **Russel microVM:** `russel deploy` with `type = "microvm"`.
- **Plain Podman:** `podman build` from the example's Dockerfile, then `podman run`.

## Deploy to first response

Time from starting the deploy until the app answered an HTTP request, including the build. Russel's Nix builds were cached; Podman rebuilt its image.

| App | Russel container | Russel microVM | Plain Podman |
|---|---|---|---|
| basic-http | **1.37 s** | 3.24 s | 7.81 s |
| hello-rust | **1.14 s** | 1.53 s | 9.17 s |
| env-config | **0.95 s** | 1.26 s | 10.23 s |
| shortlink | **1.19 s** | 1.53 s | 10.09 s |
| static-test | **1.27 s** | 1.99 s | 2.60 s |
| filebrowser | **1.74 s** | 2.68 s | 9.04 s |

Most of Podman's time is the image build. With both sides cached, the gap comes down to the start-up numbers below.

## Start-up only

Time from "the build exists" to "the app answers", without the build:

| App | Russel container | Russel microVM | Plain Podman |
|---|---|---|---|
| basic-http | 669 ms | 1892 ms | **528 ms** |
| hello-rust | 659 ms | 880 ms | **562 ms** |
| env-config | 639 ms | 851 ms | **443 ms** |
| shortlink | 847 ms | 1046 ms | **509 ms** |
| static-test | **795 ms** | 1371 ms | 1279 ms |
| filebrowser | 840 ms | 1261 ms | **766 ms** |

Plain Podman usually starts fastest. A Russel container adds 100 to 300 ms for preparing its root folder and checking that the app really answers. A microVM adds a kernel boot on top. Containers were capped at 256 MB of memory to match the microVM default.

## Where a container deploy spends its time

A typical cached deploy of `basic-http`:

| Stage | Time |
|---|---|
| Fetch repo, load Russelfile | 0 ms |
| `nix build` (cached) | 897 ms |
| Start the container | 643 ms |
| Wait for the app to answer | 0 ms |
| **Total** | **1540 ms** |

## Run it yourself

The benchmark scripts are in the repo root and need a checkout, Nix, rootless Podman, and Podman or Docker for the Dockerfile baseline. The microVM runs also need KVM and root, and are skipped without Russel's kernel.

```bash
sudo -E nix develop -c ./bench.sh --warm
```

`--cold` clears the caches first. The scripts take over `/var/lib/russel` while they run, so never run them on a server with real services.

## Related

- [Runtimes](../concepts/runtimes.md) · [Builds](../concepts/builds.md)
