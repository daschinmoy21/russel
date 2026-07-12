# Russelfile

This document describes the proposed project manifest and developer workflow for Russel.

The goal is for Nix to remain Russel's build and environment engine while Russel provides the user-facing interface. End users should be able to write application code, add a `Russelfile.toml`, and run Russel commands without needing to know the equivalent Nix commands directly.

Power users can still provide and customize a `flake.nix` when they need full Nix functionality.

## Design Goals

Russel aims to provide a workflow like:

```bash
russel init          # planned — not yet implemented
russel develop       # planned — not yet implemented
russel build         # planned — not yet implemented
russel deploy        # implemented — POST /deploy via control plane
```

Internally, these commands may invoke Nix, but Nix should be an implementation detail for normal users.

The important architectural rule is:

> `russel build`, `russel develop`, and `russel deploy` must use the same project resolver and build configuration.

This prevents the local development environment from drifting away from the artifact that deployment builds.

## The Russelfile Does Not Need to Be TOML

`Russelfile.toml` is convenient for the current MVP, but Russel should not be limited to TOML. The important contract is the Russel project model, not the file format.

A Russelfile should be a small abstraction over Nix with concepts that application developers understand:

- packages needed to build;
- packages needed for local development;
- packages needed at runtime;
- the build command;
- the output artifact or binary;
- the local development shell;
- the runtime command;
- service metadata such as ports and memory.

A future line-oriented `Russelfile` could look like this:

```text
name api
port 3000
memory 256mb

packages build pkg-config openssl
packages dev rust-analyzer cargo-watch
packages runtime cacert

build cargo build --release
artifact target/release/api

shell
  cargo run
end

exec ./target/release/api
```

Russel would compile this into Nix concepts roughly as follows:

| Russelfile concept | Nix concept | Purpose |
|--------------------|--------------|---------|
| `packages build` | derivation build inputs | Dependencies available while producing the artifact |
| `packages dev` | `pkgs.mkShell.packages` | Tools available in `russel develop` |
| `packages runtime` | runtime closure or wrapper inputs | Dependencies required by the deployed service |
| `build` | derivation build phase | Produces the deployment artifact |
| `artifact` | package output | Identifies what Russel deploys |
| `shell` | `devShells.<system>.default` shell hook or command | Local development behavior |
| `exec` | VM entrypoint / `apps.<system>.default` | Starts the service process |

The exact syntax can evolve. A dedicated Russelfile format may be easier to read than TOML while remaining much safer and simpler than exposing arbitrary Nix expressions.

## YAML as the Native Format

YAML is a strong candidate for the native Russelfile format. It is more expressive than the current TOML schema for ordered commands, nested environment configuration, secrets, and build phases, while still being familiar to developers who use Docker Compose or CI configuration.

A possible `Russelfile.yaml` could look like this:

```yaml
version: 1

service:
  name: api
  port: 3000
  memory: 256mb
  exec: ["/bin/api"]

packages:
  build:
    - pkg-config
    - openssl
  dev:
    - rust-analyzer
    - cargo-watch
  runtime:
    - cacert

check:
  - [cargo, test]

build:
  - [cargo, build, --release]

artifact:
  source: target/release/api
  target: /bin/api

develop:
  command: [cargo, run]

env:
  LOG_LEVEL: info
  APP_ENV: production

secrets:
  DATABASE_URL:
    provider: project/api
    key: database-url
```

By default, command arrays should be executed without an intermediate shell. This makes argument boundaries clear and avoids accidental shell expansion:

```yaml
build:
  - [cargo, test]
  - [cargo, build, --release]
```

Shell syntax should be an explicit opt-in for pipelines, redirects, or shell features:

```yaml
build:
  - shell: cargo test && cargo build --release
```

This gives users the convenience of build commands without making every command an opaque shell script. Both forms should execute inside the Nix build sandbox when used for a deployment build.

### Why YAML may be preferable to Dockerfile syntax

YAML can represent the complete Russel model directly:

- build and check commands are naturally ordered lists;
- artifact source and destination can be structured fields;
- environment values and secret references have distinct sections;
- runtime configuration does not need to be disguised as `CMD` or `EXPOSE`;
- dependency categories do not need to be inferred from `RUN` commands;
- the schema can be validated before generating Nix.

YAML also has risks. Russel should use a YAML 1.2 parser, reject unknown fields, validate types strictly, and avoid surprising implicit values such as `yes`, `no`, or numeric-looking strings. A single canonical filename such as `Russelfile.yaml` is preferable to silently searching many formats.

Manifest format contract and precedence

- Russelfile.yaml takes precedence over Russelfile.toml when both exist.
- If both exist, Russelfile.yaml is used for all generation and runtime behavior.
- Russelfile.toml remains supported for backwards-compatibility and is used only if YAML is not present.
- russel init reads Russelfile.yaml when present; if YAML is missing, it reads Russelfile.toml; when both exist, YAML wins.
- For core commands (russel build, russel deploy, russel check), the same precedence applies: YAML if present, otherwise TOML.
- In managed mode, custom-flake metadata, and dependency handling, Russelfile.yaml governs the behavior; TOML is only used if YAML is absent.
- TOML compatibility is maintained: existing TOML inputs should continue to work with legacy flows.

## Dockerfile Compatibility

Using a Dockerfile directly would reduce the learning curve, but Russel should distinguish between **Dockerfile syntax** and **Docker image semantics**.

A Dockerfile-compatible input could be a useful frontend:

```dockerfile
FROM rust
WORKDIR /src
COPY . .
RUN cargo test
RUN cargo build --release
ENV PORT=3000
EXPOSE 3000
CMD ["/bin/api"]
```

Russel could translate a constrained subset into its project model:

| Dockerfile instruction | Russel meaning |
|------------------------|----------------|
| `FROM` | A Russel language/runtime preset, not an OCI image layer |
| `WORKDIR` | Build working directory |
| `COPY` / `ADD` | Source or generated input selection |
| `ENV` | Non-secret build or runtime environment value |
| `RUN` | Ordered command in the Nix build sandbox |
| `EXPOSE` | Guest service port metadata |
| `CMD` / `ENTRYPOINT` | Runtime `exec` command |

However, Russel should not claim to support every Dockerfile. Several Docker concepts do not map cleanly:

- `FROM ubuntu` implies a mutable root filesystem, while Russel packages a Nix closure;
- `RUN apt-get install ...` assumes a Debian image and network access during the build;
- Docker layer caching is different from Nix derivation caching;
- arbitrary `RUN` commands may depend on undeclared tools or network access;
- OCI image entrypoints and environment behavior are not identical to a microVM artifact.

A Dockerfile compatibility mode should therefore reject unsupported instructions with actionable errors instead of silently guessing. For example:

```text
Unsupported Dockerfile instruction: RUN apt-get install openssl
Declare 'openssl' as a Russel/Nix dependency instead.
```

### Recommended adoption path

1. Keep the internal project model independent of any file format.
2. Support the proposed `Russelfile` syntax as the native frontend.
3. Add `russel import dockerfile` to convert common Dockerfiles into a Russelfile and/or generated flake.
4. Optionally detect a file named `Dockerfile` and use the constrained compatibility frontend.
5. Preserve `flake.nix` as the advanced escape hatch.

The conversion command is safer than treating arbitrary Dockerfiles as fully compatible:

```bash
russel import dockerfile
russel check
russel build
```

A Dockerfile should be accepted directly only when every instruction can be translated into the same reproducible build, development-shell, artifact, and runtime model used by a native Russelfile.

### `mkShell`, `build`, and `exec` are different things

These concepts should not be conflated:

- `mkShell` describes the local development environment. It should be used by `russel develop`.
- `build` describes how the reproducible deployment artifact is produced. It should be used by `russel build` and `russel deploy`.
- `exec` describes the long-running application process inside the microVM. It should be used as the VM entrypoint.
- A future `russel exec -- <command>` could run an arbitrary command inside the development environment, but that is separate from the service's runtime `exec` instruction.

For example:

```bash
russel develop       # enter the generated mkShell environment
russel build         # produce the Nix package
russel deploy .      # build, boot, and run the configured exec command
russel exec -- bash  # optional: run a command in the development environment
```

Commands in a Russelfile should be compiled into Nix rather than executed directly on the host wherever reproducibility matters. The development shell may execute interactively, but deployment builds and runtime closures must remain isolated and reproducible.

## Build Blocks

A sequential build block is a good abstraction for ordinary projects:

```text
build
  cargo test
  cargo build --release
end

artifact target/release/api
```

The semantics should be:

1. Commands run in the Nix build sandbox, not directly on the host.
2. Commands run in the declared order.
3. A non-zero exit status stops the build and prevents deployment.
4. `cargo build release` may be supported as shorthand, but the canonical form should remain `cargo build --release`.
5. The explicit `artifact` declaration identifies what Russel packages and deploys.

The equivalent generated Nix derivation would conceptually run `cargo test` during its check phase and `cargo build --release` during its build phase, then copy the declared artifact into `$out`.

The artifact must remain explicit because a project may produce multiple files. For example, tests, static assets, migrations, and several binaries may all exist under `target/`, but Russel needs to know which output is the service entrypoint.

A more explicit form could be supported when phase separation matters:

```text
check
  cargo test
end

build
  cargo build --release
end

artifact target/release/api -> /bin/api
exec /bin/api
```

The short combined form should be convenient, while the separated form should map more directly to Nix's `checkPhase`, `buildPhase`, and install/output steps.

### Build tests and deployment

Tests in the build block should run by default. If `cargo test` fails, `russel build` and `russel deploy` must fail rather than booting an unverified artifact.

Tests should be deterministic and compatible with the Nix sandbox. Tests that require external services or network access should declare those services explicitly or be run through a separate opt-in command such as:

```bash
russel test --local
```

Deployment should not silently skip tests. If a future `--skip-checks` option is added, it should be explicit, clearly displayed, and primarily intended for development/debugging.

### Local and deployment commands use the same build definition

The following commands should all use the same Russelfile build block:

```bash
russel build       # run checks, build, and report the artifact
russel deploy .    # run the same build, then boot the artifact
russel develop     # enter the development shell; does not create a deployment artifact
```

This gives the user Dockerfile-like sequential commands without giving up Nix's sandboxing, caching, dependency closure, and reproducibility.

## Configuration Modes

Russel should support two configuration modes.

### Managed mode

A project can use only `Russelfile.toml` for simple builds. Russel generates the required Nix expressions from the manifest:

- `packages.<system>.default`
- `devShells.<system>.default`
- optionally `apps.<system>.default`

For example:

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"

[dependencies]
build = ["pkg-config", "openssl"]
runtime = ["cacert"]
dev = ["rust-analyzer"]
```

The resulting workflow is:

```bash
russel develop
russel build
russel deploy .
```

The user does not need to run `nix develop` or `nix build` directly.

### Custom flake mode

If a project contains a committed `flake.nix`, Russel should use it instead of generating one.

In this mode:

- the flake controls `packages.<system>.default`;
- the flake controls `devShells.<system>.default`;
- `Russelfile.toml` controls runtime and deployment metadata.

A custom flake is appropriate for advanced requirements such as:

- custom build phases;
- patched sources;
- overlays;
- multiple packages;
- cross-compilation;
- platform-specific logic;
- custom shell hooks;
- complex service composition.

A minimal manifest for a custom flake might be:

```toml
[service]
name = "api"
port = 3000
memory = "256mb"
bin = "api"
```

This model avoids forcing advanced Nix expressions into TOML.

## Why TOML Should Not Replace Nix Completely

TOML is well suited to simple declarative project and runtime metadata. It is not a practical replacement for arbitrary Nix expressions.

Nix is still needed for features such as:

- conditional build logic;
- overlays and package overrides;
- custom derivations;
- multiple outputs;
- cross-compilation;
- generated sources;
- non-trivial build phases;
- package-specific runtime configuration.

Therefore:

> `Russelfile.toml` is the simple project manifest. `flake.nix` remains the advanced build-system escape hatch.

## Dependency Declaration

The proposed `[dependencies]` section describes system packages and developer tools used to generate the build and development environments:

```toml
[dependencies]
build = ["pkg-config", "openssl"]
runtime = ["cacert"]
dev = ["rust-analyzer", "cargo-watch"]
```

The values should be interpreted as nixpkgs attribute paths. Russel should validate them before generating a flake and provide a helpful error for invalid names:

```text
Dependency 'opnssl' was not found in nixpkgs.
Did you mean 'openssl'?
```

### Dependency categories

| Category | Purpose | Example |
|----------|---------|---------|
| `build` | Tools and libraries needed while compiling | `pkg-config`, `openssl` |
| `runtime` | Packages needed by the deployed application | `cacert`, `zlib` |
| `dev` | Tools needed for local development | `rust-analyzer`, `cargo-watch` |

Language-level dependencies should continue to use the language's package manager and lockfile:

- Rust dependencies come from `Cargo.toml` and `Cargo.lock`.
- Go dependencies come from `go.mod` and `go.sum`.
- Node dependencies come from `package.json` and the relevant lockfile.

The TOML dependency section should primarily describe native/system packages and development tools rather than replace language package managers.

## Dependency Consistency

When Russel generates a flake, it should generate the build and development environments from the same dependency declarations. This prevents declared dependencies from being silently omitted.

For custom flakes, Russel should not initially promise complete unused-dependency detection. Nix dependencies can be introduced indirectly through:

- package closures;
- overlays;
- transitive derivations;
- shell hooks;
- compiler configuration;
- generated files.

Instead, Russel should provide project validation through a future command:

```bash
russel check
```

The first version of `russel check` could validate:

- `Russelfile.toml` syntax;
- dependency names;
- flake syntax;
- required package outputs;
- required `devShell` outputs;
- configured service binary names;
- port and runtime configuration.

## Environment and Secret Handling

Environment values should be divided into three categories:

1. **Build environment** — values and credentials needed while producing the artifact.
2. **Development environment** — values loaded when entering `russel develop`.
3. **Runtime environment** — values injected into the application immediately before its configured `exec` command starts.

These environments should not automatically share all values with one another. In particular, a production secret should not become available during a local build or be copied into a Nix derivation.

### Public and non-sensitive environment values

Non-sensitive defaults can be declared in the Russelfile:

```text
env LOG_LEVEL=info
env PORT=${service.port}
env APP_ENV=production
```

Equivalent TOML could be:

```toml
[env]
LOG_LEVEL = "info"
APP_ENV = "production"
```

### Local development values

Local-only values should normally come from the developer's environment or an ignored file such as `.env.local`:

```bash
russel develop
```

Russel may load `.env.local` for `russel develop`, but it should:

- never commit the file;
- never copy it into the Nix store;
- never include it in build logs;
- never automatically use it for production deployment.

A separate explicit option can be used for local deployment testing:

```bash
russel deploy . --env-file .env.local
```

The default should be conservative: environment values are inherited only when the user explicitly opts into passing them to a deployment.

### Runtime secrets

Secrets should be referenced, not stored directly in a Russelfile:

```text
secret DATABASE_URL from "provider://project/api/database-url"
secret API_KEY from "provider://project/api/api-key"
```

Or, in a structured representation:

```toml
[secrets]
DATABASE_URL = { provider = "project/api", key = "database-url" }
API_KEY = { provider = "project/api", key = "api-key" }
```

The Russelfile should contain secret references and access policy, never plaintext secret values. Secret values must not be placed in:

- `flake.nix`;
- Nix derivations;
- `/nix/store`;
- command-line arguments;
- deployment logs;
- VM metadata;
- Git repositories.

### Possible secure injection design

The idea of injecting credentials through the network layer can work, but it should be defined as a separate authenticated secret-delivery channel rather than generic request rewriting.

At deployment time:

1. The control plane authenticates the deployment request and resolves the service's secret references.
2. The control plane creates a per-VM secret session with a short-lived identity.
3. The guest init process requests the secrets over a dedicated host/guest channel.
4. The init process sets the environment in memory and starts the application with `exec`.
5. The secret-delivery channel is closed or rotated after startup.

A dedicated virtio-vsock or virtio-serial channel is preferable to sending secrets through the application's ordinary TCP network. The channel should be:

- unique to the VM;
- authenticated to the service identity;
- unavailable to other VMs;
- excluded from application logs;
- short-lived and auditable;
- designed for rotation and revocation.

The host must still be treated as trusted: a host administrator can generally inspect a running VM's memory or processes. The goal is to prevent secrets from being shared between application tenants, other VMs, developers, build artifacts, and persistent files—not to claim protection from a compromised host kernel.

### Network-level credential injection

A proxy or service mesh can be useful for a narrower problem: injecting authentication into outbound HTTP requests or providing mTLS between services. For example, a proxy could add a service identity header or sign a request without exposing a long-lived API key to the application.

That is not a general replacement for environment variables:

- it does not work for arbitrary processes;
- it does not work for all TCP protocols;
- it cannot configure libraries that read credentials from environment variables;
- database protocols need protocol-aware handling;
- request rewriting can create difficult auditing and trust-boundary problems.

Therefore, Russel should use process-environment injection for ordinary runtime configuration and reserve network-layer injection for explicit service-to-service identity and authorization features.

### Suggested Russelfile model

The eventual format should make the distinction visible:

```text
env LOG_LEVEL=info
env APP_ENV=production

secret DATABASE_URL from "provider://project/api/database-url"

build-env NIX_CONFIG=...
dev-env RUST_LOG=debug
runtime-env PORT=${service.port}

exec ./bin/api
```

Build secrets require additional care. If private dependencies need credentials during a build, Russel should provide them ephemerally to the build process and ensure they do not appear in output paths, logs, or cached derivations.

## Proposed Commands

### `russel init` [planned]

Create a starter manifest:

```bash
russel init
```

This creates:

```text
Russelfile.toml
```

An optional flag can create both the manifest and a generated flake:

```bash
russel init --with-flake
```

### `russel develop` [planned]

Start the project's development shell:

```bash
russel develop
```

For a generated project, Russel creates or resolves the project's flake and internally invokes the equivalent of:

```bash
nix develop path:.
```

The command should use `exec`-style behavior so shell signals and exit codes work correctly.

If a custom `flake.nix` exists, Russel should use its `devShells.<system>.default` output.

### `russel build` [planned]

Build the deployment artifact without deploying a microVM:

```bash
russel build .
```

Internally, this is equivalent to:

```bash
nix build path:.#packages.x86_64-linux.default --no-link --print-out-paths
```

Russell substitutes the system placeholder with the actual target system, e.g., x86_64-linux, when constructing the command.

Russel should present a friendly result:

```text
Built api
Store path: /nix/store/...-api
```

This command lets users verify the application build independently from KVM, TAP networking, and the control plane.

### `russel deploy`

Deploy the application using the same resolver and artifact logic as `russel build`:

```bash
russel deploy . -p 8080:3000
```

The deployment path should not maintain a separate or subtly different Nix build implementation.

### `russel check` [planned]

Validate the project before building or deploying:

```bash
russel check
```

This command should validate the manifest, dependencies, flake outputs, binary configuration, and other project-level requirements.

## Generated Flake

For a simple Rust project, Russel could generate a flake conceptually like this:

```nix
# Pseudocode illustrating manifest deployment contract (per-system outputs)
{
  description = "Russel project";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # This schematic shows per-system artifacts; real flake would derive these with lib/genAttrs
  outputs = { self, nixpkgs }:
  {
    packages = {
      "x86_64-linux"."default" = nixpkgs.legacyPackages."x86_64-linux".rustPlatform.buildRustPackage {
        pname = "api"; version = "0.1.0"; src = ./.; cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ nixpkgs.legacyPackages."x86_64-linux".pkg-config ];
        buildInputs = [ nixpkgs.legacyPackages."x86_64-linux".openssl ];
      };
      "aarch64-linux"."default" = nixpkgs.legacyPackages."aarch64-linux".rustPlatform.buildRustPackage {
        pname = "api"; version = "0.1.0"; src = ./.; cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ nixpkgs.legacyPackages."aarch64-linux".pkg-config ];
        buildInputs = [ nixpkgs.legacyPackages."aarch64-linux".openssl ];
      };
    };

    devShells = {
      "x86_64-linux"."default" = nixpkgs.legacyPackages."x86_64-linux".mkShell {
        packages = [ nixpkgs.legacyPackages."x86_64-linux".rust-analyzer ];
      };
      "aarch64-linux"."default" = nixpkgs.legacyPackages."aarch64-linux".mkShell {
        packages = [ nixpkgs.legacyPackages."aarch64-linux".rust-analyzer ];
      };
    };

    # Additional manifest fields such as artifact source/target mappings, runtime checks,
    # and an exec/apps boot entrypoint would be derived from the Russelfile manifest.
  };
}
```

The generated flake should ideally be committed when the user runs:

```bash
russel init --with-flake
```

For deployment without a committed flake, Russel may generate a clearly marked flake in a temporary or generated location. Russel should avoid unexpectedly mutating a user's source tree during deployment unless that behavior is explicitly requested.

## Implementation Order

A practical implementation sequence is:

1. Extract the current Nix build resolution into shared code.
2. Add `russel build`.
3. Generate `devShells.<system>.default` alongside application packages.
4. Add `russel develop`.
5. Extend the `Russelfile` schema with `[dependencies]`.
6. Add `russel init`.
7. Add `russel check`.
8. Make deployment use the same shared build and project resolver.

The shared resolver should determine:

- whether a custom `flake.nix` exists;
- whether a generated flake is needed;
- which package output is the deployment artifact;
- which development shell should be entered;
- which dependencies need validation;
- which runtime metadata comes from `Russelfile.toml`.
