use std::{
    sync::{Arc, atomic::{AtomicU16, Ordering}},
    time::{Duration, Instant},
};

use tokio::process::Command;

// ── Port allocator ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PortAllocator {
    next: Arc<AtomicU16>,
}

impl Default for PortAllocator {
    fn default() -> Self {
        Self { next: Arc::new(AtomicU16::new(3100)) }
    }
}

impl PortAllocator {
    pub fn next(&self) -> u16 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    pub fn release(_service_id: &str) {}
}

// ── Subnet allocation (deterministic per service_id) ─────────────────────────

#[derive(Debug, Clone)]
pub struct SubnetAllocation {
    pub host_ip: String,
    pub vm_ip: String,
    pub mac: String,
    pub tap_id: String,
}

/// Deterministic /30 subnet for a service_id via FNV-1a hash.
pub fn subnet_for(service_id: &str) -> SubnetAllocation {
    let hash = service_id
        .bytes()
        .fold(2_166_136_261u32, |acc, b| acc.wrapping_mul(16_777_619) ^ b as u32);
    let idx = (hash % 200) as u8;
    SubnetAllocation {
        host_ip: format!("10.0.{idx}.1"),
        vm_ip:   format!("10.0.{idx}.2"),
        mac:     format!("02:00:00:00:{idx:02x}:01"),
        tap_id:  format!("vm-{service_id}"),
    }
}

pub fn release_subnet(_service_id: &str) {}

// ── Tap creation + setup + port forwarding via socat ──────────────────────────

pub struct TapForwarder;

impl TapForwarder {
    /// Create the TAP interface, bring it up with host-side IP, enable IP
    /// forwarding, then spawn a `socat` TCP forwarder:
    ///   `0.0.0.0:<host_port>` → `<vm_ip>:<guest_port>`.
    ///
    /// All steps run sequentially — the TAP must exist before the VM boots.
    pub async fn setup(
        alloc: &SubnetAllocation,
        host_port: u16,
        guest_port: u16,
    ) -> anyhow::Result<tokio::process::Child> {
        let tap = &alloc.tap_id;
        let host_ip = &alloc.host_ip;
        let vm_ip = &alloc.vm_ip;

        // 1. Create TAP (destroy first if leftover from a previous run).
        tracing::info!(tap, "creating tap interface");
        let _ = run_ip(&["link", "del", tap]).await;                // best-effort
        let _ = run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"]).await;

        // 2. Bring TAP up and assign host-side IP.
        run_ip(&["link", "set", tap, "up"]).await?;
        run_ip(&["addr", "replace", &format!("{host_ip}/30"), "dev", tap]).await?;
        tracing::info!(tap, host_ip, "tap configured");

        // 3. Enable IP forwarding (needed for host → VM traffic via TAP).
        sysctl("net.ipv4.ip_forward", "1").await;

        // 4. Spawn socat: listens on 0.0.0.0:<host_port>, forwards to VM.
        let listen = format!("TCP-LISTEN:{},fork,reuseaddr,bind=0.0.0.0", host_port);
        let connect = format!("TCP:{}:{}", vm_ip, guest_port);

        tracing::info!(
            tap, host_port, vm_ip, guest_port,
            "spawning socat: {listen} -> {connect}"
        );

        let child = Command::new("socat")
            .arg(&listen)
            .arg(&connect)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to spawn socat: {}. Install with: nix-env -iA nixpkgs.socat",
                    e
                )
            })?;

        tracing::info!(
            tap, host_port, vm_ip, guest_port,
            "port forwarding active: 0.0.0.0:{host_port} -> {vm_ip}:{guest_port}"
        );

        Ok(child)
    }

    /// Delete the TAP interface when the VM is destroyed.
    pub async fn teardown(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        tracing::info!(tap, "tearing down tap interface");
        let _ = run_ip(&["link", "del", tap]).await; // best-effort
        Ok(())
    }

    /// Poll until guest_port is reachable at vm_ip.
    pub async fn wait_for_vm_port(vm_ip: &str, guest_port: u16, timeout: Duration) -> bool {
        let addr = format!("{vm_ip}:{guest_port}");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

async fn sysctl(key: &str, val: &str) {
    let kv = format!("{key}={val}");
    match Command::new("sysctl").args(["-w", &kv]).output().await {
        Ok(out) if !out.status.success() => {
            tracing::warn!(
                "sysctl {kv} exited with status {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to run sysctl {kv}");
        }
        _ => {}
    }
}

async fn run_ip(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("ip").args(args).output().await?;
    if !out.status.success() {
        anyhow::bail!("ip {} failed: {}", args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_for_is_deterministic() {
        let a1 = subnet_for("my-service");
        let a2 = subnet_for("my-service");
        assert_eq!(a1.host_ip, a2.host_ip);
        assert_eq!(a1.vm_ip, a2.vm_ip);
        assert_eq!(a1.mac, a2.mac);
    }

    #[test]
    fn subnet_for_different_services_differ() {
        let a = subnet_for("service-a");
        let b = subnet_for("service-b");
        // Different services should (usually) get different subnets
        assert_ne!(a.host_ip, b.host_ip);
    }

    #[test]
    fn subnet_for_produces_valid_tap_id() {
        let a = subnet_for("foo");
        assert!(a.tap_id.starts_with("vm-"));
        assert!(a.tap_id.contains("foo"));
    }

    #[test]
    fn subnet_for_produces_valid_mac() {
        let a = subnet_for("bar");
        assert!(a.mac.starts_with("02:00:00:00:"));
        // MAC format: 02:00:00:00:XX:01 (6 octets = 17 chars)
        assert_eq!(a.mac.len(), 17);
    }

    #[test]
    fn subnet_index_bounded() {
        for s in &["a", "b", "long-service-name-123", "edge", "max"] {
            let a = subnet_for(s);
            let idx = a.host_ip
                .trim_start_matches("10.0.")
                .trim_end_matches(".1");
            let n: u8 = idx.parse().unwrap();
            assert!(n < 200, "index {} out of range for service {}", n, s);
        }
    }

    #[test]
    fn port_allocator_increments() {
        let alloc = PortAllocator::default();
        let p1 = alloc.next();
        let p2 = alloc.next();
        let p3 = alloc.next();
        assert_eq!(p1, 3100);
        assert_eq!(p2, 3101);
        assert_eq!(p3, 3102);
    }
}
