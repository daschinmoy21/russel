---
title: Changelog
description: Release-relevant changes — CLI renames, init, login, NixOS module, and the install stack.
sidebar_position: 2
keywords: [changelog, releases, init, login, nixos, install]
---

# Changelog

Curated, operator-visible changes. Full history is `git log`.

## 2026-09-24 — v0.1.0

- First git tag. Release workflow builds `russel` + `russel-ctrl` on Ubuntu 22.04 (glibc 2.35), the dashboard dist, and `nix build .#microvm-kernel`, then refuses to publish if `russel-kernel-<tag>-x86_64.bzImage` is missing or under 1MB.
- `russel --version` prints the crate version.
- `install.sh host` on a KVM machine fails closed when the kernel asset is absent. Optional `RUSSEL_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` for private GitHub release URLs.

## 2026-09-20 — release artifacts, download mode, KVM-gated kernel

- New `.github/workflows/release.yml` on `v*` tags: builds `russel` + `russel-ctrl`, tars the dashboard `dist`, runs `nix build .#microvm-kernel`, writes `SHA256SUMS`, and uploads everything with `gh release upload`.
- Assets: `russel-<tag>-x86_64`, `russel-ctrl-<tag>-x86_64`, `russel-ctrl-<tag>.service`, `russel-dashboard-<tag>.tar.gz`, `russel-kernel-<tag>-x86_64.bzImage`, `SHA256SUMS`.
- `contrib/install.sh` download mode: `RUSSEL_VERSION` (or a missing `target/release`) fetches the assets and verifies each one against `SHA256SUMS`. `RUSSEL_RELEASE_BASE` overrides the artifact base URL.
- The installer is safe to pipe on stdin (`curl … | RUSSEL_VERSION=… bash -s -- host`): it no longer assumes `BASH_SOURCE` points at a checkout root.
- `host` fetches the microVM kernel only when `/dev/kvm` exists, and installs it at `/var/lib/russel/_pool/kernel/bzImage` with operator ownership. Container-only hosts print a skip. `all` never fetches the kernel.
- Docs: release one-liner, glibc vs NixOS binary compatibility, and the KVM kernel gate.

## 2026-09-11 — install-scripts stack (#350–#353) + CLI/host packaging

**On `main`:**

- CLI ships as `russel` (crate `russel-cli`); `russel-cli` binary name retired. `ps` replaces `vms` (`list`, `vms` kept as aliases).
- New: `russel init` (scaffold `Russelfile.toml` + `--with-flake`), `russel login/logout/origin` (`~/.config/russel/config.toml`, `0600`, env wins).
- Improved loopback/remote connect diagnostics (anchored SSH hint, systemd + `ss` in the lock message); contention tests use tempfiles.
- Installer: `install.sh host` (user-systemd, refuses NixOS/root), `connect user@host` (anchored forward), `status` (endpoint/listener/unit/origin probe). Restart failure restores the prior binary; tokens never printed.
- Dashboard: default API base `/api` everywhere; `401` (re-enter token) distinguished from offline (tunnel/proxy); Settings presets same-origin/tunneled/custom-HTTPS; no CORS.
- Docs: topologies A (same host) / B (split + SSH) / C (split + HTTPS) without pet hostnames; TLS examples strip `/api` and keep NDJSON unbuffered; Fish guidance via `login --token-file`.
- Control plane: `repo_url` userinfo redacted in logs/metadata; microVM `deploy.env` mounted read-only with separate `scratch/` share for `.agent_ready`; `service.guest` (`busybox` default, `linux` parsed-then-rejected); filebrowser example requires auth + loopback.
- NixOS module `services.russel` (`nixosModules.russel` / `.default`): loopback, `REQUIRE_AUTH=1`, `0700` state, rootless-Podman integration.

## 2026-08-15 — audit remediation (`deslop/audit-2026-08-15`)

- P0 fixes: state-lock I/O outside the mutex, atomic metadata writes, shared constant-time tokens, bounded podman calls, `rm -rf` → fs, multibyte truncation, 400/500 split, loud init, GC outside lock.
- Dead-code purge (12 items), dep hygiene (`core -tokio`, `agent -serde`, `cli -serde/tracing`), docs de-sprawl (vendored CH docs deleted), CI hardening (concurrency/cache/timeouts/dashboard/shellcheck).
- Follow-ups: `ServiceStatus`/`VmState` enums, scary-module tests (warm_pool, ch_api parser, rollback, CLI NDJSON), `bench-common.sh` + `flock`, dashboard dedupe.

## 2026-07-24 — benchmarks

Warm-run E2E favors Russel containers over raw-podman Dockerfile builds (Nix cache); spawn-to-ready favors raw podman ≤ Russel container < microVM. See [Benchmarks](../guides/benchmarks.md).


