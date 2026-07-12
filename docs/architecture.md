# Control Plane Architecture

The Russel control plane (`russel-ctrl`) orchestrates builds, microVM lifecycles, and host-side networking.

---

## Deployment Pipeline

The core deployment pipeline follows these steps:

1. **Resolve**: Clone or locate the source repository, parse `Russelfile.toml`.
2. **Build**: Auto-generate a `flake.nix` if none exists, then build the application using Nix to produce a read-only store path.
3. **Initramfs**: Assemble a minimal CPIO initramfs containing BusyBox and the target kernel modules.
4. **Network + Boot**: Create the host TAP interface, spawn `socat` for TCP port forwarding, spawn `virtiofsd` to serve the host's `/nix/store`, then directly invoke `cloud-hypervisor` to boot the kernel and application.
5. **Traefik**: Register the backend with Traefik (placeholder — planned for multi-node routing).

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
