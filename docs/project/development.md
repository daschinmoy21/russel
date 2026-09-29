---
title: Contributing
description: Dev setup, mandatory checks, and stacked-PR conventions.
sidebar_position: 1
keywords: [contributing, dev setup, clippy, tests, stacked prs]
---

Source of truth for workflow is `CONTRIBUTING.md` at the repo root. This page summarizes it for the docs site.

## Setup

```bash
nix develop
cargo build -p russel-cli -p russel-ctrl
cargo test --workspace
```

Crates: `russel-core` (types), `russel` / `russel-cli` (CLI), `russel-ctrl` (control plane), `russel-agent` (node RPC). Production code denies `unwrap`/`expect` (workspace lints) — `#[cfg(test)]` only.

## Mandatory checks (every PR)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./contrib/tests/install_test.sh                                # installer suite (parser, refusals, rollback, probes)
shellcheck -x contrib/install.sh contrib/tests/install_test.sh   # installer changes
cd dashboard && bun test && bun run build                        # dashboard changes
```

`cargo test` never uses the real `/var/lib/russel` or `/var/lib/microvms`. ctrl code resolves them through `crate::paths`, which pins a per-process temp dir in unit tests; integration tests call `russel_core::paths::pin_temp_roots()`. Do not set `RUSSEL_DATA_DIR` from a test: parallel tests would race on it.

CI enforces concurrency + cancel-in-progress, Rust caching, timeouts, installer tests + `systemd-analyze verify`, dashboard `bun test` before build, and shellcheck. Manual deploy smoke (for deploy-path changes):

```bash
cargo build
./target/debug/russel-ctrl &
./target/debug/russel deploy examples/basic-http
curl -sf http://127.0.0.1:8080/health
./target/debug/russel destroy api
```

Needs writable `/var/lib/russel` + rootless Podman (containers) or KVM (microVMs).

## Stacked PRs

Stacked PRs (#350–#353 install-scripts) are on `main`. New work that cannot land as one review should still be a `gh` stack: each layer names its base, keeps a focused test plan, and stays safe alone or documents why the base is required.

## Docs rules

- `reference/` + code are the contract. README drift is a bug — update docs with behavior.
- New flags/fields/env vars need all three: code, `reference/` page, and (for operator-visible paths) a guide snippet.
- Docs style follows this site: frontmatter (`title`, `description`), `What you'll need`, numbered steps, `Verify`, `Related`, copyable `bash` blocks, `| tables |` for options. The site is built by Mintlify from `docs/`; [docs/README.md](https://github.com/daschinmoy21/russel/blob/main/docs/README.md) has the page and link rules, and `cd docs && npx mint dev` previews it.

## Related

- [Changelog](./releases.md) · [Architecture](../concepts/architecture.md)
