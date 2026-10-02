---
title: Builds
description: How Russel builds your repo with Nix, the flakes it writes when you have none, and how builds are kept safe from garbage collection.
sidebar_position: 5
keywords: [nix, flake, auto-generation, build, garbage collection]
---

Russel builds everything with Nix. The result is a path in `/nix/store`, and both runtimes run the app from there.

## What you'll need

- Nix with flakes enabled on the server (see [Installation](../getting-started/installation.md)).
- A repo whose flake has `packages.<system>.default`, or no flake at all for Rust, Go, and static sites.

## What happens during a build

1. The server clones the repo and loads the Russelfile.
2. It uses the repo's `flake.nix`, or writes one if there is none.
3. It runs `nix build` on `packages.<system>.default` in the folder the Russelfile points at.
4. It hands the resulting `/nix/store` path to the runtime.

There is no separate build command: builds happen inside `russel deploy` and `russel update`. To try a build on its own, run Nix yourself:

```bash
nix build .#default
```

## Projects without a flake

If there is no `flake.nix`, Russel writes one based on what it finds:

| Found | Project | How it's built |
|---|---|---|
| `Cargo.toml` | Rust | Builds the binary named by `service.bin` |
| `go.mod` | Go | `buildGoModule`. Works as-is for vendored or standard-library-only modules; otherwise the build fails and prints the `vendorHash` to add. |
| Neither | Static site | Serves the folder with a small static file server |

To own and edit the flake yourself, run `russel init --with-flake` and commit the file it writes.

For apps already packaged in nixpkgs, skip the flake entirely: set `service.package = "navidrome"` (or any nixpkgs attribute) in the Russelfile.

## Trust

A Nix build runs code from the repo as the `russel` account. Only deploy repos you trust. If you must build repos you don't fully trust, turn on restricted mode:

```bash
echo 'RUSSEL_NIX_RESTRICTED=1' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

That forces Nix's sandbox and turns off flake generation. [Nix build security](../security/nix-builds.md) covers the rest.

## Garbage collection

Russel registers a Nix garbage-collection root for everything a service might need to start again: the running version, the previous version, the version the last rollback moved away from, and (for microVMs) the kernel and boot image. So `nix-collect-garbage`, or NixOS's `nix.gc.automatic`, never deletes what a restart, a reboot, or a rollback to the previous version needs. Older versions lose their roots. Rolling back to one that Nix has since collected fails with a hint to use `russel rollback --rebuild`, which builds it from source. `russel destroy` removes the service's roots.

The roots live in `/var/lib/russel/_pool/gcroots/<id>/`.

## MicroVM kernel

MicroVMs need Russel's own kernel, which has the virtio drivers built in. The installer downloads it to `/var/lib/russel/_pool/kernel/bzImage` on hosts with `/dev/kvm`. To build it yourself from a checkout:

```bash
nix build .#microvm-kernel
```

Then set `RUSSEL_KERNEL_PATH` to the output of `readlink -f result/bzImage` in `/etc/russel/env` and restart the service. Russel refuses to boot with a stock nixpkgs kernel.

## Related

- [Architecture](./architecture.md) · [Runtimes](./runtimes.md) · [Nix build security](../security/nix-builds.md) · [Russelfile](../reference/russelfile.md)
