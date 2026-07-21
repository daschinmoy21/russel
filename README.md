# Russel

A self-hosted platform for deploying Nix-built services as **microVMs** ([Cloud Hypervisor](https://www.cloudhypervisor.org/)) or **Russel containers** (rootless Podman `--rootfs`).

## Architecture

Russel is split into three crates:

| Crate | Purpose |
|-------|---------|
| `russel-core` | Shared types: `Russelfile` config, API request/response types |
| `russel-cli` | CLI client that talks to the control plane over HTTP |
| `russel-ctrl` | Control plane (Axum HTTP API) that orchestrates builds, **microVMs** and **Russel containers**, and networking. MicroVMs use Cloud Hypervisor; containers use rootless Podman `--rootfs`. |

### Deployment Flow

1. **Resolve** — Clone (or use local) repo and parse `Russelfile.toml` (including `service.type`).
2. **Build** — Auto-generate `flake.nix` if missing (Rust/Go/static detection), then run `nix build` to produce a store path.
3. **Branch on runtime**
   - **microvm (default):** minimal initramfs → TAP + socat + virtiofsd → Cloud Hypervisor → guest runs app from virtiofs `/nix/store`.
   - **container:** prepare Docker-like rootfs → rootless Podman `--rootfs` + `/nix/store:ro` bind → publish `-p HOST:GUEST`.
4. **Ready** — TCP readiness on guest (microVM) or published host port (container); metadata written under `/var/lib/russel/<id>/`. Register with the `Ingress` trait (default: `TraefikFileIngress` writes Traefik dynamic config) so the reverse proxy can route traffic to the new backend.

## Quick Start

```bash
# Enter the dev shell (Rust toolchain + dependencies)
nix develop

# Build everything
cargo build

# Optional: set an API token (if set on ctrl, set the same value here)
export RUSSEL_API_TOKEN=your-secret-token

# Run the control plane
./target/debug/russel-ctrl

# In another terminal, deploy the example app
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id test-api

# List running VMs
./target/debug/russel-cli vms

# Stop or destroy a VM
./target/debug/russel-cli stop test-api
./target/debug/russel-cli destroy test-api
```

## Build Command

There is currently no `russel build` command. Russel builds an application automatically during `russel deploy`; the control plane invokes Nix with the repository's `packages.<system>.default` output and returns the resulting store path internally. Use `nix build` directly only when you want to inspect or run an application artifact without deploying a microVM.

For example:

```bash
nix build path:examples/basic-http
```

A future `russel build` wrapper could provide a friendlier, project-aware interface, but it should not be documented or relied on until the CLI implements it.

## API Endpoints

The control plane listens on `127.0.0.1:7878` by default (override with `RUSSEL_CTRL_ADDR`). All endpoints use HTTP/JSON; `/deploy` returns an NDJSON event stream with progress and timing.

### Authentication

When `RUSSEL_API_TOKEN` is set on the control plane, **every** API route requires an `Authorization: Bearer <token>` header. The CLI reads the same environment variable and sends it automatically. On loopback without a token the control plane runs in dev mode (with a warning); binding to a non-loopback address **requires** `RUSSEL_API_TOKEN` or the control plane refuses to start. For production deployments, always set `RUSSEL_API_TOKEN` and bind to the internal interface where your reverse proxy lives.

### Bind Policy

| Scenario | Behaviour |
|----------|-----------|
| Loopback (`127.0.0.1:…`) + no token | Dev mode (warn, no auth) |
| Loopback + `RUSSEL_API_TOKEN` set | Bearer auth required on all routes |
| Non-loopback + no token | **Refuses to start** |
| Non-loopback + `RUSSEL_API_TOKEN` set | Bearer auth required on all routes |

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/deploy` | Deploy or re-deploy a service (returns NDJSON stream) |
| `GET`  | `/status` | Get deployment status for all services (when `service_id` omitted) |
| `GET`  | `/logs`   | Get logs for all services (when `service_id` omitted) |
| `GET`  | `/vm/{service_id}/status` | Get deployment status for a service |
| `GET`  | `/vm/{service_id}/logs`   | Get logs for a service |
| `GET`  | `/vms`                     | List registered services |
| `POST` | `/vm/{service_id}/stop`    | Stop a service (microVM or container) |
| `POST` | `/vm/{service_id}/update`  | Redeploy from recorded/overridden source (returns NDJSON stream) |
| `DELETE`| `/vm/{service_id}`         | Destroy a service and clean up resources |

## CLI Commands

```bash
russel deploy <repo-url> [-p HOST:GUEST] [--config PATH] [--vm-id ID] [--runtime microvm|container] [--env KEY=VALUE...] [--env-file PATH] [-- <podman-run-args...>]
russel status [<service_id>]
russel logs [<service_id>]
russel vms
russel stop <service_id>
russel destroy <service_id>
russel update <service_id> [--repo REPO] [--config PATH]
```

- **`--env KEY=VALUE`** (repeatable): Set an environment variable for the deployed service. Overrides `[service.env]` from the Russelfile.
- **`--env-file PATH`**: Load `KEY=VALUE` pairs from a file (`#` comments, blank lines skipped). Merged with `[service.env]` and `--env` (later wins).
- Reserved keys (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are rejected for user-defined env vars.

- **`--runtime`** is **not** an override. If set, it must match `service.type` in the Russelfile (or the default `microvm` when omitted). Mismatch → hard error.
- Ports today: **`-p HOST:GUEST`** (published binds). Traefik is the primary HTTP gateway; `-p` is optional for HTTP services.

### Repo URLs

- **Local deploys** must use **absolute** paths (the CLI canonicalizes relative paths before sending). The control plane rejects relative paths, `file://` URLs, and `..` components.
- **Remote deploys** accept `https://`, `http://`, `ssh://`, and `git@host:path` only. Link-local metadata hosts (`169.254.169.254`) are blocked.

### Config Path & Bin Name

- `--config` must be a **relative** path under the repository root. The control plane opens it via `openat` with `O_NOFOLLOW` (symlinks rejected) and enforces a 1 MiB size cap.
- The binary name (from `Russelfile.toml` `bin` or `name`) must match `[A-Za-z0-9._+-]` (max 256 chars). It is injected into the guest via a shell-quoted `deploy.env` file.

### Redeploy / update

Redeploying an existing service kills and waits for old processes before reusing ports. If a new deploy fails after a prior successful deployment, Russel attempts automatic **rollback** to the previous running service. A successful rollback reports status `rolled_back`; the CLI exit code is non-zero so CI pipelines can detect the failure.

**`russel update <id>`** re-applies desired state from the `repo_url` / `config_path` recorded in metadata at the last successful deploy (override with `--repo` / `--config`).

### Health

The control plane probes `127.0.0.1:<host_port>` every `RUSSEL_HEALTH_INTERVAL_SECS` (default 30). After three consecutive failures the service is marked failed. Set `RUSSEL_HEALTH_RESTART=1` to auto-redeploy from the recorded source.

## Project Requirements

A repository you want to deploy needs a `Russelfile.toml` in its root. If no `flake.nix` is present, Russel auto-generates one based on project type (Rust → Cargo.toml, Go → go.mod, else → static server). See the [Application Deployment Guide](docs/deployment.md), [Flake Auto-Generation docs](docs/auto-generation.md), and [Examples](docs/examples.md) for details.

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"    # optional — defaults to name
type = "microvm"  # optional: "microvm" (default) or "container"

[service.env]    # optional — user-defined environment variables
LOG_LEVEL = "info"
FEATURE_X = "1"
```

### Runtimes

| `service.type` | Isolation | Host needs |
|----------------|-----------|------------|
| `microvm` (default) | KVM / Cloud Hypervisor | KVM, TAP, virtiofsd, socat |
| `container` | Rootless Podman `--rootfs` | Rootless Podman |

**Russel containers** prepare a Docker-like rootfs under `/var/lib/russel/<id>/rootfs` (with `/tmp`, `/var`, bash, curl for debugging) and bind-mount the host `/nix/store` read-only. Do **not** pass a bare Nix package path as `--rootfs` yourself — use `russel deploy`.

```bash
# MicroVM (default)
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id api

# Container — Russelfile must have type = "container", and --runtime must match if passed
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id api \
  --runtime container

# Container with extra podman run flags (after --)
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --runtime container \
  -- -v /data:/data:ro --network bridge
```

Russel checks readiness by TCP-connecting to the published host port (container) or guest port via TAP (microVM). For application-level health monitoring, expose a `/health` endpoint on `PORT` as a convention (Traefik is already the ingress).

## Networking Model

- **MicroVM:** Each VM gets a deterministic `/30` subnet from `service_id` (FNV-1a), host TAP `rsl-<hex>`, `socat` host→guest port forward.
- **Container:** Rootless Podman publishes `-p HOST:GUEST` (from CLI `-p` / allocator).

### Port Publishing (today)

`-p HOST:GUEST` publishes a host port via `socat` (microVM) or Podman port mapping (container). Both paths go through the port allocator so host ports never collide across services.

### Traefik Gateway

Traefik is the **primary HTTP ingress gateway**. Russel writes dynamic configuration files into `/var/lib/russel/traefik/dynamic/` (override with `RUSSEL_TRAEFIK_DYNAMIC_DIR`). Each deployed service gets a Host rule: `<service_id>.<domain>` (domain defaults to `russel.local`, override with `RUSSEL_TRAEFIK_DOMAIN`).

```yaml
# Static Traefik config snippet (traefik.yml)
entryPoints:
  web:
    address: ":80"
providers:
  file:
    directory: /var/lib/russel/traefik/dynamic
    watch: true
```

With Traefik running, access your service at `http://<service_id>.russel.local` (requires DNS or `/etc/hosts` entry pointing to Traefik's IP). The host port is auto-allocated as a private backend — no user `-p` required for normal HTTP apps. `-p` remains available as an escape hatch for direct host publishing.

On destroy/stop, Russel removes the dynamic config file so Traefik stops routing to the dead backend. Redeploy re-registers with the new backend port.

## Lifecycle

### Redeploy & Rollback

On redeploy, Russel stops the old workload (kill + wait up to 5 s, then force-kill) before allocating ports for the new one. If the new deploy fails and a previous deployment existed, Russel attempts automatic rollback: the old service directory and metadata are restored, the previous VM or container is re-spawned, and a readiness check confirms the rollback before reporting `rolled_back`.

### Control Plane Shutdown

Stopping the control plane (`SIGINT`/`SIGTERM`) **does not** destroy running workloads — they are detached and keep running. On the next startup, the control plane removes only **orphan** TAP interfaces that have no corresponding live service directory (`/var/lib/russel/<id>/metadata.json`). Host iptables rules (Docker, VPN, admin) are never touched.

### Container Lifecycle

Containers are started only after podman arguments are fully validated and rootless mode is confirmed. For redeploys, the old container is stopped only after validation succeeds (fail-closed). Readiness is confirmed by TCP-polling the published host port.

## Boot & Network Timing Optimization (Under 2s Boot)

To optimize boot time from 40s+ to under 2s, we transitioned from a heavy guest-side NixOS systemd VM to a direct microVM boot model:

- **Minimal Initramfs**: Dropped the Nix OS closure from guest RAM. The initramfs is only 2-3MB, consisting of BusyBox and necessary drivers.
- **Virtiofs /nix/store Share**: The host `/nix/store` is mounted directly inside the guest using `virtiofsd` and the `virtiofs` filesystem driver, allowing instant access to the application closure without packaging it in the initrd.
- **Kernel Module Bootstrapping**: Since standard nixpkgs kernels compile VirtIO networking (`virtio_net`) and VirtIO filesystem (`virtiofs`) as modules (`=m`), our `/init` script dynamically extracts and loads the VirtIO dependency chain (e.g. `virtio_ring`, `virtio.ko`, `virtio_pci_*`, `virtio_net`, `fuse`, `virtiofs`) via `insmod` before mounting the store or bringing up `eth0`.
- **Direct App Execution**: Bypasses systemd in the guest. The `/init` script executes the application binary directly, reducing guest-side lifecycle overhead to virtually zero.

## System Requirements

Russel currently runs the control plane on **Linux only**. The `russel-ctrl` binary needs the following tools available on `PATH`:

| Dependency | Used for | Required when |
|------------|----------|----------------|
| Nix with flakes enabled | Building application closures, the kernel, BusyBox, and kernel modules | Always |
| `cloud-hypervisor` | Booting the microVM | MicroVM deploy |
| `virtiofsd` | Sharing the host `/nix/store` with the guest | MicroVM deploy |
| `socat` | Forwarding the host port to the guest | MicroVM deploy |
| `iproute2` (`ip`) | Creating and configuring TAP interfaces | MicroVM / ctrl networking |
| `iptables` | Cleaning and configuring host NAT/forwarding rules | Starting `russel-ctrl` / deploying |
| `podman` (rootless) | Russel containers via `--rootfs` | Container deploy |
| `git` | Cloning remote application repositories | Deploying a remote repository |

The host also needs:

- For microVMs: a working KVM setup (`/dev/kvm`), permission to create TAP devices and change networking/iptables rules.
- For containers: **rootless Podman** configured and working (`podman info` reports rootless). Russel does not use rootful Podman.
- A writable `/var/lib/russel` directory for service state, logs, rootfs, and metadata.

After Nix is installed, the repository flake provides Rust, `rust-analyzer`, Cloud Hypervisor, Podman (Linux), and related tools:

```bash
nix develop
```

On a non-Nix host, install the equivalent packages with your distribution's package manager. The exact package names vary; on Debian/Ubuntu they are typically `build-essential`, `pkg-config`, `libssl-dev`, `nix`, `cloud-hypervisor`, `virtiofsd`, `socat`, `iproute2`, `iptables`, `podman`, and `git`.

## Repo Layout

```
.
├── crates/
│   ├── cli/          # russel-cli
│   ├── core/         # shared types & config
│   └── ctrl/         # control plane (main logic)
├── examples/
│   ├── basic-http/   # Go app with flake.nix + /health
│   ├── filebrowser/  # Nix wrapper around pkgs.filebrowser
│   └── static-test/  # Static HTML served via Python http.server
├── nix/
│   ├── microvm/      # legacy/reference configs; runtime boots Cloud Hypervisor directly
│   └── modules/      # host NixOS modules
├── docs/
│   ├── architecture.md   # Control plane internals
│   ├── auto-generation.md # Flake auto-detection
│   ├── deployment.md      # App packaging guide
│   └── examples.md        # Example projects (microVM + container)
├── flake.nix         # development shell
└── README.md
```

## Benchmark (2026-07-12)

| Metric | Value |
|--------|-------|
| Clean build (debug) | 12.3s |
| Release build | 24.4s |
| Incremental build | 0.8s |
| Tests | 43 passing, 0.7s |
| Binary size (cli) | 6.7MB |
| Binary size (ctrl) | 4.5MB |
| Rust LOC | 3,994 (16 files) |
| Direct deps | 332 |
| Dockerfiles | 3 (examples/basic-http, static-test, filebrowser) |

### Russel vs Container: Application Boot Race

The benchmark races Russel (microVM via cloud-hypervisor) against the detected
container runtime (podman or docker) for each example application — end to end:
build → spawn → first HTTP response. The table reports end-to-end times.
A second "Spawn-to-Ready" table (excluding build time) is printed below it.

| Example | Russel (deploy+curl) | Docker/Podman (build+run+curl) | Winner |
|---------|---------------------|--------------------------------|--------|
| basic-http | 3.9s | 8.6s | Russel |
| static-test | 1.7s | 2.7s | Russel |
| filebrowser | 1.9s | 10.1s | Russel |

Times are end-to-end: build + spawn + first HTTP 200. The runtime label
(podman/docker) is auto-detected. Russel phase breakdown
(resolve, nix build, initramfs, network, boot, ready) is printed after the
race by `./bench.sh`.

Run `./bench.sh [--cold|--warm]` to reproduce. Requires Rust toolchain, nix,
and optionally podman/docker for the comparison.

### Container Boot Comparison (legacy, for reference)

| Example | Container ready | Build time |
|---------|----------------|------------|
| basic-http | 176ms | 19.1s |
| static-test | 519ms | 1.2s |
| filebrowser | 292ms | 8.3s |

Container startup (spawn→ready) is faster than microVM boot (which includes
kernel init, initramfs extraction, and module loading), but microVMs provide
stronger isolation via hardware virtualization. Container resources are capped
to `--memory=256m` to match the microVM memory limit. Container run time
includes port-resolution polling (`sleep 0.5` up to 5×) while Russel's port is
pre-allocated.

> **Fair comparison note:** The main End-to-End table conflates build and spawn
> into one number, which favors Russel (its Nix cache persists; Docker images
> were previously destroyed each run via `rmi`). The Spawn-to-Ready table
> (printed by `./bench.sh`) strips build time. `--warm` (default) now keeps both
> caches warm; `--cold` forces cold builds on both sides.

### Known Limitations

- **Subnet collision detection** — 16-bit FNV-1a space, <2% collision at 50 services. Add when scale demands it.
- **virtiofsd --readonly** — `/nix/store` is read-only from the guest (added via `--readonly` flag). Remove only when a workflow needs guest-side store mutations.
- **Database/Health stubs** — documented placeholders, fully functional via direct socat access.
- **No integration/e2e tests** — requires KVM + root. Marked `#[ignore]` candidate for a future e2e crate.
- **Auth optional on loopback** — dev mode warns but does not enforce. Production should always set `RUSSEL_API_TOKEN`.

## License

MIT
