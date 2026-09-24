---
title: Nix build security
description: Threat model for building untrusted flakes — sandboxing, restricted mode, and nix.conf hardening.
sidebar_position: 2
keywords: [nix, sandbox, restricted, threat model, flake, auto-generation]
---

# Nix build security

**Nix builds trust the source repo** — a malicious `flake.nix` runs as the build user. Single-operator trusted repos are the default posture. Anything else needs the hardening below.

## Threat model

- Attacker controls the repo (flake, builder expressions, hooks).
- Build runs as the control-plane user with that user's filesystem/network reachability.
- Impact without hardening: arbitrary code as the build user, store poisoning, exfiltration via build-time network, host-file reads through impure evaluation.
- Out of scope for this page: guest escape (see [Runtimes](../concepts/runtimes.md)), ctrl API auth (see [Security overview](overview.md)).

## Restricted mode

```bash
export RUSSEL_NIX_RESTRICTED=1   # sandboxed nix build + no auto-flake
```

- Forces sandboxed `nix build` (no impure access beyond the sandbox).
- Disables flake auto-generation (attacker cannot coax a generated flake into an unsafe shape; missing `flake.nix` fails instead).

Use it whenever the source is not fully trusted. Production multi-user hosts should default it on.

## `nix.conf` hardening

```ini
sandbox = true
sandbox-fallback = false
allowed-users = russel
trusted-users = root
require-sigs = true
allow-import-from-derivation = false
```

- Keep the store (`/nix/store`) and state (`/var/lib/russel`) on persistent, backed-up volumes.
- Build users must not be in `trusted-users` beyond what the daemon needs.
- Pin inputs (`flake.lock`); review auto-generated flakes before committing them (`russel init --with-flake` writes an owned starter instead of a restricted-mode marker).

## Auto-generation note

Auto-generated flakes (Rust/Go/static) live in the checkout and evaluate with the repo's purity. In restricted mode they are disabled — commit an explicit `flake.nix` (via `russel init --with-flake` or hand-written) so the build is reviewable.

## Checklist

- [ ] Trusted repos only, or `RUSSEL_NIX_RESTRICTED=1`
- [ ] `sandbox = true`, signatures required, least-privilege build user
- [ ] No auto-flake on untrusted sources; committed `flake.nix` reviewed
- [ ] Store + state backed up; env/secrets files `0600`

## Related

- [Builds](../concepts/builds.md) · [Security overview](overview.md) · [Environment](../reference/environment.md)
