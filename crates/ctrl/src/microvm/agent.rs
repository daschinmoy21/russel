//! Agent init script and initramfs constants (config-driven guest, no app baked in).

use std::path::Path;

/// Basename for the agent initramfs CPIO file. Bump when `AGENT_INIT_SCRIPT` changes
/// so stale disk caches cannot serve an old init.
pub(super) const AGENT_INITRAMFS_BASENAME: &str = "agent-initramfs-v4.cpio";

/// Busybox applets symlinked into the agent initramfs.
pub(super) const AGENT_BUSYBOX_APPLETS: &[&str] = &[
    "sh", "mount", "ip", "mkdir", "insmod", "xzcat", "cat", "sleep", "usleep", "ls",
];

pub(super) const AGENT_INIT_SCRIPT: &str = r#"#!/bin/sh
/bin/mkdir -p /proc /sys /dev /nix/store /config /tmp /run/russel
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs devtmpfs /dev

# Load virtio/fuse modules if present (stock kernel fallback).
# Ordered list matches legacy per-service init, xzcat+insmod.
if [ -d /modules ] && ls /modules/*.ko.xz >/dev/null 2>&1; then
  echo "Loading kernel modules from /modules..."
  for mod in /modules/virtio_ring.ko.xz /modules/virtio.ko.xz \
             /modules/virtio_pci_modern_dev.ko.xz /modules/virtio_pci_legacy_dev.ko.xz \
             /modules/virtio_pci.ko.xz \
             /modules/failover.ko.xz /modules/net_failover.ko.xz \
             /modules/virtio_net.ko.xz \
             /modules/fuse.ko.xz /modules/virtiofs.ko.xz; do
    [ -f "$mod" ] || continue
    ko="/tmp/${mod##*/}"
    ko="${ko%.xz}"
    if /bin/xzcat "$mod" > "$ko" 2>/dev/null && /bin/insmod "$ko" 2>/dev/null; then
      true
    else
      echo "WARN: failed to load ${mod##*/}"
    fi
  done
fi

# Mount host store (read-only). Virtio devices can lag a few hundred ms.
echo "Mounting /nix/store via virtiofs..."
i=0
mounted=0
while [ $i -lt 30 ]; do
  if /bin/mount -t virtiofs nixstore /nix/store; then
    echo "nixstore mounted"
    mounted=1
    break
  fi
  i=$((i + 1))
  /bin/usleep 5000
done
if [ "$mounted" -ne 1 ]; then
  echo "ERROR: Failed to mount /nix/store via virtiofs after retries"
  /bin/ls -l /sys/bus/virtio/devices 2>/dev/null || true
  exec /bin/sh
fi

# Mount config (read-only) for host-written deploy.env.
echo "Mounting /config via virtiofs..."
i=0
mounted=0
while [ $i -lt 30 ]; do
  if /bin/mount -t virtiofs -o ro russelcfg /config; then
    echo "russelcfg mounted"
    mounted=1
    break
  fi
  i=$((i + 1))
  /bin/usleep 5000
done
if [ "$mounted" -ne 1 ]; then
  echo "ERROR: Failed to mount /config via virtiofs after retries"
  exec /bin/sh
fi

# Mount scratch (read-write) for the readiness marker only.
echo "Mounting /run/russel via virtiofs..."
i=0
mounted=0
while [ $i -lt 30 ]; do
  if /bin/mount -t virtiofs russelscratch /run/russel; then
    echo "russelscratch mounted"
    mounted=1
    break
  fi
  i=$((i + 1))
  /bin/usleep 5000
done
if [ "$mounted" -ne 1 ]; then
  echo "ERROR: Failed to mount /run/russel via virtiofs after retries"
  exec /bin/sh
fi

# Signal readiness to host (scratch only — cfg is read-only).
echo "ready" > /run/russel/.agent_ready

# Wait for deploy.env to be written by the host.
echo "Waiting for /config/deploy.env..."
while [ ! -f /config/deploy.env ]; do
  /bin/usleep 10000
done

# Source deployment config.
set -a
. /config/deploy.env
set +a

# Find network interface (net.ifnames=0 friendly).
IFACE="eth0"
if ! /bin/ip link show eth0 >/dev/null 2>&1; then
  for iface in /sys/class/net/*; do
    ifname="${iface##*/}"
    if [ "$ifname" != "lo" ]; then
      IFACE="$ifname"
      break
    fi
  done
fi

echo "Configuring $IFACE: ip=$VM_IP gw=$HOST_IP port=$PORT"
/bin/ip addr add $VM_IP/30 dev $IFACE
/bin/ip link set $IFACE up
/bin/ip route add default via $HOST_IP

export PORT
cd /
echo "exec $APP"
exec "$APP"
echo "ERROR: exec failed! Spawning emergency shell..."
exec /bin/sh
"#;

/// Pack `work` into a newc CPIO at `initramfs_file` using busybox multi-call
/// `find` + `cpio`. Both processes share `current_dir(work)` so relative paths
/// from find resolve correctly for cpio. Used by agent initramfs builds.
pub(super) fn pack_cpio_blocking(
    work: &Path,
    initramfs_file: &Path,
    bb_bin: &str,
) -> anyhow::Result<()> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let out_file = std::fs::File::create(initramfs_file)?;

    let mut find = std::process::Command::new(bb_bin)
        .arg("find")
        .arg(".")
        .current_dir(work)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn busybox find: {e}"))?;

    let find_stdout = find
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture find stdout"))?;

    // cpio must run with the same cwd as find: find emits paths like `./bin/sh`
    // which cpio opens relative to its cwd. Also use spawn+wait — not
    // Command::output() — so stdout stays the archive file (output() forces a pipe).
    let cpio = std::process::Command::new(bb_bin)
        .arg("cpio")
        .arg("-o")
        .arg("-H")
        .arg("newc")
        .current_dir(work)
        .stdin(find_stdout)
        .stdout(out_file)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn busybox cpio: {e}"))?;

    // Wait consumer first so the pipe drains; then the producer.
    let cpio_out = cpio
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("wait cpio: {e}"))?;
    let find_status = find.wait().map_err(|e| anyhow::anyhow!("wait find: {e}"))?;

    if !cpio_out.status.success() {
        anyhow::bail!(
            "cpio failed: exit {:?} — {}",
            cpio_out.status.code(),
            String::from_utf8_lossy(&cpio_out.stderr).trim()
        );
    }
    if !find_status.success() {
        anyhow::bail!(
            "find failed: exit code {:?} signal {:?}",
            find_status.code(),
            find_status.signal()
        );
    }

    let meta = std::fs::metadata(initramfs_file)?;
    if meta.len() == 0 {
        anyhow::bail!(
            "cpio produced empty initramfs at {}",
            initramfs_file.display()
        );
    }
    Ok(())
}
