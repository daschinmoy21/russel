---
title: Builds
description: How Russel turns source into a Nix closure — flakes, auto-generation, and trust.
sidebar_position: 5
keywords: [nix, flake, auto-generation, build, trusted source, sandbox]
---

# Builds

Nix is the only build system. The deployable artifact is always a `/nix/store` path; both runtimes consume it directly (no image builds, no registry).

## What you'll need

- Nix with flakes enabled on the control-plane host.
- A repo with `packages.<system>.default` (or nothing — Russel can generate it).

## Pipeline

1. Resolve the repo (clone with leases + GC, or use a trusted local path).
2. Load `Russelfile.toml` (`openat` + `O_NOFOLLOW`, 1 MiB cap).
3. Ensure a `flake.nix` exists (use the repo's, or auto-generate).
4. `nix build path:checkout#packages.<system>.default` → `/nix/store/<hash>`.
5. Hand the closure to the runtime (virtiofs share for microVMs, `--rootfs` + `/nix/store:ro` bind for containers).

There is no `russel build` command. Russel builds automatically during `deploy`. Use `nix build` directly only to inspect an artifact without deploying:

```bash
nix build path:examples/basic-http
```

## Auto-generation

If no `flake.nix` is present, Russel writes one from project type:

| Detected file | Project kind | Generated flake |
|---|---|---|
| `Cargo.toml` | Rust | `crane`/`rustPlatform` build of `bin` |
| `go.mod` | Go | `buildGoModule` (`vendorHash = null` for vendored/stdlib-only; `lib.fakeHash` hint otherwise) |
| else | Static | Minimal static server |

`russel init --with-flake` writes the same starter as a **committed** file you own and edit. Control-plane auto-generation in restricted mode carries a "do not edit" marker instead.

## Trust model

**Nix builds trust the source repo** — a malicious `flake.nix` runs as the build user. On multi-tenant or untrusted-source hosts, only deploy trusted repos and harden the host:

```bash
export RUSSEL_NIX_RESTRICTED=1   # sandboxed nix build + no auto-flake
```

Full threat model, `nix.conf` hardening, and sandbox options: [Security: Nix builds](../security/nix-builds.md).

## Garbage collection

Builds run with `--no-link`, so ctrl keeps its own GC roots. Each service has
`/var/lib/russel/_pool/gcroots/<id>/` with one indirect root per store path it
may re-exec: the current `metadata.json` paths (app, microVM kernel and
initramfs, container rootfs) and the deployment journal's `active` and
`previous` generations. Ctrl syncs the directory after every deploy and
automatic rollback, and for every service at startup. Older generations lose
their roots, and destroy removes the directory. `nix-collect-garbage` (or
NixOS `nix.gc.automatic`) therefore never deletes what a restart, reboot
recovery, or rollback to the previous generation needs. Rolling back further
than `previous` rebuilds from the recorded source.

## Kernel note (microVMs)

MicroVMs must use the Russel-compiled kernel (flake package `.#microvm-kernel`, virtio drivers built-in). Bench and production deploys export `RUSSEL_KERNEL_PATH` to that `bzImage` so ctrl never falls back to a stock nixpkgs kernel (slower / wrong module set):

```bash
nix build .#microvm-kernel -o result-kernel
export RUSSEL_KERNEL_PATH="$(readlink -f result-kernel/bzImage)"
```

## Related

- [Architecture](architecture.md) · [Runtimes](runtimes.md) · [Nix builds](../security/nix-builds.md) · [App packaging](../guides/vps-one-dev.md#d-first-deploy)
