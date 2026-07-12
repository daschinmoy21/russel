# Russel

A self-hosted platform for deploying services as microVMs using [Cloud Hypervisor](https://www.cloudhypervisor.org/) and Nix for reproducible builds.

## Architecture

Russel is split into three crates:

| Crate | Purpose |
|-------|---------|
| `russel-core` | Shared types: `Russelfile` config, API request/response types |
| `russel-cli` | CLI client that talks to the control plane over HTTP |
| `russel-ctrl` | Control plane (Axum HTTP API) that orchestrates builds, microVMs, and networking. Boots Cloud Hypervisor directly (no systemd, no NixOS guest). |

### Deployment Flow

1. **Resolve** — Clone (or use local) repo and parse `Russelfile.toml`
2. **Build** — Auto-generate `flake.nix` if missing (Rust/Go/static detection), then run `nix build` to produce a store path
3. **Create Initramfs** — Build a minimal CPIO initramfs containing only BusyBox and required VirtIO kernel modules (2-3MB).
4. **Network + Start VM** — Create the TAP interface, assign a deterministic `/30` subnet (`10.x.y.1` host, `10.x.y.2` guest), spawn `socat` for port forwarding, spawn `virtiofsd` to share `/nix/store` via a UNIX socket, then boot `cloud-hypervisor` directly with the stock kernel and minimal initramfs.
5. **Kernel Init** — Guest kernel boots. The custom `/init` script decompresses and loads VirtIO modules via `insmod`, mounts `/nix/store` via `virtiofs`, configures the guest IP, and directly executes the application binary (no systemd).
6. **Ready** — TCP-connects to the guest port until it responds, confirming deployment success.

## Quick Start

```bash
# Enter the dev shell (Rust toolchain + dependencies)
nix develop

# Build everything
cargo build

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

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/deploy` | Deploy or re-deploy a service (returns NDJSON stream) |
| `GET`  | `/vm/{service_id}/status` | Get deployment status for a service |
| `GET`  | `/vm/{service_id}/logs`   | Get logs for a service |
| `GET`  | `/vms`                     | List registered services |
| `POST` | `/vm/{service_id}/stop`    | Stop a microVM |
| `DELETE`| `/vm/{service_id}`         | Destroy a microVM and clean up resources |

## CLI Commands

```bash
russel deploy <repo-url> [-p HOST:GUEST] [--config PATH] [--vm-id ID]
russel status <service_id>
russel logs <service_id>
russel vms
russel stop <service_id>
russel destroy <service_id>
```

## Project Requirements

A repository you want to deploy needs a `Russelfile.toml` in its root. If no `flake.nix` is present, Russel auto-generates one based on project type (Rust → Cargo.toml, Go → go.mod, else → static server). See the [Application Deployment Guide](docs/deployment.md), [Flake Auto-Generation docs](docs/auto-generation.md), and [Examples](docs/examples.md) for details.

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"    # optional — defaults to name
```

Russel checks readiness by TCP-connecting to the guest port. For application-level health monitoring (planned for Traefik integration), expose a `/health` endpoint on `PORT` as a convention.

## Networking Model

- Each VM gets a deterministic `/30` subnet derived from its `service_id` via FNV-1a hash (e.g. `10.x.y.1` host, `10.x.y.2` guest)
- Host-side TAP interface (`vm-<id>`) is created and configured by `russel-ctrl` via `ip tuntap`
- `socat` forwards `0.0.0.0:<host_port>` → `<vm_ip>:<guest_port>`
- Traefik integration is planned for multi-node ingress (current: placeholder)

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
| `cloud-hypervisor` | Booting the microVM | Deploying |
| `virtiofsd` | Sharing the host `/nix/store` with the guest | Deploying |
| `socat` | Forwarding the host port to the guest | Deploying |
| `iproute2` (`ip`) | Creating and configuring TAP interfaces | Starting `russel-ctrl` / deploying |
| `iptables` | Cleaning and configuring host NAT/forwarding rules | Starting `russel-ctrl` / deploying |
| `git` | Cloning remote application repositories | Deploying a remote repository |

The host also needs:

- A working KVM setup with access to `/dev/kvm` (and virtualization enabled in firmware).
- Permission to create TAP devices and change networking/iptables rules. Run the control plane with the appropriate root privileges, or grant equivalent capabilities to the binary in a controlled environment.
- A writable `/var/lib/russel` directory for VM state, logs, and cached artifacts.

After Nix is installed, the repository flake provides Rust, `rust-analyzer`, Cloud Hypervisor, and the runtime utilities for development:

```bash
nix develop
```

On a non-Nix host, install the equivalent packages with your distribution's package manager. The exact package names vary; on Debian/Ubuntu they are typically `build-essential`, `pkg-config`, `libssl-dev`, `nix`, `cloud-hypervisor`, `virtiofsd`, `socat`, `iproute2`, `iptables`, and `git`.

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

## License

MIT
