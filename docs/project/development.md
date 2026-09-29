---
title: Contributing
description: Set up a development environment, run the checks every PR needs, work on the dashboard, and edit these docs.
sidebar_position: 1
keywords: [contributing, development, clippy, tests, dashboard, docs]
---

`CONTRIBUTING.md` in the repo root is the full guide. This page covers what you need to get started.

## Setup

```bash
nix develop
cargo build -p russel-cli -p russel-ctrl
cargo test --workspace
```

The workspace has four crates: `russel-core` (shared types and Russelfile validation), `russel-cli` (the `russel` command), `russel-ctrl` (the control plane), and `russel-agent` (an experimental per-node agent). Production code may not use `unwrap` or `expect`; tests may.

## Checks for every PR

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

When you change the installer:

```bash
./contrib/tests/install_test.sh
shellcheck -x contrib/install.sh contrib/tests/install_test.sh
```

When you change the dashboard:

```bash
cd dashboard && bun test && bun run build
```

CI runs all of these, plus `systemd-analyze verify` on the service unit and a deploy of every container example.

Tests never touch the real `/var/lib/russel`: each test process gets its own temporary folder. Don't set `RUSSEL_DATA_DIR` in a test, because tests run in parallel and would share it.

## Trying a deploy by hand

For changes to the deploy path, run a local control plane and deploy an example. This needs rootless Podman and a writable `/var/lib/russel`:

```bash
cargo build
RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1 ./target/debug/russel-ctrl &
./target/debug/russel deploy "$PWD/examples/basic-http"
./target/debug/russel ps                         # find the host port
curl -sf http://127.0.0.1:<host-port>/health
./target/debug/russel destroy api
```

## Dashboard

The dashboard is an Astro app in `dashboard/`. During development, run it with Vite, which forwards `/api` to a control plane on `127.0.0.1:7878`:

```bash
cd dashboard
bun install
bun run dev      # http://127.0.0.1:4321
bun run build    # writes dashboard/dist, which russel-ctrl and the installer use
```

A control plane started from a checkout serves `dashboard/dist` if it exists. Never put an API token in the dashboard's build environment; users paste it into Settings at runtime.

## Large changes

A change too big to review in one go can land as a stack of PRs. Each PR names the one below it as its base, has its own test plan, and is safe to merge on its own (or says why it depends on the one below).

## Docs

- These pages are built by Mintlify from `docs/`. `docs/README.md` has the rules for links and pages; preview with `cd docs && npx mint dev`.
- A change to a flag, Russelfile field, or setting updates the matching reference page in the same PR, plus a guide if operators will notice it.
- Write for someone new to Russel: plain sentences, the common case first, copyable commands, and the exact error text users will see. Describe what users see and do, and leave internal function and file names out of user pages. No em dashes.

## Related

- [Changelog](./releases.md) · [Architecture](../concepts/architecture.md)
