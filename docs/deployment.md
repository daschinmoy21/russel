# Application Deployment Guide

This guide describes how to configure and deploy any application onto the Russel platform.

## Zero-Config Deployment (Automatic flake.nix)

By default, Russel features **Zero-Config deployment**. If your repository does not contain a `flake.nix`, the Russel control plane will automatically detect your project type and generate a default `flake.nix` for you:

- **Rust projects**: Detected by `Cargo.toml`. Auto-generates a flake using `Cargo.lock` to fetch and compile dependencies.
- **Go projects**: Detected by `go.mod`. Auto-generates a Go build module.
- **Static files / scripts / others**: Auto-generates a lightweight static server powered by Python's `http.server` to host directory files.

If you are a power user or need custom native dependencies/system libraries, you can write your own `flake.nix` in the root of your repository to gain full control over the build environment.

---

## 1. Russelfile.toml Configuration

The `Russelfile.toml` specifies deploy requirements for your service (microVM or container).

### Template & Reference

Create a file named `Russelfile.toml` in your project root with the following structure:

```toml
[service]
# The unique name of your service
name = "my-app"

# The source directory of the project (reserved for future multi-service repos)
source = "."

# The internal application port the process listens on
port = 8080

# Memory limit (microVM RAM / container --memory). Supports mb or mib suffixes.
# Minimum recommended is 256mb for Go/Rust, 512mb-1gb for Java/Node.
memory = "256mb"

# Optional: The name of the binary to execute inside the build package.
# Defaults to the value of `service.name` if not specified.
bin = "my-app"

# Optional: "microvm" (default) or "container" (rootless Podman).
# This is the source of truth. CLI --runtime must match if provided.
type = "microvm"

# Optional: User-defined environment variables injected at deploy time.
# Keys must start with a letter or underscore, contain only [A-Za-z0-9_].
# Reserved keys (PORT, VM_IP, HOST_IP, APP) are rejected.
# Plain values or secret://NAME refs (resolved from host secrets store).
# Max 64 keys, 4096 bytes each.
[service.env]
LOG_LEVEL = "info"
FEATURE_X = "1"
# DB_PASSWORD = "secret://DB_PASSWORD"

# Optional: Database provisioning (planned — current: placeholder)
[database.postgres]
enabled = false

[database.redis]
enabled = false
```

---

## 2. flake.nix Configuration

Russel builds your application using Nix. The control plane builds the **`packages.<system>.default`** output (using your host's detected Nix system), falling back to `#defaultPackage.<system>` if that fails. If no `flake.nix` exists, Russel auto-generates one.

Here are three templates for common application stacks.

### Template A: Go Application

For Go applications, use `buildGoModule`. The `system` variable should match your host (e.g. `x86_64-linux`, `aarch64-linux`):

```nix
{
  description = "Go Application on Russel";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs   = import nixpkgs { inherit system; };
    in {
      packages.${system}.default = pkgs.buildGoModule {
        pname = "my-go-app";
        version = "0.1.0";
        src = ./.;
        vendorHash = null;
      };
    };
}
```

### Template B: Rust Application

For Rust applications, use `rustPlatform.buildRustPackage`:

```nix
{
  description = "Rust Application on Russel";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs   = import nixpkgs { inherit system; };
    in {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "my-rust-app";
        version = "0.1.0";
        src = ./.;
        
        # Specify Cargo.lock hash or set cargoHash
        cargoHash = "sha256-...........................................";
      };
    };
}
```

### Template C: Precompiled Binaries & Shell Wrappers

If your app is already distributed as a precompiled binary in Nixpkgs (e.g., Python scripts, Caddy, Node, Jenkins, Filebrowser), you can use a shell script wrapper. This avoids compilation entirely and fetches the package from the Nix cache:

```nix
{
  description = "Wrapper Deployment on Russel";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs   = import nixpkgs { inherit system; };
    in {
      packages.${system}.default = pkgs.writeShellScriptBin "my-wrapper-app" ''
        # Run pre-execution setups here (e.g., directories creation)
        mkdir -p /tmp/data

        # Directly execute the nixpkgs package.
        # Ensure it respects the dynamic PORT env var injected by Russel.
        exec ${pkgs.caddy}/bin/caddy run \
          --config /tmp/Caddyfile \
          --adapter caddyfile
      '';
    };
}
```

---

## 3. Readiness Check

Russel verifies deployment readiness by TCP-connecting to the guest port for up to 10 seconds. If the port doesn't respond, Russel attempts rollback to the previous working VM (only when a prior VM backup exists). For an initial deployment with no backup, readiness failure cleans up the attempted VM and returns failure without restoring a prior VM.

**Recommended convention**: Expose a `GET /health` endpoint on your app's `PORT`. While Russel doesn't currently check HTTP status, this endpoint will be used by Traefik health checks and is a good practice for any service.

## 4. Accessing Your Service

### Via Traefik (recommended)

When Traefik is running as the ingress gateway, Russel writes dynamic configuration automatically. Access your service at:

```text
http://<service_id>.russel.local
```

This requires:
1. Traefik running with the file provider pointed at `/var/lib/russel/traefik/dynamic` (see [docs/traefik.md](traefik.md)).
2. DNS or `/etc/hosts` entry mapping `*.russel.local` to your host's IP:
   ```text
   127.0.0.1  api.russel.local  demo.russel.local
   ```

### Via Direct Port

`-p HOST:GUEST` publishes a host port. Access directly at `http://localhost:<HOST>`.

---

## 5. Security & Validation

### Repository URLs

- **Local deploys** require **absolute** paths. The CLI canonicalizes relative paths before sending; the control plane rejects relative paths, `file://` URLs, and `..` traversal components.
- **Remote deploys** accept `https://`, `http://`, `ssh://`, and `git@host:path` only. Link-local metadata hosts (`169.254.169.254`) are blocked. URLs starting with `-` (git option injection) are rejected.

### Config Path (`--config`)

The config file path must be **relative** to the repository root. The control plane opens it safely:

- Path components are walked with `openat` + `O_NOFOLLOW` — symlinks at any level (including the leaf) are rejected.
- Opened files must be regular files (not directories or devices).
- A **1 MiB size cap** is enforced (both via `fstat` pre-check and a bounded `read`).

### Binary Name

The binary name (from `Russelfile.toml` `bin` or `name`) must match the safe charset `[A-Za-z0-9._+-]` (max 256 characters). It is injected into the guest via a shell-quoted `deploy.env` file — single quotes with embedded `'` escaped as `'\''`.

## 6. Environment Variables

Deploy-time environment variables can be set via three mechanisms, merged in order (later wins):

1. **`[service.env]` in Russelfile.toml** — project defaults, checked into version control.
2. **`--env-file PATH`** — a file with `KEY=VALUE` lines (`#` comments, blank lines skipped).
3. **`--env KEY=VALUE`** — repeatable CLI flag, highest priority.

### Validation

- Keys must match `^[A-Za-z_][A-Za-z0-9_]*$` (start with letter or underscore, alphanumeric + underscore).
- Reserved keys **`PORT`**, **`VM_IP`**, **`HOST_IP`**, **`APP`** are rejected (managed by Russel).
- Values must not contain NUL bytes. Maximum value length is 4096 bytes.
- Maximum 64 keys total after merging all sources.

### Injection

- **microVM:** Custom env vars are appended to `/config/deploy.env` (shell-quoted) and exported before the app starts.
- **Container:** Custom env vars are passed via `podman run -e` after the managed `PORT` variable.

### Secrets

Store secret values on the control plane host (not in the Russelfile). Values live under
`/var/lib/russel/secrets/` (mode `0600`). Reference them in env maps as `secret://NAME`;
the control plane resolves them at deploy time.

```bash
printf '%s' "$VAL" | russel-cli secrets set NAME
russel-cli secrets list
russel-cli secrets delete NAME
```

```toml
[service.env]
DATABASE_URL = "secret://DATABASE_URL"
```

## 7. Nix DX vs Docker DX

| Developer Experience | Docker | Russel (Nix + MicroVM) |
|----------------------|--------|------------------------|
| **Build Artifact**   | A multi-layer OCI container image | A reproducible Nix store path |
| **Caching**          | Docker Layer Cache (can be brittle) | Nix Store Cache (atomic dependency caching) |
| **Incremental Build**| Re-runs steps from changed layers | Near-instantaneous (re-evaluates only changed paths) |
| **Isolation**        | OS-level namespaces (Shared kernel) | Hardware-level KVM virtualization (Independent kernel) |
| **Boot Overhead**    | Container namespaces startup (< 1s) | MicroVM boot and kernel load (< 2s) |
