# Russel

A self-hosted platform for deploying Nix-built services as **microVMs** ([Cloud Hypervisor](https://www.cloudhypervisor.org/)) or **Russel containers** (rootless Podman `--rootfs`).

## Architecture

Russel is split into four crates:

| Crate | Purpose |
|-------|---------|
| `russel-core` | Shared types: `Russelfile` config, API request/response types |
| `russel-cli` | CLI crate. The command is **`russel`**. |
| `russel-ctrl` | Control plane (Axum HTTP API) that orchestrates builds, **microVMs** and **Russel containers**, and networking. MicroVMs use Cloud Hypervisor; containers use rootless Podman `--rootfs`. |
| `russel-agent` | Node-local agent for multi-host lifecycle RPC (heartbeat + stop/destroy/status proxies) |

### Deployment Flow

1. **Resolve** — Clone (or use local) repo and parse `Russelfile.toml` (including `service.type`).
2. **Build** — Auto-generate `flake.nix` if missing (Rust/Go/static detection), then run `nix build` to produce a store path.
3. **Branch on runtime**
   - **microvm (experimental; KVM + root ctrl):** minimal initramfs → TAP + socat + virtiofsd → Cloud Hypervisor → guest runs app from virtiofs `/nix/store`.
   - **container (default):** prepare Docker-like rootfs → rootless Podman `--rootfs` + `/nix/store:ro` bind → publish on `[ingress].port` or an allocated host port.
4. **Ready** — the app accepts connections on the guest (microVM) or through the published host port while the container stays up (container); metadata written under `/var/lib/russel/<id>/`. Register with the `Ingress` trait (default: `TraefikFileIngress` writes Traefik dynamic config) so the reverse proxy can route traffic to the new backend.

## Quick Start

**Requirements:** Linux host with Nix (flakes), rootless Podman (`podman info` reports rootless), and a writable `/var/lib/russel`. For microVM deploys only: KVM (`/dev/kvm`), TAP, iptables.

```bash
# 1. Dev shell (Rust + cloud-hypervisor + podman on Linux)
nix develop

# 2. Build CLI + control plane
cargo build
# → target/debug/russel
# → target/debug/russel-ctrl

# 3. Optional auth (required if binding non-loopback; min 32 chars)
export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"   # same value in both terminals

# 4. Start control plane (terminal 1) — default http://127.0.0.1:7878
# Local absolute path deploys need an explicit opt-in on the control plane:
export RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1
./target/debug/russel-ctrl
# override bind: RUSSEL_CTRL_ADDR=127.0.0.1:7878

# 5. Deploy example (terminal 2, from repo root)
./target/debug/russel deploy examples/basic-http
# CLI target: RUSSEL_CONTROL_PLANE=http://127.0.0.1:7878 (default)
# Prefer a git URL for remote/shared control planes (local paths default OFF).

# 6. Verify
curl http://127.0.0.1:8080/health
# → ok

./target/debug/russel status api
./target/debug/russel ps
./target/debug/russel logs api

# 7. Tear down
./target/debug/russel stop api
./target/debug/russel destroy api
```

Put `russel` on PATH with [docs/getting-started/installation.md](docs/getting-started/installation.md) (`./contrib/install.sh cli`). For the non-NixOS path, the short remote flow is:

```bash
# On the control-plane host (runs russel-ctrl as a dedicated `russel` account):
./contrib/install.sh check
sudo ./contrib/install.sh host

# On the laptop, with the token file copied privately:
./contrib/install.sh connect user@host
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
```

Use `services.russel` on NixOS. The full topology guide is in [docs/getting-started/installation.md](docs/getting-started/installation.md).

**Traefik ingress:** Open `http://<service.name>.russel.local` once Traefik watches `/var/lib/russel/traefik/dynamic` (see [docs/guides/traefik-ingress.md](docs/guides/traefik-ingress.md)).

**Container runtime:** `examples/basic-http` sets `type = "container"`. It needs **rootless** Podman.

If ctrl runs under `sudo` (for microVM TAP networking), container deploys use rootless podman as `RUSSEL_PODMAN_USER` or `SUDO_USER` (not root). That user needs `podman info` → rootless true and `/run/user/$(id -u)` (try `loginctl enable-linger $USER` on headless hosts).

```bash
# Container path (examples/basic-http already sets type = "container")
./target/debug/russel deploy examples/basic-http
```

**Secrets** (host store under `/var/lib/russel/secrets/`, mode `0600`):

```bash
printf '%s' "$DB_PASSWORD" | ./target/debug/russel secrets set DB_PASSWORD
./target/debug/russel secrets list
# Reference in the Russelfile as secret://DB_PASSWORD
```

**Update** a running service from its last deploy source:

```bash
./target/debug/russel update api
# optional overrides: --repo PATH_OR_URL --config Russelfile.toml
```

More walkthroughs: [docs/reference/examples.md](docs/reference/examples.md) · packaging: [docs/getting-started/first-deploy.md](docs/getting-started/first-deploy.md).

## Remote VPS (one developer)

For a single trusted operator on one Linux VPS, the supported path is containers
(rootless Podman). Most cheap VPS images have no `/dev/kvm`; skip microVMs
unless you have nested virt or bare metal.

| Ready? | Item |
|--------|------|
| Yes | CLI → remote `russel-ctrl` → git deploy → status / logs / destroy |
| Yes | Bearer auth (`RUSSEL_API_TOKEN` ≥32 chars); non-loopback requires token |
| Yes | TLS via reverse proxy (Caddy/nginx) in front of loopback ctrl |
| Containers | The default `service.type`; microVMs are experimental (KVM + root ctrl) |
| Yes | Install: NixOS module (`nixosModules.russel`) or `contrib/russel-ctrl.service` |
| No | Multi-tenant isolation, managed DBs, native ctrl TLS |

NixOS hosts use `services.russel`. The module binds `127.0.0.1:7878`, sets
`RUSSEL_REQUIRE_AUTH=1`, keeps `/var/lib/russel` at mode 0700, and leaves the
warm pool off. `rootlessPodman` defaults true. Put Caddy, nginx, or Traefik in
front ([docs/guides/tls-reverse-proxy.md](docs/guides/tls-reverse-proxy.md), [docs/guides/traefik-ingress.md](docs/guides/traefik-ingress.md)).
KVM and TAP are optional on a container-only VPS.

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl"; # or services.russel.package
  services.russel.environmentFile = "/etc/russel/env"; # RUSSEL_API_TOKEN=... mode 0600
  # Existing login account. Linger is set. Set group, or omit it:
  # services.russel.user = "you";
  # services.russel.group = "users";
  # services.russel.createUser = false;
  # NNP on, no Podman (does not configure microVMs):
  # services.russel.rootlessPodman = false;
}
```

On other Linux, run `./contrib/install.sh check`, then `sudo ./contrib/install.sh host`
from the repository root after building the release binaries. It creates an
unprivileged `russel` account that runs russel-ctrl and every workload, installs
the system unit, creates `/etc/russel/env` (readable by the `russel` group, which
you join) if needed, and keeps existing `/etc/russel/env` and `/var/lib/russel`
contents on upgrades. The unit binds loopback and uses `RUSSEL_REQUIRE_AUTH=1`.

Choose one control-plane topology:

| Topology | Client connection | Dashboard |
|----------|-------------------|-----------|
| A. Same host | `russel login http://127.0.0.1:7878 --token-file /etc/russel/env` | `http://127.0.0.1:7878/`, Settings `/api` |
| B. Split plus SSH | `./contrib/install.sh connect user@host`, then the same loopback login | same URL through the tunnel, Settings `/api` |
| C. Split plus HTTPS | `russel login https://russel.example.com --token-file ~/.config/russel/env` | same HTTPS origin; proxy can forward `/` and `/api/` to loopback ctrl |

For a remote control plane, deploy from a git URL. A laptop filesystem path is
not uploaded; absolute paths are resolved on the control-plane host. See the
[full install guide](docs/getting-started/installation.md), the [VPS checklist](docs/guides/vps-one-dev.md),
and the [TLS runbook](docs/guides/tls-reverse-proxy.md).

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

When `RUSSEL_API_TOKEN` is set on the control plane, **every** API route requires an `Authorization: Bearer <token>` header. The token must be **at least 32 characters** after trim (generate with `openssl rand -hex 32`). The CLI reads the same environment variable and sends it automatically. On loopback without a token the control plane runs in dev mode (with a warning); binding to a non-loopback address **requires** `RUSSEL_API_TOKEN` or the control plane refuses to start. Set `RUSSEL_REQUIRE_AUTH=1` (or `true`/`yes`) to refuse startup without a valid token even on loopback — recommended for production packaging. Always set a strong `RUSSEL_API_TOKEN` and bind to the internal interface where your reverse proxy lives.

**Cleartext Bearer (#189):** `russel-ctrl` is HTTP-only. The CLI (and dashboard client) **refuse** to send the token over `http://` to a non-loopback host. Terminate TLS at Caddy/nginx/Traefik in front of loopback ctrl — see [docs/guides/tls-reverse-proxy.md](docs/guides/tls-reverse-proxy.md). Override only with `--insecure` or `RUSSEL_INSECURE_CLEARTEXT=1` (not recommended).

### Bind Policy

| Scenario | Behaviour |
|----------|-----------|
| Loopback (`127.0.0.1:…`) + no token | Dev mode (warn, no auth) |
| Loopback + `RUSSEL_REQUIRE_AUTH` + no token | **Refuses to start** |
| Loopback + `RUSSEL_API_TOKEN` set (≥32 chars) | Bearer auth required on all routes |
| Any bind + token set but &lt;32 chars | **Refuses to start** |
| Non-loopback + no token | **Refuses to start** |
| Non-loopback + `RUSSEL_API_TOKEN` set (≥32 chars) | Bearer auth required on all routes |

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/deploy` | Deploy or re-deploy a service (`vm_id` **required**; returns NDJSON stream) |
| `GET` | `/secrets` | List secret names (values never returned) |
| `POST` | `/secrets/{name}` | Set secret (`{"value":"..."}`); store mode `0600` |
| `DELETE` | `/secrets/{name}` | Delete a secret |
| `GET`  | `/status` | Status of the single registered service; **400** if multiple services exist (pass `/vm/{id}/status`) |
| `GET`  | `/logs`   | Logs of the single registered service; **400** if multiple services exist (pass `/vm/{id}/logs`) |
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

Binaries are `russel` (client) and `russel-ctrl` (control plane). Debug: `./target/debug/...`. Install: [docs/getting-started/installation.md](docs/getting-started/installation.md).

**Global options:**
- `--control-plane URL` (env: `RUSSEL_CONTROL_PLANE`; else `russel login` config; else `http://127.0.0.1:7878`)
- `--insecure` — allow Bearer over plain HTTP to non-loopback hosts (also `RUSSEL_INSECURE_CLEARTEXT=1|true|yes`; prefer HTTPS — [docs/guides/tls-reverse-proxy.md](docs/guides/tls-reverse-proxy.md))

Auth is `RUSSEL_API_TOKEN` **or** `russel login` (writes `~/.config/russel/config.toml`, mode `0600`). Env wins over the file.

**Fish.** Use the token-file login path. It writes the CLI config without
loading a `KEY=VALUE` file into shell state:

```fish
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
russel ps
```

```bash
russel login [<url>] [--token-file PATH]   # token from file, env, or stdin
russel logout
russel origin                              # which ctrl this CLI will hit
russel deploy <REPO> [--config PATH]
russel status [<service_id>]
russel logs [<service_id>]
russel ps                                  # aliases: list, vms
russel stop <service_id>
russel destroy <service_id>
russel update <service_id> [--repo REPO] [--config PATH]
russel secrets set <name>        # value from stdin
russel secrets list
russel secrets delete <name>
```

- Reserved keys (`PORT`, `VM_IP`, `HOST_IP`, `APP`) are rejected for user-defined env vars.
- **Secrets** (host store, not committed): `printf '%s' "$VAL" | russel secrets set NAME`, `list`, `delete` (value from stdin, never argv). In env maps use `secret://NAME` — the control plane resolves the value at deploy time from `/var/lib/russel/secrets/` (mode `0600`). HTTP: `GET /secrets`, `POST /secrets/{name}`, `DELETE /secrets/{name}` (Bearer auth when configured).

- `service.type` selects the runtime; `[ingress].port` pins the host side of the service port. Traefik is the primary HTTP gateway.
- **Remote deploys** accept `https://`, `http://`, `ssh://`, and `git@host:path` only. Link-local / private / metadata hosts (e.g. `169.254.169.254`, RFC1918) are blocked on the control plane.
- **Local absolute path deploys** are **disabled by default**. Set `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane only for single-tenant trusted hosts (local dev). Paths are resolved on the **control-plane host**, not the CLI client — do not enable this on a shared/remote ctrl. Relative paths are always rejected; prefer a git URL when the control plane is remote.

### Config Path & Bin Name

- `--config` must be a **relative** path under the repository root. The control plane opens it via `openat` with `O_NOFOLLOW` (symlinks rejected) and enforces a 1 MiB size cap.
- `service.name` follows the service id rule (`[A-Za-z0-9_-]`, max 128). The binary name (`bin`, else `name`) must match `[A-Za-z0-9._+-]` (max 256 chars). Both are checked when the Russelfile loads. It is injected into the guest via a shell-quoted `deploy.env` file.

### Redeploy / update

Redeploying an existing service kills and waits for old processes before reusing ports. If a new deploy fails after a prior successful deployment, Russel attempts automatic **rollback** to the previous running service. A successful rollback reports status `rolled_back`; the CLI exit code is non-zero so CI pipelines can detect the failure.

**`russel update <id>`** rebuilds from the recorded repo and config path, then applies the current Russelfile. Changes to `[service.env]`, `service.port`, and `[ingress]` take effect; values from the previous deploy are not replayed. The Russelfile `service.name` must equal `<id>`. `--repo` / `--config` can select a source and config path.

### Health

The control plane probes `127.0.0.1:<host_port>` every `RUSSEL_HEALTH_INTERVAL_SECS` (default 30). After three consecutive failures the service is marked failed. Set `RUSSEL_HEALTH_RESTART=1` to auto-redeploy from the recorded source.

## Project Requirements

A repository you want to deploy needs a `Russelfile.toml` in its root. Scaffold one with `russel init` (add `--with-flake` to also write a starter `flake.nix`). If no `flake.nix` is present, Russel auto-generates one based on project type (Rust → Cargo.toml, Go → go.mod, else → static server). See [First deploy](docs/getting-started/first-deploy.md), [Builds](docs/concepts/builds.md), and [Examples](docs/reference/examples.md) for details.

```bash
cd my-app
russel init                       # writes Russelfile.toml
russel init --type microvm        # experimental: KVM host + passt
russel init --with-flake          # also writes flake.nix
# then: russel deploy .
```

```toml
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "api"    # optional — defaults to name
type = "container"  # optional: "container" (default) or "microvm" (experimental)
guest = "busybox" # optional: "busybox" (default). "linux" is rejected until implemented

[service.env]    # optional — user-defined environment variables
LOG_LEVEL = "info"
FEATURE_X = "1"
```

### Runtimes

| `service.type` | Isolation | Host needs |
|----------------|-----------|------------|
| `container` (default) | Rootless Podman `--rootfs` | Rootless Podman |
| `microvm` (experimental) | KVM / Cloud Hypervisor | KVM, virtiofsd, passt (no root; a root ctrl uses TAP + socat) |

`type` is isolation. `guest` is the userspace inside that isolation.

| Field | Values | Status |
|-------|--------|--------|
| `type` | `microvm`, `container` | both ship |
| `guest` | `busybox` (default), `linux` | `linux` is parsed then rejected at load until boot exists |

**Russel containers** prepare a rootfs under `/var/lib/russel/<id>/rootfs` and bind-mount the host `/nix/store` read-only. By default (`debug = false`) the rootfs is **read-only** (tmpfs `/tmp` and `/run` only) with no bash, curl, or `/usr/bin/env` — entrypoints must be statically linked or use an absolute `/nix/store/…` interpreter. Set `debug = true` in your Russelfile to include shell debugging tools. Do **not** pass a bare Nix package path as `--rootfs` yourself — use `russel deploy`.

```bash
# The example Russelfile sets name = "api" and type = "container".
./target/debug/russel deploy examples/basic-http

# Optional container Podman flags belong in the Russelfile:
# [service]
# podman_args = ["-v", "/data:/data:ro", "--network", "bridge"]
```

Russel checks readiness by connecting to the guest port via TAP (microVM) or to the published host port (container). For containers, a bare connect is not enough, because the rootless port forwarder accepts even when the app is down: the connection must stay open for 200 ms (or the app must send data), and the container must not exit or restart. A container that exits during startup fails the deploy at once with its exit code and the last 40 lines of its output. Otherwise the app has 30 s to answer. For application-level health monitoring, expose a `/health` endpoint on `PORT` as a convention (Traefik is already the ingress).

## Networking Model

- **MicroVM:** Each VM gets a deterministic `/30` subnet from `service_id` (FNV-1a), host TAP `rsl-<hex>`, `socat` host→guest port forward. Guest L3 isolation uses a dedicated `RUSSEL-FORWARD` iptables chain (default-deny for `rsl-*`); set `RUSSEL_FORWARD=allow` only for single-tenant debugging (guests can otherwise pivot via host routing when `ip_forward=1`).
- **Container:** Rootless Podman publishes the host port configured by `[ingress].port` or selected by the allocator.

### Port Publishing (today)

`[ingress].port` pins a host port; otherwise Russel allocates one. The port is published via `socat` (microVM) or Podman port mapping (container), through the port allocator so host ports do not collide across services.

### Traefik Gateway

Traefik is the **primary HTTP ingress gateway**. Russel writes dynamic configuration files into `/var/lib/russel/traefik/dynamic/` (override with `RUSSEL_TRAEFIK_DYNAMIC_DIR`). Each deployed service gets a Host rule: `<service.name>.<domain>` (domain defaults to `russel.local`, override with `RUSSEL_TRAEFIK_DOMAIN`).

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

With Traefik running, access your service at `http://<service.name>.russel.local` (requires DNS or `/etc/hosts` entry pointing to Traefik's IP). The host port is auto-allocated as a private backend unless `[ingress].port` pins it.

On destroy/stop, Russel removes the dynamic config file so Traefik stops routing to the dead backend. Redeploy re-registers with the new backend port.

## Lifecycle

### Redeploy & Rollback

On redeploy, Russel stops the old workload (kill + wait up to 5 s, then force-kill) before allocating ports for the new one. If the new deploy fails and a previous deployment existed, Russel attempts automatic rollback: the old service directory and metadata are restored, the previous VM or container is re-spawned, and a readiness check confirms the rollback before reporting `rolled_back`.

### Control Plane Shutdown

Stopping the control plane (`SIGINT`/`SIGTERM`) **does not** destroy running workloads — they are detached and keep running. On the next startup, the control plane removes only **orphan** TAP interfaces that have no corresponding live service directory (`/var/lib/russel/<id>/metadata.json`). Host iptables rules (Docker, VPN, admin) are never touched.

### Container Lifecycle

Containers are started only after podman arguments are fully validated and rootless mode is confirmed. For redeploys, the old container is stopped only after validation succeeds (fail-closed). Readiness requires the app to accept connections through the published host port while the container stays up (see above).

## Boot & Network Timing Optimization (Under 2s Boot)

To optimize boot time from 40s+ to under 2s, we transitioned from a heavy guest-side NixOS systemd VM to a direct microVM boot model:

- **Minimal Initramfs**: Dropped the Nix OS closure from guest RAM. The initramfs is only 2-3MB, consisting of BusyBox and necessary drivers.
- **Virtiofs /nix/store Share**: The host `/nix/store` is mounted directly inside the guest using `virtiofsd` and the `virtiofs` filesystem driver, allowing instant access to the application closure without packaging it in the initrd.
- **Drivers Built In**: Russel's kernel (`.#microvm-kernel`) compiles VirtIO networking and virtiofs in (`=y`), so the initramfs carries no kernel modules. Ctrl does not fall back to a stock nixpkgs kernel, which builds them as modules. Kernels supplied through `RUSSEL_KERNEL_PATH` or the kernel pool must also have these drivers built in.
- **Direct App Execution**: Bypasses systemd in the guest. The `/init` script executes the application binary directly, reducing guest-side lifecycle overhead to virtually zero.

## System Requirements

Russel currently runs the control plane on **Linux only**. The `russel-ctrl` binary needs the following tools available on `PATH`:

| Dependency | Used for | Required when |
|------------|----------|----------------|
| Nix with flakes enabled | Building application closures, the kernel, and BusyBox | Always |
| `cloud-hypervisor` (v52 or newer for `cpus` > 1 with passt) | Booting the microVM | MicroVM deploy |
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
│   ├── agent/        # russel-agent (node-local lifecycle RPC)
│   ├── cli/          # russel
│   ├── core/         # shared types & config
│   └── ctrl/         # control plane (main logic)
├── examples/         # see examples/README.md
│   ├── basic-http/   # Go + /health, type = container
│   ├── microvm-http/ # same app, type = microvm
│   ├── hello-rust/   # pure-std Rust HTTP
│   ├── env-config/   # [service.env] + secret://
│   ├── shortlink/    # in-memory URL shortener
│   ├── filebrowser/  # nixpkgs filebrowser wrapper
│   └── static-test/  # Python static site
├── nix/
│   └── microvm-kernel.nix  # compiled kernel (virtio drivers built-in)
├── docs/
│   ├── architecture.md    # Control plane internals
│   ├── auto-generation.md # Flake auto-detection
│   ├── deployment.md      # App packaging guide
│   ├── examples.md        # Example projects (microVM + container)
│   ├── russelfile.md      # Russelfile reference / design
│   ├── security-tls.md    # TLS reverse-proxy runbook
│   ├── traefik.md         # Traefik ingress setup
│   ├── vps-one-dev.md     # Single-VPS one-dev checklist
│   └── plans/             # MVP / scaling plans
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
- **virtiofsd --readonly** — `/nix/store` and the per-service `cfg/` dir (host-written `deploy.env`) are read-only from the guest. Guest writes `.agent_ready` to a separate `scratch/` share (`/run/russel` in the guest). Scratch has no quota; disk-fill is bounded to that directory.
- **No integration/e2e tests** — requires KVM + root. Marked `#[ignore]` candidate for a future e2e crate.
- **Auth optional on loopback** — dev mode warns but does not enforce. Production should always set `RUSSEL_API_TOKEN`.
- **Nix builds trust the source repo** — a malicious `flake.nix` runs as the build user. Multi-tenant: only deploy trusted repos. Opt-in `RUSSEL_NIX_RESTRICTED=1` forces sandboxed `nix build` and disables auto-flake. Full threat model: [docs/security/nix-builds.md](docs/security/nix-builds.md).

## Security

Report vulnerabilities privately, not in public issues. See [SECURITY.md](SECURITY.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, coding standards, and the mandatory full test/fmt/clippy checklist. Always run `cargo test --workspace` (and fmt + clippy) before opening a PR.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
