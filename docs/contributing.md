# Contributing

How to develop, test, and open pull requests.

## Layout

| Path | Role |
|------|------|
| `crates/core` | Shared types (`Russelfile`, API) |
| `crates/cli` | `russel` CLI (crate name `russel-cli`) |
| `crates/ctrl` | `russel-ctrl` control plane |
| `crates/agent` | `russel-agent` node-local lifecycle RPC |
| `docs/` | Operator and contributor docs |
| `examples/` | Sample services |
| `.github/workflows/ci.yml` | CI gates |

## Prerequisites

- **Linux** (primary target)
- **Rust** stable (edition 2024 workspace)
- Optional for full local deploys: **Nix** (flakes), rootless **Podman**, KVM for microVMs

```bash
nix develop   # recommended
cargo build
```

## Mandatory checks

Run **all** of these before every commit that changes code, and before opening or updating a PR:

```bash
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI on PRs to `main`: `cargo check`, `cargo fmt --check`, clippy as above, `cargo test`, `cargo build --release`.

Targeted tests while iterating are fine; finish with the full workspace suite.

## Pull requests

Small, reviewable PRs. Split stacks (`gh stack`, Graphite, or base-branch chains) instead of a megapr.

1. Open or claim an issue for non-trivial work.
2. Branch from up-to-date `main`.
3. Implement with focused commits.
4. Run the mandatory checks.
5. Open a PR against `main` with summary + test plan.

Do not force-push `main`. If CI conflicts with this guide, **CI wins**.

## Coding standards

- Workspace Clippy **denies** `unwrap` / `expect` in production code. Tests may allow them on the test module only.
- Prefer `Result` + `anyhow` context over panics on operator/API paths.
- No `todo!()` on reachable production paths.
- Keep async handlers free of blocking `std::fs` / `std::process` on hot paths when an async alternative exists nearby.
- One clear responsibility per module. Domain code should not depend on Axum types.
- Extra care (and tests) for auth, git clone/URLs, Podman passthrough, secrets, TAP/ports, and writes under `/var/lib/russel`.

## Local smoke (optional)

```bash
cargo build
./target/debug/russel-ctrl &
./target/debug/russel deploy examples/basic-http -p 8080:3000 --vm-id contrib-smoke
curl -sf http://127.0.0.1:8080/health
./target/debug/russel destroy contrib-smoke
```

See [examples.md](examples.md) and the root [README](../README.md).
