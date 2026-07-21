# Control Plane Architecture

The Russel control plane (`russel-ctrl`) orchestrates builds, **microVM and container**
lifecycles, and host-side networking.

---

## Ingress Trait

Deploy uses the `Ingress` trait (defined in `crates/ctrl/src/ingress.rs`) to advertise service backends to a reverse proxy. `TraefikFileIngress` (in `crates/ctrl/src/traefik.rs`) is the default implementation, writing Traefik dynamic configuration files. Future proxies (Caddy, Envoy, NGINX) implement the same trait — deploy, stop, and destroy never import Traefik types directly.

## Traefik Gateway

Russel integrates with [Traefik](https://traefik.io/) as the primary HTTP reverse proxy. The control plane writes dynamic configuration files (JSON) into a watched directory. Traefik picks up changes automatically — no reload signal needed.

### Flow

```
Client → Traefik (:80) → 127.0.0.1:<host_port> (socat/podman) → guest:<guest_port>
          ↑                        ↑
     Host(`svc.russel.local`)   dynamic file written by russel-ctrl
```

1. On deploy success, russel-ctrl writes `{service_id}.json` to the dynamic config directory.
2. Traefik's file provider watches the directory and applies the new router + service.
3. On stop/destroy, russel-ctrl removes the file; Traefik stops routing.

### Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `RUSSEL_TRAEFIK_DYNAMIC_DIR` | `/var/lib/russel/traefik/dynamic` | Directory for dynamic config files |
| `RUSSEL_TRAEFIK_DOMAIN` | `russel.local` | Domain suffix for Host rules |

### Router / Service Naming

- Router: `russel-{service_id}`
- Service: `russel-{service_id}`
- Rule: `Host(\`{service_id}.{domain}\`)`
- EntryPoints: `web`
- Backend: `http://127.0.0.1:{host_port}`

See [docs/traefik.md](traefik.md) for a complete static Traefik configuration example.

---

## Deployment Pipeline

Shared steps:

1. **Resolve**: Clone or locate the source repository, parse `Russelfile.toml` (including `service.type`).
   - Local repos must be absolute paths; remote repos restricted to `https://`, `http://`, `ssh://`, `git@host:path`.
   - Config file opened via `openat` + `O_NOFOLLOW` under repo root; 1 MiB size cap; binary name validated against `[A-Za-z0-9._+-]`.
2. **Build**: Auto-generate a `flake.nix` if none exists, then `nix build` → store path.

Then branch on `RuntimeKind` (`microvm` default, or `container`):

### MicroVM path

3. **Initramfs**: Minimal CPIO with BusyBox + VirtIO kernel modules.
4. **Network + Boot**: TAP, `socat` host→guest, `virtiofsd` for `/nix/store`, Cloud Hypervisor boot.
5. **Metadata**: `/var/lib/russel/<id>/metadata.json` (`schema_version`, `runtime: microvm`, ports, PIDs, …).

### Container path (Russel containers)

3. **Rootfs**: Docker-like tree under `/var/lib/russel/<id>/rootfs` (bash/curl, `/tmp`, `/var`, app symlinks).
4. **Start**: Rootless Podman `--rootfs` + bind-mount host `/nix/store:ro`, publish `-p HOST:GUEST`. Old container is stopped only **after** podman args are validated (fail-closed).
5. **Metadata**: same directory with `runtime: container`, `container_id`, `rootfs_path`, …

CLI `--runtime` must match Russelfile `type` when provided; the file is source of truth.

### Redeploy & Rollback

On redeploy, the pipeline:
1. Takes ownership of old child processes from the supervisor (`take_processes`).
2. Kills and waits for old children (up to 5 s, then force-kill) so ports are freed.
3. Renames existing service directories to `.bak` for rollback.
4. Tears down prior runtime resources (TAP, ports).
5. Proceeds with the new deploy.

If the new deploy fails and a backup exists, Russel attempts automatic rollback: `.bak` directories are restored, the previous VM or container is re-spawned, metadata is re-written, and a readiness check confirms the rollback before reporting `rolled_back`.

### Auth & Bind Policy

All API routes pass through a Bearer-auth middleware:
- When `RUSSEL_API_TOKEN` is set, every request must include `Authorization: Bearer <token>` (constant-time comparison).
- When unset on loopback, the control plane runs in dev mode with a warning.
- Binding to a non-loopback address **requires** `RUSSEL_API_TOKEN`; otherwise the control plane refuses to start.

### Control Plane Shutdown

On `SIGINT`/`SIGTERM`, the control plane:
1. Waits for in-flight deploy tasks to complete.
2. Detaches all workload child processes (they keep running).
3. On next startup, removes only **orphan** TAP interfaces (`rsl-<hex>`) that have no corresponding live service directory under `/var/lib/russel/<id>/metadata.json`. Host iptables chains are never flushed.

### Container Lifecycle

- Rootless Podman is verified (`podman info`) before any container operation.
- Old containers are stopped only after new args are fully validated.
- Readiness is confirmed by TCP-polling the published host port for up to 10 s.

---

## Cloud Hypervisor Integration

Initially, Russel planned to manage VMs via `microvm.nix` (a systemd-based NixOS microVM manager). We bypassed this layer in favor of **direct Cloud Hypervisor orchestration** to eliminate guest systemd overhead and achieve faster boot times.

Because we spawn the `cloud-hypervisor` binary directly in the Rust control plane, we have native access to its complete API.

### Current Command-Line Configuration

The VM process is spawned using the following key parameters:

```rust
Command::new("cloud-hypervisor")
    .arg("--kernel").arg(kernel_path)
    .arg("--initramfs").arg(initramfs_path)
    .arg("--cmdline").arg("console=ttyS0 panic=-1 random.trust_cpu=on")
    .arg("--cpus").arg("boot=1")
    .arg("--memory").arg(format!("size={}M,shared=on", mem_mb))
    .arg("--net").arg(format!("tap={},mac={}", tap, mac))
    .arg("--fs").arg(format!("tag=nixstore,socket={},num_queues=1,queue_size=512", virtiofs_sock))
    .arg("--console").arg("null")
    .arg("--serial").arg(format!("file=/var/lib/russel/{}/console.log", service_id))
```

| Parameter | Function | Why it is critical |
|-----------|----------|-------------------|
| `--kernel` & `--initramfs` | Direct boot | Boots the standard NixOS kernel and our custom Busybox initrd immediately, bypassing virtual BIOS/UEFI stages. |
| `--memory size=X,shared=on` | Shared memory | The `shared=on` attribute is required to permit the host's `virtiofsd` daemon to map memory directly into the guest address space. |
| `--fs` | Virtiofs share | Attaches the Unix socket of `virtiofsd`. Inside the guest, `/init` mounts this as a high-performance `virtiofs` filesystem at `/nix/store`. |
| `--net tap=X,mac=Y` | Bridged networking | Binds the guest directly to the host-provisioned TAP interface, creating `eth0` in the guest. |
| `--serial file=X` | Logging | Redirects console log outputs to `/var/lib/russel/<service-id>/console.log` for debugging and telemetry. |

---

## Future Cloud Hypervisor Features

Direct orchestration gives us access to Cloud Hypervisor capabilities that can be added as needed:

### 1. Dynamic VM Management (`--api-socket`)

By adding `--api-socket /var/lib/russel/<service-id>/api.sock`, Cloud Hypervisor exposes an HTTP REST API over a Unix Domain Socket for dynamic VM interaction:

- **Hotplug CPUs**: Add more virtual cores under heavy loads.
- **Hotplug Memory**: Scale RAM limits dynamically without restarting the VM.
- **Query Telemetry**: Retrieve virtual machine statistics (CPU usage, network packets, etc.).
- **Lifecycle Control**: Pause, resume, or cleanly shut down the guest.

### 2. High-Performance Storage (`--disk`)

For database services or stateful workloads, we can attach block devices via `--disk path=disk.img,readonly=off`.

### 3. Entropy Generation (`--rng`)

For cryptographic applications (HTTPS servers), `--rng` can attach a virtio-rng hardware random number generator device to the guest.
