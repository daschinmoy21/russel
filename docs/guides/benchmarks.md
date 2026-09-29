---
title: Benchmarks
description: What Russel costs vs raw Podman — E2E and spawn-to-ready numbers and how to reproduce.
sidebar_position: 7
keywords: [benchmark, performance, boot time, spawn-to-ready, bench.sh]
---

Warm-run snapshot (2026-07-24, NixOS, podman 5.8.2, rootless containers via `SUDO_USER`, microVM via Cloud Hypervisor). Total wall ~271 s (clean debug + release + tests + 7 app races).

**MicroVMs must use the Russel-compiled kernel** (flake `.#microvm-kernel`, virtio built-in). `bench.sh` builds it and exports `RUSSEL_KERNEL_PATH` so ctrl never falls back to stock nixpkgs. Numbers below assume that kernel.

## Systems

| Metric | Value |
|---|---|
| Clean build (debug) | 21.7 s |
| Release build | 70.2 s |
| Tests | ~301 unit tests, 45.9 s (workspace; now 500+ with audit batches) |
| Binary size (cli) | 7.1 MB (~5.5 MB stripped) |
| Binary size (ctrl) | 5.9 MB (~4.5 MB stripped) |
| Rust LOC | ~17 k (22 files at snapshot; higher after `init` + agent) |
| Clippy warnings | 0 (`--workspace --all-targets -- -D warnings`) |

## App boot race — end-to-end (build/deploy + first HTTP)

Three paths per example: **Russel microVM**, **Russel container** (rootless `--rootfs`), **raw podman** (Dockerfile build + run). Winner = lowest E2E.

| App | Russel microVM | Russel container | Podman baseline | Winner |
|---|---|---|---|---|
| basic-http | 3.24 s (3.21+0.04) | **1.37 s** (1.35+0.02) | 7.81 s (7.28+0.53) | Russel-ctr |
| microvm-http | 1.27 s (1.26+0.01) | **1.09 s** (1.07+0.02) | 9.24 s (8.88+0.37) | Russel-ctr |
| hello-rust | 1.53 s (1.48+0.05) | **1.14 s** (1.11+0.03) | 9.17 s (8.61+0.56) | Russel-ctr |
| env-config | 1.26 s (1.24+0.02) | **0.95 s** (0.94+0.01) | 10.23 s (9.79+0.44) | Russel-ctr |
| shortlink | 1.53 s (1.50+0.03) | **1.19 s** (1.16+0.02) | 10.09 s (9.58+0.51) | Russel-ctr |
| static-test | 1.99 s (1.96+0.03) | **1.27 s** (1.10+0.17) | 2.60 s (1.32+1.28) | Russel-ctr |
| filebrowser | 2.68 s (2.64+0.04) | **1.74 s** (1.60+0.14) | 9.04 s (8.28+0.77) | Russel-ctr |

`(deploy+curl)` for Russel; `(image build + run→HTTP)` for the baseline.

## Spawn-to-ready (excluding build)

Best apples-to-apples "how fast after the package exists":

| App | Russel microVM | Russel container | Podman baseline |
|---|---|---|---|
| basic-http | 1892 ms | 669 ms | **528 ms** |
| microvm-http | 919 ms | 655 ms | **366 ms** |
| hello-rust | 880 ms | 659 ms | **562 ms** |
| env-config | 851 ms | 639 ms | **443 ms** |
| shortlink | 1046 ms | 847 ms | **509 ms** |
| static-test | 1371 ms | **795 ms** | 1279 ms |
| filebrowser | 1261 ms | 840 ms | **766 ms** |

**Reading the tables:** E2E favors Russel on warm runs (Nix store cache vs image build). Spawn-to-ready is usually raw podman ≤ Russel container < microVM — microVM pays for Cloud Hypervisor + virtiofs + guest init for stronger isolation. Container memory capped at 256 m to match microVM defaults.

## Last container deploy phases (example)

| Phase | Time | Detail |
|---|---|---|
| resolve | 0 ms | repo + Russelfile |
| build | 897 ms | nix build (package) |
| create | 0 ms | rootfs adapter |
| network | 0 ms | n/a for container |
| start | 643 ms | rootless podman run --rootfs |
| ready | 0 ms | host TCP/HTTP ready |
| **total** | **1540 ms** | sum of phases |

## Reproduce

```bash
# Root for TAP/KVM + SUDO_USER rootless podman; builds .#microvm-kernel automatically:
sudo -E nix develop -c ./bench.sh --warm

# Or pin a prebuilt kernel explicitly:
nix build .#microvm-kernel -o result-kernel
export RUSSEL_KERNEL_PATH="$(readlink -f result-kernel/bzImage)"
sudo -E env RUSSEL_KERNEL_PATH="$RUSSEL_KERNEL_PATH" nix develop -c ./bench.sh --warm
```

Needs Rust toolchain, Nix, KVM + root for microVMs, the compiled `.#microvm-kernel` (or valid `RUSSEL_KERNEL_PATH`), rootless podman for Russel containers, and podman/docker for the Dockerfile baseline. `--cold` forces cold Nix + image rebuilds. Without the custom kernel, microVM races are skipped. Scripts share `bench-common.sh`, hold a `flock` run-lock on `/var/lib/russel`, and clean up the demo secret.

## Related

- [Runtimes](../concepts/runtimes.md) · [Builds](../concepts/builds.md)
