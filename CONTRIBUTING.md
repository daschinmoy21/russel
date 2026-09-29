# Contributing to Russel

Thanks for contributing. This document is the source of truth for how to develop, test, and open pull requests against this repository.

## License and sign-off

Russel is licensed under the [Apache License 2.0](LICENSE). Unless you state otherwise, your contribution is licensed under the same terms (Apache-2.0, section 5).

### Developer Certificate of Origin

Every commit in a pull request must be signed off under the [Developer Certificate of Origin 1.1](https://developercertificate.org/) (DCO). The sign-off certifies that you wrote the change, or otherwise have the right to submit it under the project's license. It is a `Signed-off-by` trailer whose email matches the commit author:

```
Signed-off-by: Ada Lovelace <ada@example.com>
```

`git commit -s` adds it using your `user.name` and `user.email`, so set `user.email` to the address you author commits with. To sign every commit automatically, enable the repo hook once per clone:

```bash
git config core.hooksPath contrib/hooks
```

To sign off commits already on your branch (this also resets their author to your current identity, so author and sign-off match):

```bash
git rebase --exec 'git commit --amend --no-edit --reset-author --signoff' origin/main
git push --force-with-lease
```

The `DCO` workflow runs `contrib/check-dco.sh` from the base branch (a PR cannot change its own validator) and checks every non-merge commit in a pull request and fails when a sign-off is missing or does not match the author email. Run it locally with `contrib/check-dco.sh origin/main..HEAD`.

## Project layout

| Path | Role |
|------|------|
| `crates/core` | Shared types (`Russelfile`, API request/response) |
| `crates/cli` | `russel` CLI (crate name `russel-cli`) |
| `crates/ctrl` | `russel-ctrl` control plane (deploy, microVM, container, API) |
| `crates/agent` | `russel-agent` node-local agent for multi-host lifecycle RPC (heartbeat + stop/destroy/status proxies) |
| `docs/` | Architecture and operator docs |
| `examples/` | Sample services |
| `.github/workflows/ci.yml` | CI gates (must stay green) |

## Prerequisites

- **Linux** host (primary target)
- **Rust** stable (edition 2024 workspace)
- Optional for full local deploys: **Nix** (flakes), rootless **Podman**, KVM for microVMs

```bash
# Recommended: toolchain + local deps
nix develop   # if you use Nix
cargo build
```

## Mandatory checks (always run all of these)

**Before every commit that changes code, and before opening or updating a PR, run the full suite below and ensure every command exits 0.** Do not skip tests, clippy, or fmt because “it’s only a small change.”

```bash
# 1. Format (CI runs cargo fmt --check)
cargo fmt

# 2. Lint (CI: cargo clippy --workspace --all-targets -- -D warnings)
cargo clippy --workspace --all-targets -- -D warnings

# 3. All unit/integration tests (CI: cargo test)
cargo test --workspace

# 4. Optional but recommended before merge
cargo check --workspace
cargo build --release
```

CI jobs that must pass on PRs to `main`:

| Job | Command |
|-----|---------|
| Check | `cargo check` |
| Format | `cargo fmt --check` |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| Test | `cargo test` |
| Build (release) | `cargo build --release` |
| DCO sign-off (outside contributors' PRs) | `contrib/check-dco.sh` |

If CI fails, fix on your branch and push; do not merge red builds.

### Targeted re-runs while iterating

You may run a subset **while developing**, but you must still finish with the **full** workspace suite above before you consider the change done:

```bash
# Examples (not a substitute for cargo test --workspace)
cargo test -p russel-ctrl deployments::
cargo test -p russel-core
cargo test -p russel-cli
```

## Pull requests and stacks

Prefer **small, reviewable PRs**. For multi-step features (security hardening, horizontal/vertical scaling, runtime work), use **GitHub stacked pull requests** (public preview, 2026-07) or a plain base-branch chain so each layer has its own CI and review, then merge bottom-up (or one-click stack merge when available).

- Tooling: `gh stack` (preferred), Graphite `gt`, or base of PR *n+1* = head of PR *n*
- Historical example: container path stack in [`docs/audits/epic-74-container.md`](docs/audits/epic-74-container.md) (#84→#91)
- Do not open a single megapr that “will be split later” — split first

Each stack layer must pass the full mandatory checks below before review.

## Coding standards

### Rust / Clippy

- Workspace Clippy **denies** `unwrap` / `expect` in production code (`Cargo.toml` workspace lints). Tests may use `#[allow(clippy::unwrap_used, clippy::expect_used)]` on the test module only.
- Prefer explicit `Result` + context (`anyhow`) over panics on operator/API paths.
- No `todo!()` on reachable production paths.
- Keep async handlers free of blocking `std::fs` / `std::process` on hot paths when an async alternative already exists nearby.

### Structure (LLD / SOLID)

- Keep **one clear responsibility per module** (e.g. journal logic in `deployments.rs`, HTTP in `api.rs`, orchestration in `deploy.rs`).
- Domain / persistence modules should **not** depend on Axum types; map HTTP status codes at the API boundary.
- Prefer **typed errors** for control-flow decisions over string-matching error messages.
- Reuse existing helpers (metadata, ingress, runners) instead of duplicating lifecycle or path logic.
- Match existing naming, logging (`tracing`), and file layout; avoid drive-by refactors outside the PR goal.

### Security-sensitive areas

Extra care (and tests) when touching:

- Auth / `RUSSEL_API_TOKEN`
- `git` clone / URL handling
- Podman passthrough args
- Secrets store and env resolution
- Network / TAP / port allocation
- Metadata and journal writes under `/var/lib/russel`

### Tests and host state

Tests must not touch the real host roots. In `russel-ctrl`, resolve paths with `crate::paths::{data_root, service_dir, microvms_root, microvm_dir}`: in unit tests the first call pins them to a per-process temp dir. Integration tests call `russel_core::paths::pin_temp_roots()` (see `crates/ctrl/tests/api_integration.rs`). Do not set `RUSSEL_DATA_DIR` from a test; parallel tests would race on it.

## Workflow

1. **Open an issue** (or claim an existing one) for non-trivial work.
2. **Branch** from up-to-date `main`:
   ```bash
   git fetch origin
   git checkout -b fix/short-description origin/main
   ```
3. **Implement** with small, focused commits, each signed off (`git commit -s`, see [Developer Certificate of Origin](#developer-certificate-of-origin)).
4. **Always run all checks** (format, clippy, full `cargo test --workspace`).
5. **Push** and open a PR against `main`.
6. Link issues (`Fixes #N` / `Implements #N`), fill the PR body with summary + test plan.
7. Keep the PR green; address review comments with new commits or a tidy history (no force-push to `main`).

### PR expectations

- Description: what changed, why, how to verify.
- Test plan: paste the commands you ran (must include full workspace tests when code changed).
- Scope: one concern per PR when practical; split large features.
- Docs: update `README.md` / `docs/` when behavior or APIs change.
- Draft PRs are fine for early review; mark ready only when checks pass.

## Local control-plane smoke (optional)

Not required for pure unit-test PRs; useful for deploy-path changes:

```bash
cargo build
./target/debug/russel-ctrl &
./target/debug/russel deploy examples/basic-http
curl -sf http://127.0.0.1:8080/health
./target/debug/russel destroy api
```

Requirements: writable `/var/lib/russel`, rootless Podman (container example), or KVM for microVM examples. See `README.md` and `docs/examples.md`.

## Commit messages

Prefer conventional, scoped subjects:

```
feat(ctrl): …
fix(cli): …
docs: …
test(ctrl): …
chore: …
```

Body should explain *why* when the diff is non-obvious.

## Code of collaboration

- Be precise in reviews; prefer code-backed claims (file/line).
- Assume good intent; suggest alternatives when blocking.
- Security and data-loss issues block merge until resolved.
- Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md), not in public issues or PRs.

## Questions

- Architecture: `docs/architecture.md`
- API surface: `README.md` (API table)
- Examples: `docs/examples.md` and `examples/`

If something in this guide conflicts with CI, **CI wins** — update this file in the same PR that changes workflow commands.
