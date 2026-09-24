---
title: Runtimes
description: microVM vs container isolation, guest userspace, and hardened defaults.
sidebar_position: 2
keywords: [runtime, microvm, container, guest, busybox, podman, rootfs]
---

# Runtimes

`service.type` selects **isolation**. `service.guest` selects **userspace inside that isolation**. They are orthogonal — do not conflate them.

## At a glance

| `service.type` | Isolation | Host needs | Best for |
|---|---|---|---|
| `microvm` (default) | KVM / Cloud Hypervisor | `/dev/kvm`, TAP, `virtiofsd`, `socat`, `ip`, `iptables` | Stronger isolation, secret-heavy workloads |
| `container` | Rootless Podman `--rootfs` | Rootless Podman | Cheap no-KVM VPS, fastest spawn-to-ready |

| `service.guest` | What pid 1 / rootfs is | Status |
|---|---|---|
| `busybox` (default) | microVM: busybox `/init` execs one ELF. Container: hardened rootfs, no distro | Ships |
| `linux` | Host-built NixOS userspace (bash, coreutils, glibc, `$HOME`) | Parsed, then **rejected at load** until the boot path lands |

```toml
[service]
type = "container"  # or "microvm"
guest = "busybox"   # omit or busybox; linux errors until implemented
```

`--runtime` on the CLI must match `service.type` when both are set — it is a check, not an override.

## MicroVM (Cloud Hypervisor)

```mermaid
flowchart TB
    subgraph Host
        VFD1[virtiofsd<br/>/nix/store ro]
        VFD2[virtiofsd<br/>cfg dir ro]
        SCR[virtiofsd<br/>scratch rw → /run/russel]
        SOC[socat<br/>127.0.0.1:host_port]
        TAP[TAP rsl-key<br/>10.x.y.1/30]
        CH[cloud-hypervisor<br/>--api-socket --kernel --initramfs]
    end
    subgraph Guest["guest (no systemd)"]
        INIT[/init<br/>busybox/]
        MODS[insmod virtio chain<br/>if modules =m]
        APP[app binary<br/>from /nix/store]
    end
    SOC <-->|host_port ↔ 10.x.y.2:guest_port| TAP
    TAP <-->|virtio-net| CH
    VFD1 <-->|virtiofs nixstore| CH
    VFD2 <-->|virtiofs russelcfg| CH
    SCR <-->|virtiofs scratch| CH
    CH --> INIT --> MODS --> APP
    INIT -.->|reads /config/deploy.env| VFD2
```

- **Direct CH orchestration**, no `microvm.nix`/systemd in the guest. Kernel → busybox `/init` → app; cold boot ~2 s.
- **Virtiofs, not image packaging**: host `/nix/store` mounted read-only; app closure never copied.
- **Generic agent initramfs**: one cached CPIO for all services. Per-service config (`VM_IP`, `HOST_IP`, `PORT`, `APP`, user env) arrives via a second virtiofs share (`russelcfg`) as shell-quoted `deploy.env` (mounted read-only). Guest writes `.agent_ready` to a separate `scratch/` share (`/run/russel` in-guest, no quota — bounded to that dir).
- **Kernel**: prefer the repo's `microvm-kernel` flake attr (virtio/fuse built-in `=y`); fallback is stock nixpkgs kernel + `insmod` of the virtio chain in `/init`.
- **Readiness**: TCP poll of the guest IP across TAP (10 s), then a host-port check (2 s) to catch `socat` bind failures.
- **Lifecycle**: graceful stop via CH REST socket (`vm.shutdown` + `vmm.shutdown`), fallback to PID-ownership-verified signals, then pattern-anchored `pkill`. Destroy tears down TAP, `socat`, `virtiofsd`, ports, service dir.
- Each VM gets `--memory size=XM,shared=on` (shared required for virtiofs), 1 vCPU (Russelfile `cpus` validated `1..=32`), serial log at `/var/lib/russel/<id>/console.log`, and an API socket.

Warm pool (`RUSSEL_WARM_POOL=1`, snapshot/restore of a golden paused VM) is experimental and off by default.

## Container (rootless Podman `--rootfs`)

```mermaid
flowchart TB
    subgraph Host
        ROOTFS["/var/lib/russel/id/rootfs<br/>(read-only, no shell)"]
        STORE[/nix/store<br/>bind ro/]
        POD[podman run --rootfs<br/>rootless]
        C[container<br/>cap-drop ALL, no-new-privs,<br/>ro rootfs, tmpfs /tmp+/run]
    end
    TRAEFIK[Traefik / -p] -->|127.0.0.1:host_port| C
    POD --> C
    ROOTFS --> POD
    STORE --> C
```

- **`--rootfs`, not images.** Minimal Docker-like tree (etc files, `/bin/<app>` symlink into the closure) + host `/nix/store` bind-mounted read-only. No registry, no pulls, no Dockerfile.
- **Rootless only.** When ctrl runs as root (needed for microVM TAP/KVM), podman runs as `RUSSEL_PODMAN_USER` or `SUDO_USER` via `sudo -u <user> -H …`; rootless verified via `podman info`. Headless hosts may need `loginctl enable-linger $USER` for `/run/user/$(id -u)`.
- **Hardened defaults**: `--cap-drop ALL`, `--security-opt no-new-privileges`, `--read-only`, tmpfs `/tmp` + `/run`, `--memory` from the Russelfile. `debug = true` opts into bash/curl + `/usr/bin/env`.
- Entry points must be statically linked or use an absolute `/nix/store/…` interpreter (no shell, no `/usr/bin/env` by default). Do not pass a bare Nix package path as `--rootfs` yourself — use `russel deploy`.
- **Fail-closed redeploy**: old container stops only after the new podman argv validates. Readiness is a TCP poll of the published host port (10 s). Logs via `k8s-file` at `<service>/container.log`, fallback `podman logs`.
- Passthrough (`-- …`) uses an allowlist posture — see [First deploy](../getting-started/first-deploy.md).

> **Warning:** Container env (including resolved `secret://` values) is passed as `podman -e` and visible via `podman inspect`. Prefer microVMs for secret-heavy workloads.

## Choosing

- No `/dev/kvm` (most cheap VPS)? Use `type = "container"` everywhere.
- Need the strongest boundary or hide secrets from `podman inspect`? Use `type = "microvm"` on KVM-capable metal.
- Need a shell in the container for debugging? Set `debug = true` temporarily — do not ship it.

```bash
# MicroVM (default)
russel deploy examples/microvm-http -p 8080:3000 --vm-id api
# Container
russel deploy examples/basic-http -p 8080:3000 --vm-id api --runtime container
```

## Related

- [Architecture](architecture.md) · [Networking](networking.md) · [Russelfile](../reference/russelfile.md) · [Examples](../reference/examples.md)
