# Application Deployment Guide

This guide describes how to configure and deploy any application onto the Russel platform.

Deploying an application onto Russel requires two configuration files in the root of your repository:
1. **`Russelfile.toml`**: Configures microVM resources (CPU, Memory, Ports) and metadata.
2. **`flake.nix`**: Defines the Nix package build process, specifying dependencies and compiling your application into a reproducible Nix store path.

---

## 1. Russelfile.toml Configuration

The `Russelfile.toml` specifies the runtime requirements of your microVM. 

### Template & Reference

Create a file named `Russelfile.toml` in your project root with the following structure:

```toml
[service]
# The unique name of your service
name = "my-app"

# The source directory of the project (relative to the repository root)
source = "."

# The internal guest port the application listens on
port = 8080

# MicroVM Memory limit. Supports mb or mib suffixes.
# Minimum recommended is 256mb for Go/Rust, 512mb-1gb for Java/Node.
memory = "256mb"

# Optional: The name of the binary to execute inside the build package.
# Defaults to the value of `service.name` if not specified.
bin = "my-app"

# Optional: Automatic Docker-backed database provisioning on the host
[database.postgres]
enabled = false

[database.redis]
enabled = false
```

---

## 2. flake.nix Configuration

Russel builds your application using Nix. The control plane specifically builds the **`packages.x86_64-linux.default`** output from your `flake.nix`.

Here are three templates for common application stacks.

### Template A: Go Application
For Go applications, use `buildGoModule`. Nix handles vendoring and building automatically:

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
        
        # Set to null if using go.mod and dependencies are vendored, 
        # or specify the sha256 hash of the dependencies:
        # vendorHash = "sha256-...........................................";
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

## 3. Health Checks Requirement

Every application deployed on Russel must expose a HTTP health check endpoint:
- **Path**: `/health`
- **Port**: The port specified in `Russelfile.toml` (or read from the `PORT` environment variable).

Russel polls this endpoint immediately after booting the guest VM. The deployment is considered successful once the endpoint returns a `200 OK` status. If it fails to respond within 10 seconds, the deployment will report a warning.

---

## 4. Nix DX vs Docker DX

| Developer Experience | Docker | Russel (Nix + MicroVM) |
|----------------------|--------|------------------------|
| **Build Artifact**   | A multi-layer OCI container image | A reproducible Nix store path |
| **Caching**          | Docker Layer Cache (can be brittle) | Nix Store Cache (atomic dependency caching) |
| **Incremental Build**| Re-runs steps from changed layers | Near-instantaneous (re-evaluates only changed paths) |
| **Isolation**        | OS-level namespaces (Shared kernel) | Hardware-level KVM virtualization (Independent kernel) |
| **Boot Overhead**    | Container namespaces startup (< 1s) | MicroVM boot and kernel load (< 2s) |
