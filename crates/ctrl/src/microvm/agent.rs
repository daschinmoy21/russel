//! Agent init script and initramfs constants (config-driven guest, no app baked in).

use std::path::Path;

/// Basename for the agent initramfs CPIO file. Bump when `AGENT_INIT_SCRIPT` changes
/// so stale disk caches cannot serve an old init.
pub(super) const AGENT_INITRAMFS_BASENAME: &str = "agent-initramfs-v11.cpio";

/// Busybox applets symlinked into the agent initramfs.
pub(super) const AGENT_BUSYBOX_APPLETS: &[&str] = &[
    "sh", "mount", "ip", "mkdir", "insmod", "xzcat", "cat", "sleep", "usleep", "ls", "poweroff",
    "chpst", "kill",
];

pub(super) const AGENT_INIT_SCRIPT: &str = r#"#!/bin/sh
/bin/mkdir -p /proc /sys /dev /nix/store /config /tmp /run/russel
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs devtmpfs /dev
# /run is a tmpfs like a container's (#469). Mount it before the scratch share
# at /run/russel, which it would otherwise hide.
/bin/mount -t tmpfs -o mode=755,nosuid,nodev,noexec tmpfs /run
/bin/mkdir -p /run/russel

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

# [[volumes]]: one virtiofs share per line, "<tag> <ro|rw> <guest path>"
# (#386). A volume that does not mount must not let the app write its data
# into guest RAM instead, so stop here and let the deploy fail.
if [ -f /config/mounts ]; then
  while read -r tag mode path; do
    [ -n "$tag" ] || continue
    /bin/mkdir -p "$path"
    opts=""
    [ "$mode" = "ro" ] && opts="-o ro"
    i=0
    mounted=0
    while [ $i -lt 30 ]; do
      if /bin/mount -t virtiofs $opts "$tag" "$path"; then
        mounted=1
        break
      fi
      i=$((i + 1))
      /bin/usleep 5000
    done
    if [ "$mounted" -ne 1 ]; then
      echo "ERROR: volume $tag did not mount at $path; not starting the app"
      exec /bin/sh
    fi
    echo "volume $tag mounted at $path ($mode)"
  done < /config/mounts
fi

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

# Tell the host the NIC is up. Under passt the host probes through the
# published port, and before this point a SYN is not answered at all, which
# a probe cannot tell from a listening app (#461).
echo "ready" > /run/russel/.net_ready

export PORT
cd /

# Run the app unprivileged (#466) unless the Russelfile says user = "root":
# /config/user holds "uid gid". /etc/passwd names that user for chpst and for
# apps that look themselves up (postgres does), so write it before / goes
# read-only. Ports below 1024 stay bindable, as in a container.
RUN_AS=""
if [ -f /config/user ]; then
  read -r APP_UID APP_GID < /config/user
  /bin/mkdir -p /etc
  echo "root:x:0:0:root:/root:/bin/sh" > /etc/passwd
  echo "app:x:$APP_UID:$APP_GID:app:/tmp:/bin/sh" >> /etc/passwd
  echo "root:x:0:" > /etc/group
  echo "app:x:$APP_GID:" >> /etc/group
  echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start
  RUN_AS="/bin/chpst -u app:app"
fi

# Same filesystem contract as a container (#469): read-only root, tmpfs /tmp
# (and /run above), persistent writes only through [[volumes]]. An app that
# writes elsewhere fails the same way on both runtimes, and a root that stays
# writable would let data vanish on redeploy, so do not start the app then.
# /dev/shm is Podman's 64 MiB tmpfs; POSIX shared memory needs it (postgres
# does not start without it).
if ! /bin/mount -t tmpfs -o mode=1777,nosuid,nodev,noexec tmpfs /tmp \
  || ! /bin/mkdir -p /dev/shm \
  || ! /bin/mount -t tmpfs -o mode=1777,nosuid,nodev,noexec,size=64m tmpfs /dev/shm \
  || ! /bin/mount -o remount,ro /; then
  echo "ERROR: could not set up tmpfs /tmp and /dev/shm and a read-only root; not starting the app"
  exec /bin/sh
fi

# service.args, one entry per line, into "$@" verbatim: no eval, no word
# splitting, no globbing (#463).
set --
if [ -f /config/argv ]; then
  while IFS= read -r arg; do
    set -- "$@" "$arg"
  done < /config/argv
fi
# Run the app as a child rather than exec it as pid 1. When pid 1 exits the
# guest panics and reboots, and the reboot truncates the serial log, so a
# crash would erase its own output. Log the exit status and power off
# instead: cloud-hypervisor exits and ctrl sees the VM go down.
echo "starting $APP ($# args)"
$RUN_AS "$APP" "$@" &
APP_PID=$!
# When a deploy retires this generation, the host writes /config/stop (#562).
# Pass it on as SIGTERM so the app can finish the requests it is serving; the
# host kills the VM if it is still up after its grace period.
(
  while [ ! -f /config/stop ]; do
    /bin/usleep 200000
  done
  echo "stop requested; sending SIGTERM to the app"
  /bin/kill -TERM "$APP_PID"
) &
wait "$APP_PID"
echo "app exited with status $?; powering off"
/bin/poweroff -f
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
        // Root-owned in the guest whoever packed it: the app may run as the
        // ctrl's uid (#466) and must not own /, /etc, or /init.
        .arg("-R")
        .arg("0:0")
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
