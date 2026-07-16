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

**Recommended convention**: Expose a `GET /health` endpoint on your app's `PORT`. While Russel doesn't currently check HTTP status, this endpoint will be used by future Traefik health checks and is a good practice for any service.

---

## 4. Nix DX vs Docker DX

| Developer Experience | Docker | Russel (Nix + MicroVM) |
|----------------------|--------|------------------------|
| **Build Artifact**   | A multi-layer OCI container image | A reproducible Nix store path |
| **Caching**          | Docker Layer Cache (can be brittle) | Nix Store Cache (atomic dependency caching) |
| **Incremental Build**| Re-runs steps from changed layers | Near-instantaneous (re-evaluates only changed paths) |
| **Isolation**        | OS-level namespaces (Shared kernel) | Hardware-level KVM virtualization (Independent kernel) |
| **Boot Overhead**    | Container namespaces startup (< 1s) | MicroVM boot and kernel load (< 2s) |
