---
title: Changelog
description: What changed in each Russel release that affects people running it.
sidebar_position: 2
keywords: [changelog, releases, init, login, nixos, install]
---

Curated, operator-visible changes. Full history is `git log`.

## v0.1.0 (2026-09-30): first public release

The first release: everything below this entry, plus these changes. Two earlier tags (v0.1.0 on 2026-09-29 and v0.1.1) were withdrawn and folded into this one.

- **Container deploys are faster** (#540). The control plane asks Podman for its settings once instead of on every deploy, skips stopping and removing on a first deploy, and waits 75 ms instead of 200 ms to confirm the app is up. Spawn-to-ready in the bench dropped by 150 to 420 ms per app, to within about 100 ms of a plain `podman run`.
- **Stopping a container reaches the app** (#540). Containers run with `--init`, so Podman's init is PID 1 and passes SIGTERM on. Before, an app with no SIGTERM handler of its own (meilisearch) ignored it as PID 1, so every stop and replace waited 10 s and then killed it. On a host without catatonit (it is only recommended by Debian's podman package), the control plane logs one warning and runs containers without `--init`.
- An update keeps the service's `[ingress].port` pin (#538). Before, the first `russel update` of a pinned container or passt microVM moved it to an allocated port. Pinned services now update cold; one `russel update` puts a lost pin back.
- **Containers start under `install.sh host` and the NixOS module** (#524). Before, every container failed: Podman asked the `russel` account's user manager for a systemd scope, which systemd refuses for a process in a system unit. The control plane now runs Podman with `--cgroup-manager=cgroupfs` when it runs unprivileged in a delegated system unit. `--cpus` and `--memory` still apply.
- **The unit drops `PrivateTmp`, `ProtectSystem`, and `ProtectHome`** (#524). Podman's pause process outlives the unit and kept the first control plane's private `/tmp`, which systemd deleted on restart. Re-running `install.sh host` replaces a pre-release unit even without `--force-unit`, keeps your drop-ins, and ends the stale pause process. `[[volumes]].host` roots no longer need `ReadWritePaths=`. See [systemd and NixOS](../operations/systemd-nixos.md).
- **The build runs in the Russelfile's folder joined with `service.source`** (#525). Before, it always ran at the repo root and `source` was only validated. `russel deploy https://github.com/daschinmoy21/russel.git --config examples/basic-http/Russelfile.toml` now builds that example, so the quickstart needs no local checkout or `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY`. A Russelfile in a subfolder that relied on building the repo root must now set `source` to reach it, or move to the root.
- `russel ps` no longer lists `.config`, `.local`, or `.cache` in `/var/lib/russel` as services (#526). Every dot-directory in the data root is skipped.
- No cleartext-token warning for a loopback control plane (#527). Plain HTTP to a non-loopback host is still refused without `--insecure`.
- `install.sh check` finds `useradd` and `usermod` in `/usr/sbin` when run without `sudo`, and `host` waits up to 30 s for a freshly started control plane instead of reporting a good install as failed.

## 2026-09-28: an update can't replace a working version with one that crashes on start-up (#493)

- **Updates and rollbacks (dual-live)** switch traffic to the new version as soon as it answers, then keep the previous version running for 2 s (`switch · New version is live; keeping the previous one running for 2s in case it crashes`). If the new version dies in that window, traffic goes back to the previous one, the new one is torn down, and `apply` fails with `new version crashed within 2s of answering` and the app's last output. Before, one accepted connection was enough: an app that listened and then crashed (postgres without `/dev/shm`) reported `deployed`, and the previous version was already drained. Both runtimes.
- **Cold updates** (services with `[[ports]]`) require the new version to stay up for 2 s after it answers; a crash restores the previous version from its backup (`rolled_back`).
- **First deploys** are unchanged in speed: `deployed` at the first answer. A crash right after is reported within about a second by the supervisor (microVMs) or the exit watcher (containers).
- A microVM whose guest exits before the app answers fails at once with `app exited during startup (exit code N)` and the guest console tail, instead of after the 30 s readiness timeout.
- Fixed: a failed container cold update never rolled back. The rollback looked for the previous rootfs at its live path while it sat under `.bak`, and failed with `previous rootfs_path no longer exists`.
- Details and trade-offs: [When a deploy counts as ready](../concepts/lifecycle.md#when-a-deploy-counts-as-ready).

## 2026-09-28: a dedicated `russel` account, and host checks (#515, breaking for existing `host` installs)

- `install.sh host` now runs as root (`sudo ./contrib/install.sh host`, or `curl … | sudo RUSSEL_VERSION=… bash -s -- host`). It creates an unprivileged `russel` system account with its own subuid/subgid range and linger, and installs `/etc/systemd/system/russel-ctrl.service` running as `User=russel`. This is the same model as the NixOS module. Before, russel-ctrl and every container ran as the operator's own login account, so an app that escaped its container landed next to that account's SSH keys and sudo.
- The token moves to `/etc/russel/env` (`root:russel`, `0640`). The operator who ran `sudo` joins the `russel` group and can `russel login --token-file /etc/russel/env` after logging in again. On a host that had the old per-user install, the token in `~/.config/russel/env` is carried over.
- The unit puts the Nix daemon profile on `PATH` and points rootless Podman at the account's runtime dir and user bus, so builds find `nix` and `podman run --memory` works under a system unit.
- New `install.sh check` lists what the host is missing (Podman 4+, uidmap, a systemd user bus, a multi-user Nix with flakes, git, cgroup v2), with the Debian/Ubuntu command that fixes each. `host` runs it first and stops on failures; `--skip-checks` overrides that.
- `host` refuses to run next to the old per-user install and prints the steps to move over: destroy the services, disable the user unit, then `sudo … --take-state-ownership host`, which now hands the whole `/var/lib/russel` tree to `russel`.
- Connection hints say `systemctl status russel-ctrl` (installer) and `systemctl status russel` (NixOS). The old hint named `russel-ctrl` for NixOS, whose unit is `russel.service`.

## 2026-09-26: Nix GC roots for deployed generations (#411)

- Ctrl roots every store path a service may re-exec under `_pool/gcroots/<id>/`: the current metadata's app, kernel, initramfs, and rootfs paths, plus the journal's `active` and `previous` generations. It syncs after deploys and automatic rollbacks, and for all services at startup, so hosts upgraded from earlier builds get roots on the next ctrl start. Destroy removes them.
- Container rollback fails before renaming anything when the previous `rootfs_path` no longer exists. MicroVM rollback already checked `app_path` and `kernel_path`.

## 2026-09-26: one port model (#385)

- `service.port` is the guest listen port and `PORT` in every case. A host-side port pin is configured with `[ingress].port`; `service.port` remains the guest listen port and `PORT`.
- `[ingress].port` pins the host side. `[ingress].port` and `[[ports]]` hosts share one rule (`validate_publish_host_port`: not 0, `>= 1024`, not 7878/7946).
- An `[ingress].port` that equals a `[[ports]]` host is rejected instead of publishing the port twice. `[[ports]]` stays additional listeners only (microVM support is #386).

## 2026-09-26: GPL source for the kernel, dashboard third-party licenses

- New flake package `packages.x86_64-linux.microvm-kernel-source`: one tar with the upstream `linux-<version>.tar.xz` the kernel is built from, the nixpkgs patches in apply order, the generated `.config`, Russel's `flake.nix` / `flake.lock` / `nix/microvm-kernel.nix`, nixpkgs' `pkgs/os-specific/linux/kernel` build expressions, and a `README` with the nixpkgs rev and rebuild steps.
- `contrib/release-kernel.sh` builds it next to the bzImage and uploads it as `russel-kernel-<tag>-source.tar` with a `SHA256SUMS` line. `--publish` refuses to publish without it, so the GPL-2.0 kernel binary never ships without its corresponding source. A `release.yml` re-run keeps its checksum line like the kernel's.
- `bun run build` writes `dist/THIRD-PARTY-LICENSES.txt` (license and license text of every package in the dashboard's production npm closure), so `russel-dashboard-<tag>.tar.gz` carries it. The release workflow fails if it is missing.
- `NOTICE` and the release notes point at both.

## 2026-09-26: Russelfile contract for v0.1 (breaking)

- `[database.postgres]` and `[database.redis]` are gone from the schema. They were placeholders (`enabled = true` already failed load) and skipped `deny_unknown_fields`, so `[database.mysql]` loaded silently. Any `[database]` table now fails load with ``unknown field `database` ``. Delete it; run Postgres or Redis as its own `service.package` service with `[[volumes]]` (`examples/postgres`, `examples/redis`). `russel init` no longer emits the commented stub.
- Load enforces the name, bin, and env rules the reference states. `service.name` uses the service id rule (ASCII `[A-Za-z0-9_-]`, 1–128, not `secrets`/`traefik`/`_pool`), the same rule `russel init --name` now reuses from `russel_core::ids`; `my.app`, `my app`, and non-ASCII names fail load. `service.bin` keeps the wider `[A-Za-z0-9._+-]`, max 256 rule and is checked at load too. `[service.env]` goes through `validate_env_map` at load, so `PORT = "1"` fails before any build. The reference, env guide (its example was missing `source`), and the init template (`1gb` is not a valid memory value; `LD_AUDIT` was missing from the reserved list) are corrected, and a test loads every `examples/**/Russelfile.toml`.
- `service.args` and the CLI's trailing tokens are no longer both called args. `service.args` is process argv after the entrypoint and stays in the Russelfile; it is container-only for now, and a microVM file with non-empty `args` fails load instead of having them silently dropped by the guest init. Extra `podman run` flags are configured in Russelfile `service.podman_args` (one token per entry; API field is `podman_args`).

## 2026-09-26: kernel uploaded from the maintainer's build

- `release.yml` no longer builds the microVM kernel (linux from source ran past 2h on hosted runners and the first release run was cancelled). It builds the binaries, dashboard, `LICENSE`, and `NOTICE`, and leaves a **draft** release. A re-run keeps an uploaded kernel's `SHA256SUMS` line and fails instead of dropping it.
- `contrib/release-kernel.sh <tag> [--repo OWNER/NAME] [--publish]` checks that the local tag matches the release repo's tag, builds `packages.x86_64-linux.microvm-kernel` at that commit, uploads `russel-kernel-<tag>-x86_64.bzImage`, adds it to `SHA256SUMS`, and reads the sums back to confirm the line landed. `--publish` takes the release out of draft only once every asset is present with a checksum line. `--repo` defaults to `daschinmoy21/russel`, where `contrib/install.sh` downloads from.
- The publish job sets `GH_REPO`: it has no checkout, so `gh release` could not resolve the repository before.

## 2026-09-25: tests stay off the host state root

- `cargo test` no longer reads or writes `/var/lib/russel` or `/var/lib/microvms` (#412). ctrl unit tests pin both roots to a per-process temp dir (`russel-test-<pid>-…` under `$TMPDIR`, removed by the next run once the process is gone). The API integration tests do the same, clear the roots before each test, and point Podman at a missing binary so `GET /vms` does not list the host's real containers.
- Before this, a test run on a host with a writable `/var/lib/russel` overwrote `ctrl-catalog.json` with fixture services, and `vms_list_empty_when_no_services` failed on any host with a live Russel container. The catalog is informational and ctrl rewrites it on the next state change.
- ctrl code resolves host paths through `russel_ctrl::paths`; `crates/ctrl/clippy.toml` bans the direct `russel_core::paths` calls. `/var/lib/microvms` literals go through `paths::microvms_root()` (still `/var/lib/microvms` in production; making it configurable is #413).
- The port-hold tests use OS-assigned ports instead of fixed 4010–4012, which failed when another process on the host held them.

## 2026-09-24: release workflow and `--version`

- Release workflow builds `russel` + `russel-ctrl` on Ubuntu 22.04 (glibc 2.35), the dashboard dist, and `nix build .#microvm-kernel`, then refuses to publish if `russel-kernel-<tag>-x86_64.bzImage` is missing or under 1MB.
- `russel --version` prints the crate version.
- `install.sh host` on a KVM machine fails closed when the kernel asset is absent. Optional `RUSSEL_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` for private GitHub release URLs.

## 2026-09-20: release artifacts, download mode, KVM-gated kernel

- New `.github/workflows/release.yml` on `v*` tags: builds `russel` + `russel-ctrl`, tars the dashboard `dist`, runs `nix build .#microvm-kernel`, writes `SHA256SUMS`, and uploads everything with `gh release upload`.
- Assets: `russel-<tag>-x86_64`, `russel-ctrl-<tag>-x86_64`, `russel-ctrl-<tag>.service`, `russel-dashboard-<tag>.tar.gz`, `russel-kernel-<tag>-x86_64.bzImage`, `SHA256SUMS`.
- `contrib/install.sh` download mode: `RUSSEL_VERSION` (or a missing `target/release`) fetches the assets and verifies each one against `SHA256SUMS`. `RUSSEL_RELEASE_BASE` overrides the artifact base URL.
- The installer is safe to pipe on stdin (`curl … | RUSSEL_VERSION=… bash -s -- host`): it no longer assumes `BASH_SOURCE` points at a checkout root.
- `host` fetches the microVM kernel only when `/dev/kvm` exists, and installs it at `/var/lib/russel/_pool/kernel/bzImage` with operator ownership. Container-only hosts print a skip. `all` never fetches the kernel.
- Docs: release one-liner, glibc vs NixOS binary compatibility, and the KVM kernel gate.

## 2026-09-11: install-scripts stack (#350–#353) + CLI/host packaging

**On `main`:**

- CLI ships as `russel` (crate `russel-cli`); `russel-cli` binary name retired. `ps` replaces `vms` (`list`, `vms` kept as aliases).
- New: `russel init` (scaffold `Russelfile.toml` + `--with-flake`), `russel login/logout/origin` (`~/.config/russel/config.toml`, `0600`, env wins).
- Improved loopback/remote connect diagnostics (anchored SSH hint, systemd + `ss` in the lock message); contention tests use tempfiles.
- Installer: `install.sh host` (user-systemd, refuses NixOS/root), `connect user@host` (anchored forward), `status` (endpoint/listener/unit/origin probe). Restart failure restores the prior binary; tokens never printed.
- Dashboard: default API base `/api` everywhere; `401` (re-enter token) distinguished from offline (tunnel/proxy); Settings presets same-origin/tunneled/custom-HTTPS; no CORS.
- Docs: topologies A (same host) / B (split + SSH) / C (split + HTTPS) without pet hostnames; TLS examples strip `/api` and keep NDJSON unbuffered; Fish guidance via `login --token-file`.
- Control plane: `repo_url` userinfo redacted in logs/metadata; microVM `deploy.env` mounted read-only with separate `scratch/` share for `.agent_ready`; `service.guest` (`busybox` default, `linux` parsed-then-rejected); filebrowser example requires auth + loopback.
- NixOS module `services.russel` (`nixosModules.russel` / `.default`): loopback, `REQUIRE_AUTH=1`, `0700` state, rootless-Podman integration.

## 2026-08-15: audit remediation (`deslop/audit-2026-08-15`)

- P0 fixes: state-lock I/O outside the mutex, atomic metadata writes, shared constant-time tokens, bounded podman calls, `rm -rf` → fs, multibyte truncation, 400/500 split, loud init, GC outside lock.
- Dead-code purge (12 items), dep hygiene (`core -tokio`, `agent -serde`, `cli -serde/tracing`), docs de-sprawl (vendored CH docs deleted), CI hardening (concurrency/cache/timeouts/dashboard/shellcheck).
- Follow-ups: `ServiceStatus`/`VmState` enums, scary-module tests (warm_pool, ch_api parser, rollback, CLI NDJSON), `bench-common.sh` + `flock`, dashboard dedupe.

## 2026-07-24: benchmarks

Warm-run E2E favors Russel containers over raw-podman Dockerfile builds (Nix cache); spawn-to-ready favors raw podman ≤ Russel container < microVM. See [Benchmarks](../guides/benchmarks.md).
