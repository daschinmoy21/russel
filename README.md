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

**Requirements:** Linux host with Nix (flakes), rootless Podman (`podman info` reports rootless), and a writable `/var/lib/russel`. For microVM deploys only: KVM (`/dev/kvm`), TAP, iptables.

```bash
# 1. Dev shell (Rust + cloud-hypervisor + podman on Linux)
nix develop

# 2. Build CLI + control plane
cargo build
# → target/debug/russel-cli
# → target/debug/russel-ctrl

# 3. Optional auth (required if binding non-loopback)
export RUSSEL_API_TOKEN=your-secret-token   # same value in both terminals

# 4. Start control plane (terminal 1) — default http://127.0.0.1:7878
./target/debug/russel-ctrl
# override bind: RUSSEL_CTRL_ADDR=127.0.0.1:7878

# 5. Deploy example (terminal 2, from repo root)
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id test-api
# CLI target: RUSSEL_CONTROL_PLANE=http://127.0.0.1:7878 (default)

# 6. Verify
curl http://127.0.0.1:8080/health
# → ok

./target/debug/russel-cli status test-api
./target/debug/russel-cli vms
./target/debug/russel-cli logs test-api

# 7. Tear down
./target/debug/russel-cli stop test-api
./target/debug/russel-cli destroy test-api
```

**Without `-p`:** Traefik is the primary HTTP ingress. Omit publish and open `http://<service_id>.russel.local` once Traefik watches `/var/lib/russel/traefik/dynamic` (see [docs/traefik.md](docs/traefik.md)).

**Container runtime:** the example Russelfile already has `type = "container"`. Pass `--runtime container` only if it matches. Needs **rootless** Podman.

If ctrl runs under `sudo` for microVMs, container deploys use rootless podman as `RUSSEL_PODMAN_USER` or `SUDO_USER` (not root). That user needs `podman info` → rootless true and `/run/user/$(id -u)` (try `loginctl enable-linger $USER` on headless hosts).

```bash
# Container path (examples/basic-http already sets type = "container")
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id test-api \
  --runtime container
```

**Secrets** (host store under `/var/lib/russel/secrets/`, mode `0600`):

```bash
printf '%s' "$DB_PASSWORD" | ./target/debug/russel-cli secrets set DB_PASSWORD
./target/debug/russel-cli secrets list
# Reference in Russelfile or --env as secret://DB_PASSWORD
```

**Update** a running service from its last deploy source:

```bash
./target/debug/russel-cli update test-api
# optional overrides: --repo PATH_OR_URL --config Russelfile.toml
```

More walkthroughs: [docs/examples.md](docs/examples.md) · packaging: [docs/deployment.md](docs/deployment.md).

## Build Command

There is currently no `russel-cli build` command. Russel builds an application automatically during `russel-cli deploy`; the control plane invokes Nix with the repository's `packages.<system>.default` output and returns the resulting store path internally. Use `nix build` directly only when you want to inspect or run an application artifact without deploying a microVM.

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
| `GET` | `/secrets` | List secret names (values never returned) |
| `POST` | `/secrets/{name}` | Set secret (`{"value":"..."}`); store mode `0600` |
| `DELETE` | `/secrets/{name}` | Delete a secret |
| `GET`  | `/status` | Get deployment status for all services (when `service_id` omitted) |
| `GET`  | `/logs`   | Get logs for all services (when `service_id` omitted) |
| `GET`  | `/vm/{service_id}/status` | Get deployment status for a service |
| `GET`  | `/vm/{service_id}/logs`   | Get logs for a service |
| `GET`  | `/vm/{service_id}/deployments` | List deployment history (newest first; journal under `/var/lib/russel/<id>/deployments.json`) |
| `POST` | `/vm/{service_id}/rollback` | Explicit operator rollback to a prior version (`{"version":N}` optional; NDJSON stream) |
| `GET`  | `/vms`                     | List registered services |
| `POST` | `/vm/{service_id}/stop`    | Stop a service (microVM or container) |
| `POST` | `/vm/{service_id}/update`  | Redeploy from recorded/overridden source (returns NDJSON stream) |
| `DELETE`| `/vm/{service_id}`         | Destroy a service and clean up resources |

### Deployment history and rollback

Each successful deploy appends a versioned row to `/var/lib/russel/<service_id>/deployments.json` (cap 20). Status values: `active`, `previous`, `superseded`, `rolled_back`. The prior `active` becomes `previous` (one); older previous rows become `superseded`. Each row stores a `desired_state` snapshot (`repo_url`, `config_path`, `runtime`, `env`, `podman_args`, ports) so operators can roll back without waiting for a failed redeploy.

`POST /vm/{id}/rollback` with optional `{"version": N}` (omit to select the latest `previous` with `rollback_ready`) **redeploys from history** via the normal deploy pipeline. Instant dual-live retain-N=2 cutover (keeping the previous generation artifact hot) is a follow-up; if a target has no recorded source, the API returns **409** with a clear message.

## CLI Commands

Binaries are `russel-cli` and `russel-ctrl` (debug: `./target/debug/...`). Clap program name is `russel`.

**Global option:** `--control-plane URL` (env: `RUSSEL_CONTROL_PLANE`, default `http://127.0.0.1:7878`). All subcommands honour it.

```bash
russel-cli deploy <repo> [-p HOST:GUEST] [--config PATH] [--vm-id ID] \
  [--runtime microvm|container] [--env KEY=VALUE...] [--env-file PATH] \
  [-- <podman-run-args...>]          # container only, after --
russel-cli status [<service_id>]
russel-cli logs [<service_id>]
russel-cli vms
russel-cli stop <service_id>
russel-cli destroy <service_id>
russel-cli update <service_id> [--repo REPO] [--config PATH]
russel-cli secrets set <name>        # value from stdin
russel-cli secrets list
russel-cli secrets delete <name>
```

- **`--env KEY=VALUE`** (repeatable): Set an environment variable for the deployed service. Overrides `[service.env]` from the Russelfile.
- **`--env-file PATH`**: Load `KEY=VALUE` pairs from a file (`#` comments, blank lines skipped). Merged with `[service.env]` and `--env` (later wins).
- Reserved keys (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are rejected for user-defined env vars.
- **Secrets** (host store, not committed): `printf '%s' "$VAL" | russel-cli secrets set NAME`, `list`, `delete` (value from stdin, never argv). In env maps use `secret://NAME` — the control plane resolves the value at deploy time from `/var/lib/russel/secrets/` (mode `0600`). HTTP: `GET /secrets`, `POST /secrets/{name}`, `DELETE /secrets/{name}` (Bearer auth when configured).

- **`--runtime`** is **not** an override. If set, it must match `service.type` in the Russelfile (or the default `microvm` when omitted). Mismatch → hard error.
- Ports today: **`-p HOST:GUEST`** (published binds). Traefik is the primary HTTP gateway; `-p` is optional for HTTP services.
- **Remote deploys** accept `https://`, `http://`, `ssh://`, and `git@host:path` only. Link-local metadata hosts (`169.254.169.254`) are blocked.

### Config Path & Bin Name

- `--config` must be a **relative** path under the repository root. The control plane opens it via `openat` with `O_NOFOLLOW` (symlinks rejected) and enforces a 1 MiB size cap.
- The binary name (from `Russelfile.toml` `bin` or `name`) must match `[A-Za-z0-9._+-]` (max 256 chars). It is injected into the guest via a shell-quoted `deploy.env` file.

### Redeploy / update

Redeploying an existing service kills and waits for old processes before reusing ports. If a new deploy fails after a prior successful deployment, Russel attempts automatic **rollback** to the previous running service. A successful rollback reports status `rolled_back`; the CLI exit code is non-zero so CI pipelines can detect the failure.

**`russel-cli update <id>`** re-applies desired state from the `repo_url` / `config_path` recorded in metadata at the last successful deploy (override with `--repo` / `--config`).

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

**Russel containers** prepare a rootfs under `/var/lib/russel/<id>/rootfs` and bind-mount the host `/nix/store` read-only. By default (`debug = false`) the rootfs is **read-only** (tmpfs `/tmp` and `/run` only) with no bash, curl, or `/usr/bin/env` — entrypoints must be statically linked or use an absolute `/nix/store/…` interpreter. Set `debug = true` in your Russelfile to include shell debugging tools. Do **not** pass a bare Nix package path as `--rootfs` yourself — use `russel deploy`.

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
- For containers: **rootless Podman** configured and working (`podman info` reports rootless). Russel does not use rootful Podman. When ctrl runs as root (`sudo`), set `RUSSEL_PODMAN_USER` or rely on `SUDO_USER` so podman runs as that user.
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
├── examples/         # see examples/README.md
│   ├── basic-http/   # Go + /health (container default)
│   ├── microvm-http/ # same app, type = microvm
│   ├── hello-rust/   # pure-std Rust HTTP
│   ├── env-config/   # [service.env] + secret://
│   ├── shortlink/    # in-memory URL shortener
│   ├── filebrowser/  # nixpkgs filebrowser wrapper
│   └── static-test/  # Python static site
├── nix/
│   ├── microvm/      # legacy/reference configs; runtime boots Cloud Hypervisor directly
│   └── modules/      # host NixOS modules
├── docs/
│   ├── architecture.md    # Control plane internals
│   ├── auto-generation.md # Flake auto-detection
│   ├── deployment.md      # App packaging guide
│   ├── examples.md        # Example projects (microVM + container)
│   ├── russelfile.md      # Russelfile reference / design
│   └── traefik.md         # Traefik ingress setup
├── flake.nix         # development shell
└── README.md
```

## Benchmark (2026-07-24)

Warm run (`./bench.sh --warm`) on NixOS, podman 5.8.2, rootless containers via
`SUDO_USER`, microVM via Cloud Hypervisor. Total wall time ~271s (includes clean
debug + release + tests + 7 app races).

**MicroVM path must use the Russel-compiled kernel** (flake package
`.#microvm-kernel`, virtio drivers built-in). `./bench.sh` builds that package
and exports `RUSSEL_KERNEL_PATH` so `russel-ctrl` does **not** fall back to a
stock nixpkgs kernel (slower / wrong module set). Numbers below assume that
kernel.

### Systems

| Metric | Value |
|--------|-------|
| Clean build (debug) | 21.7s |
| Release build | 70.2s |
| Tests | ~301 unit tests, 45.9s (workspace) |
| Binary size (cli) | 7.1MB (~5.5MB stripped) |
| Binary size (ctrl) | 5.9MB (~4.5MB stripped) |
| Rust LOC | 17,241 (22 files) |
| Direct deps | 340 |
| Dockerfiles | 7 (all examples) |
| Clippy warnings | 0 |

### App boot race — End-to-End (build/deploy + first HTTP)

Three paths per example: **Russel microVM**, **Russel container** (rootless
podman `--rootfs`), **raw podman** (Dockerfile build + run). Winner = lowest E2E.

| App | Russel microVM | Russel container | Podman baseline | Winner |
|-----|----------------|------------------|-----------------|--------|
| basic-http | 3.24s (3.21+0.04) | **1.37s** (1.35+0.02) | 7.81s (7.28+0.53) | Russel-ctr |
| microvm-http | 1.27s (1.26+0.01) | **1.09s** (1.07+0.02) | 9.24s (8.88+0.37) | Russel-ctr |
| hello-rust | 1.53s (1.48+0.05) | **1.14s** (1.11+0.03) | 9.17s (8.61+0.56) | Russel-ctr |
| env-config | 1.26s (1.24+0.02) | **0.95s** (0.94+0.01) | 10.23s (9.79+0.44) | Russel-ctr |
| shortlink | 1.53s (1.50+0.03) | **1.19s** (1.16+0.02) | 10.09s (9.58+0.51) | Russel-ctr |
| static-test | 1.99s (1.96+0.03) | **1.27s** (1.10+0.17) | 2.60s (1.32+1.28) | Russel-ctr |
| filebrowser | 2.68s (2.64+0.04) | **1.74s** (1.60+0.14) | 9.04s (8.28+0.77) | Russel-ctr |

`(deploy+curl)` for Russel; `(image build + run→HTTP)` for podman baseline.

### Spawn-to-Ready (excluding build)

Strips Nix / Dockerfile build time. Best apples-to-apples “how fast after the
package exists.”

| App | Russel microVM | Russel container | Podman baseline |
|-----|----------------|------------------|-----------------|
| basic-http | 1892ms | 669ms | **528ms** |
| microvm-http | 919ms | 655ms | **366ms** |
| hello-rust | 880ms | 659ms | **562ms** |
| env-config | 851ms | 639ms | **443ms** |
| shortlink | 1046ms | 847ms | **509ms** |
| static-test | 1371ms | **795ms** | 1279ms |
| filebrowser | 1261ms | 840ms | **766ms** |

**Reading the tables:** E2E favors Russel on warm runs (Nix store cache vs image
build). Spawn-to-Ready is usually raw podman ≤ Russel container &lt; microVM —
microVM pays for Cloud Hypervisor + virtiofs + guest init in exchange for stronger
isolation. Container memory capped at 256m to match microVM defaults.

### Last container deploy phases (example)

| Phase | Time | Detail |
|-------|------|--------|
| resolve | 0ms | repo + Russelfile |
| build | 897ms | nix build (package) |
| create | 0ms | rootfs adapter |
| network | 0ms | n/a for container |
| start | 643ms | rootless podman run --rootfs |
| ready | 0ms | host TCP/HTTP ready |
| **total** | **1540ms** | sum of phases |

Reproduce:

```bash
# Required for microVM: custom kernel (bench builds .#microvm-kernel and sets
# RUSSEL_KERNEL_PATH automatically). Root for TAP/KVM + SUDO_USER rootless podman:
sudo -E nix develop -c ./bench.sh --warm

# Or pin a prebuilt kernel explicitly (same artifact as nix build .#microvm-kernel):
nix build .#microvm-kernel -o result-kernel
export RUSSEL_KERNEL_PATH="$(readlink -f result-kernel/bzImage)"
sudo -E env RUSSEL_KERNEL_PATH="$RUSSEL_KERNEL_PATH" nix develop -c ./bench.sh --warm
```

Requires Rust toolchain, nix, KVM + root for microVM, the **compiled**
`.#microvm-kernel` (or a valid `RUSSEL_KERNEL_PATH` to that `bzImage`), rootless
podman for Russel containers, and podman/docker for the Dockerfile baseline.
Use `--cold` to force cold Nix + image rebuilds on both sides. Without the
custom kernel, microVM races are skipped.

### Known Limitations

- **Subnet collision detection** — 16-bit FNV-1a space, <2% collision at 50 services. Add when scale demands it.
- **virtiofsd --readonly** — `/nix/store` is read-only from the guest (added via `--readonly` flag). Remove only when a workflow needs guest-side store mutations.
- **Database stubs** — `[database.*]` in Russelfile is still a placeholder; health probes + optional restart are implemented (`RUSSEL_HEALTH_*`).
- **No integration/e2e tests** — requires KVM + root. Marked `#[ignore]` candidate for a future e2e crate.
- **Auth optional on loopback** — dev mode warns but does not enforce. Production should always set `RUSSEL_API_TOKEN`.
- **Nix builds trust the source repo** — a malicious `flake.nix` runs as the build user. Multi-tenant: only deploy trusted repos. Opt-in `RUSSEL_NIX_RESTRICTED=1` forces sandboxed `nix build` and disables auto-flake. Full threat model: [docs/security/nix-builds.md](docs/security/nix-builds.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, coding standards, and the mandatory full test/fmt/clippy checklist. Always run `cargo test --workspace` (and fmt + clippy) before opening a PR.

## License

MIT
