# Examples

Three example projects demonstrate Russel's deployment flow, from simple static sites to
Go binaries and Nix-wrapped third-party packages.

## Prerequisites

These examples use Russel's current Linux microVM backend. Before deploying any example:

1. Install Nix with flakes enabled, then enter the repository development environment. The shell provides Rust, Cloud Hypervisor, `virtiofsd`, `socat`, `iproute2`, `iptables`, and `git`:

   ```bash
   nix develop
   ```

   If you are not using Nix for the development shell, install equivalent packages with your system package manager.
2. Use a Linux host with KVM enabled and access to `/dev/kvm`.
3. Ensure the account running `russel-ctrl` can create TAP devices and change networking/iptables rules. Run it with the required root privileges or equivalent narrowly scoped capabilities.
4. Build the workspace once:

   ```bash
   cargo build
   ```

5. Start the control plane in the first terminal. It listens on `127.0.0.1:7878` by default.
   On loopback, auth is optional (dev mode). For production, set `RUSSEL_API_TOKEN` and
   pass the same token to the CLI:

   ```bash
   export RUSSEL_API_TOKEN=your-secret-token  # optional on loopback; required for non-loopback
   ./target/debug/russel-ctrl
   ```

   Keep it running while deploying from a second terminal. The examples below assume commands are run from the repository root.

`russel-ctrl` invokes Nix internally to build the application, kernel, BusyBox, and kernel modules. You do not need to run `nix build` before a microVM deployment.

---

## 1. basic-http — Go HTTP Server

A minimal Go HTTP server with embedded static assets and a `/health` endpoint.

| Field           | Value                |
| --------------- | -------------------- |
| Language        | Go (stdlib only)     |
| Guest port      | 3000                 |
| Binary          | `basic-http`         |
| Health endpoint | `GET /health` → `ok` |
| Config          | `examples/basic-http/Russelfile.toml` |

### MicroVM Deploy

```bash
# Start the control plane (first terminal)
./target/debug/russel-ctrl

# Deploy (second terminal)
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id basic
```

```
  russel deploy
  /home/.../examples/basic-http

     resolve  · Resolving source & Russelfile
      build  · Building Nix package + ensuring kernel/busybox/modules
     create  · Building minimal initramfs
      start  · Setting up network + booting VM
      ready  · Waiting for VM service to be reachable

  ✓ Deployed in 3.2s  (server: 2.8s)

  vm-id      basic
  status     deployed
  port       localhost:8080 → guest:3000
  vm-ip      10.49.158.2  (direct: curl 10.49.158.2:3000)

  Test:  curl -I http://127.0.0.1:8080/
```

```bash
# Verify
curl http://localhost:8080/health
# → ok

curl http://localhost:8080/
# → <!DOCTYPE html>...

# Check status
./target/debug/russel-cli status
# → service_id=basic status=deployed vm_state=running

# Stop and clean up
./target/debug/russel-cli stop basic
./target/debug/russel-cli destroy basic
```

### Container Deploy (Russel + rootless Podman)

Set `type = "container"` in `Russelfile.toml` (source of truth). Optionally pass
`--runtime container` — it must **match** the file or deploy errors.

Russel prepares a Docker-like rootfs (with `/tmp`, `/var`, bash, curl) under
`/var/lib/russel/<id>/rootfs` and starts rootless Podman with `--rootfs` plus a
read-only bind of host `/nix/store`. **Do not** use a bare package path as rootfs.

```toml
# examples/basic-http/Russelfile.toml
[service]
name = "api"
# ...
type = "container"
```

```bash
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id basic \
  --runtime container

curl http://localhost:8080/health
# → ok

./target/debug/russel-cli status basic   # shows runtime=container
./target/debug/russel-cli stop basic
./target/debug/russel-cli destroy basic
```

> **Key insight**: The Nix store path is still the artifact. MicroVM (virtiofs)
> and container (bind-mounted store into a prepared rootfs) share the same build
> output — no OCI image rebuild.

---

## 2. filebrowser — Nix-Wrapped Third-Party Binary

Demonstrates deploying a pre-built binary from nixpkgs (`pkgs.filebrowser`) using a
`writeShellScriptBin` wrapper. No compilation — the package is fetched from the Nix
cache.

| Field      | Value                        |
| ---------- | ---------------------------- |
| Language   | Shell wrapper around Go binary |
| Guest port | 8080                         |
| Binary     | `filebrowser`                |
| Config     | `examples/filebrowser/Russelfile.toml` |

### MicroVM Deploy

```bash
./target/debug/russel-cli deploy examples/filebrowser -p 8081:8080 --vm-id files
```

```bash
curl http://localhost:8081/
# → Welcome to Russel File Manager inside your MicroVM!
```

### Container Deploy (Russel)

```bash
# Russelfile: type = "container"
./target/debug/russel-cli deploy examples/filebrowser -p 8081:8080 --vm-id files \
  --runtime container
```

---

## 3. static-test — Zero-Config Static Site

A directory of static HTML files deployed without any custom `flake.nix` — Russel auto-detects
the project as a fallback static site and generates a Python `http.server` wrapper.

| Field      | Value                       |
| ---------- | --------------------------- |
| Language   | Auto-detected (fallback)    |
| Guest port | 8000                        |
| Binary     | `app`                       |
| Config     | `examples/static-test/Russelfile.toml` |

### MicroVM Deploy

```bash
./target/debug/russel-cli deploy examples/static-test -p 8082:8000 --vm-id static
```

```bash
curl http://localhost:8082/
# → <!DOCTYPE html>...

./target/debug/russel-cli stop static
./target/debug/russel-cli destroy static
```

### Container Deploy (Russel)

```bash
# Russelfile: type = "container"
./target/debug/russel-cli deploy examples/static-test -p 8082:8000 --vm-id static \
  --runtime container
```

---

## Direct Nix Build (No Russel)

Each example can be built and run directly with Nix — useful for development and debugging:

```bash
# basic-http
nix build path:examples/basic-http
PORT=3000 ./result/bin/basic-http

# filebrowser
nix build path:examples/filebrowser
PORT=8080 ./result/bin/filebrowser

# static-test
nix build path:examples/static-test
PORT=8000 ./result/bin/app
```

---

## Deployment Comparison

| Step            | MicroVM (Cloud Hypervisor)                         | Container (Podman)                    |
| --------------- | -------------------------------------------------- | ------------------------------------- |
| Build           | `nix build` (same)                                 | `nix build` (same)                    |
| Isolation       | Hardware-level KVM (separate kernel)               | OS-level namespaces (shared kernel)   |
| Store sharing   | `virtiofsd` + `virtiofs` mount in guest            | Prepared rootfs + `/nix/store:ro` bind |
| Cold start      | ~1-3s (kernel + initramfs + app)                   | Typically faster (no guest kernel)    |
| Privilege       | Often privileged ctrl (TAP/KVM)                    | **Rootless** Podman only              |
| Use case        | Untrusted workloads, strict isolation              | Trusted / lighter isolation           |

Both paths use the **same Nix store path** as the build artifact. Choose runtime with
`service.type` in Russelfile (`microvm` default, or `container`). Changing type and
redeploying is supported today with downtime; **zero-downtime swap is planned later**
(see deferred issues).
