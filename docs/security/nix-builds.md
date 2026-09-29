---
title: Nix build security
description: What a malicious repo can do during a build, and how to limit it with restricted mode and nix.conf settings.
sidebar_position: 2
keywords: [nix, sandbox, restricted, threat model, flake, build security]
---

A Nix build evaluates the repo's `flake.nix`, and that code runs on your server as the `russel` account. Deploying someone's repo is like running their script. Russel's default assumes you deploy your own, trusted repos.

## What a malicious repo could do

Without the settings below, a build could:

- read files the `russel` account can read, through impure evaluation;
- send data out over the network during the build;
- put misleading paths into the Nix store.

Escaping an app's sandbox at runtime is a separate question; see [Runtimes](../concepts/runtimes.md). API access is covered in the [Security overview](./overview.md).

## Restricted mode

If you build repos you don't fully trust, turn on restricted mode:

```bash
echo 'RUSSEL_NIX_RESTRICTED=1' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

Restricted mode:

- always builds inside Nix's sandbox, with no access to the host beyond it;
- stops Russel from writing a flake for repos without one, so every build uses a `flake.nix` that was committed and can be reviewed.

## `nix.conf` settings

These tighten the Nix daemon for every user. Add them to `/etc/nix/nix.conf` and restart `nix-daemon`:

```ini
sandbox = true
sandbox-fallback = false
require-sigs = true
allow-import-from-derivation = false
trusted-users = root
```

- `sandbox = true` with `sandbox-fallback = false` makes a build fail when the sandbox can't be set up.
- `require-sigs = true` only accepts signed binaries from caches.
- `allow-import-from-derivation = false` stops evaluation from running a build to decide what to build.
- Keep `russel` out of `trusted-users`. Trusted users can change daemon settings and bypass these rules.

## Also

- Commit `flake.lock` so inputs are pinned.
- Review a generated flake before relying on it: `russel init --with-flake` writes one you can read and commit.
- Back up `/var/lib/russel` and `/etc/russel/env`. Russel can rebuild the Nix store, but not your secrets, volumes, or history.

## Checklist

- [ ] Only trusted repos, or `RUSSEL_NIX_RESTRICTED=1`
- [ ] Sandbox on with no fallback, signatures required
- [ ] `russel` isn't a trusted Nix user
- [ ] Committed, reviewed `flake.nix` and `flake.lock` for anything you didn't write

## Related

- [Builds](../concepts/builds.md) · [Security overview](./overview.md) · [Environment reference](../reference/environment.md)
