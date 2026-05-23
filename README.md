# Russel

A self-hosted platform for deploying services as NixOS microVMs using [microvm.nix](https://github.com/astro/microvm.nix) and Cloud-Hypervisor.

## Architecture

Russel is split into three crates:

| Crate | Purpose |
|-------|---------|
| `russel-core` | Shared types: `Russelfile` config, API request/response types |
| `russel-cli` | CLI client that talks to the control plane |
| `russel-ctrl` | Control plane (Axum HTTP API) that orchestrates builds, microVMs, and networking |

### Deployment Flow

1. **Resolve** — Clone (or use local) repo and parse `Russelfile.toml`
2. **Build** — Run `nix build` on the repo's `flake.nix` to produce a store path
3. **Create Initramfs** — `russel-ctrl` builds a minimal initramfs containing only BusyBox and required VirtIO kernel modules.
4. **Start Auxiliaries** — Spawns `virtiofsd` serving the host `/nix/store` to the guest via a UNIX socket.
5. **Start VM** — Spawns `cloud-hypervisor` directly (bypassing systemd and registration) booting the stock kernel, the minimal initramfs, and sharing `/nix/store` via virtiofs.
6. **Kernel Init** — Guest kernel boots. The custom `/init` script decompresses and loads the VirtIO networking and filesystem modules via `insmod` in dependency order, mounts `/nix/store` via `virtiofs`, configures static guest IP (`10.0.<idx>.2`), and directly executes the application binary.
7. **Network** — Host creates the TAP interface, configures host IP (`10.0.<idx>.1`), and spawns `socat` to forward `0.0.0.0:<host_port>` → `<vm_ip>:<guest_port>`.
8. **Ready** — Polls the guest port until it responds, confirming deployment success.

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
```

## API Endpoints

The control plane listens on `127.0.0.1:7878` by default (override with `RUSSEL_CTRL_ADDR`).

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/deploy` | Deploy or re-deploy a service (returns NDJSON stream) |
| `GET`  | `/status` | Get current deployment status |
| `GET`  | `/logs`   | Get aggregated logs |
| `GET`  | `/vms`    | List registered microVMs |
| `POST` | `/vm/:id/stop` | Stop a microVM |
| `DELETE`| `/vm/:id` | Destroy a microVM and clean up resources |

## CLI Commands

```bash
russel deploy <repo-url> [-p HOST:GUEST] [--config PATH] [--vm-id ID]
russel status
russel logs
russel vms
russel stop <vm-id>
russel destroy <vm-id>
```

## Project Requirements

A repository you want to deploy must contain configuration files in its root. For a complete guide, templates, and language references, see the [Application Deployment Guide](docs/deployment.md).

At a minimum, it must contain:

1. **`flake.nix`** with `packages.x86_64-linux.default` building your app
2. **`Russelfile.toml`** describing the service:

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"
```

3. The app must expose a **`/health`** endpoint on `PORT` — this is what Russel polls to determine readiness.

## Networking Model

- Each VM gets a deterministic `/30` subnet derived from its `service_id` (e.g. `10.0.<idx>.2/30`)
- Host-side TAP interface (`vm-<id>`) is created by Cloud-Hypervisor and configured by `russel-ctrl`
- `socat` forwards host port → VM port
- Traefik is configured automatically for ingress

## Boot & Network Timing Optimization (Under 2s Boot)

To optimize boot time from 40s+ to under 2s, we transitioned from a heavy guest-side NixOS systemd VM to a direct microVM boot model:

- **Minimal Initramfs**: Dropped the Nix OS closure from guest RAM. The initramfs is only 2-3MB, consisting of BusyBox and necessary drivers.
- **Virtiofs /nix/store Share**: The host `/nix/store` is mounted directly inside the guest using `virtiofsd` and the `virtiofs` filesystem driver, allowing instant access to the application closure without packaging it in the initrd.
- **Kernel Module Bootstrapping**: Since standard nixpkgs kernels compile VirtIO networking (`virtio_net`) and VirtIO filesystem (`virtiofs`) as modules (`=m`), our `/init` script dynamically extracts and loads the VirtIO dependency chain (e.g. `virtio_ring`, `virtio.ko`, `virtio_pci_*`, `virtio_net`, `fuse`, `virtiofs`) via `insmod` before mounting the store or bringing up `eth0`.
- **Direct App Execution**: Bypasses systemd in the guest. The `/init` script executes the application binary directly, reducing guest-side lifecycle overhead to virtually zero.

## Requirements

- Nix with flakes enabled
- `microvm` command available on the host (from microvm.nix)
- `cloud-hypervisor`
- `socat`, `iproute2`
- Traefik running for ingress
- KVM / `/dev/kvm` access

## Repo Layout

```
.
├── crates/
│   ├── cli/          # russel-cli
│   ├── core/         # shared types & config
│   └── ctrl/         # control plane (main logic)
├── examples/
│   └── basic-http/   # example Go service with flake.nix
├── microvm.nix/      # vendored microvm.nix
├── nix/
│   └── modules/      # host NixOS modules
├── flake.nix         # dev shell only
└── README.md
```

## License

MIT
