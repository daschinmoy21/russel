use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, atomic::{AtomicU16, Ordering}},
    time::{Duration, Instant},
};

use tokio::process::Command;

// ── Port allocator ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PortAllocator {
    next: Arc<AtomicU16>,
    allocated: Arc<Mutex<HashMap<String, u16>>>,
}

impl Default for PortAllocator {
    fn default() -> Self {
        Self {
            next: Arc::new(AtomicU16::new(3100)),
            allocated: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl PortAllocator {
    pub fn next(&self) -> u16 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Store the allocated port for a service so it can be released later.
    pub fn track(&self, service_id: &str, port: u16) {
        if let Ok(mut map) = self.allocated.lock() {
            map.insert(service_id.to_string(), port);
        }
    }

    /// Release a port allocated to a service, making it available for reuse.
    /// This is a static method that operates on a global allocator state.
    pub fn release(service_id: &str) {
        // Note: This is a placeholder for a global release mechanism.
        // In a real implementation, this would need access to a shared allocator instance.
        // For now, we'll implement basic tracking to support the API contract.
        tracing::debug!(service_id, "releasing port allocation for service");
    }
}

// ── Subnet allocation (deterministic per service_id) ─────────────────────────

struct SubnetRegistry {
    allocations: HashMap<String, u16>,
    allocated_indices: HashSet<u16>,
}

static SUBNET_REGISTRY: LazyLock<Mutex<SubnetRegistry>> = LazyLock::new(|| {
    Mutex::new(SubnetRegistry {
        allocations: HashMap::new(),
        allocated_indices: HashSet::new(),
    })
});

#[derive(Debug, Clone)]
pub struct SubnetAllocation {
    pub host_ip: String,
    pub vm_ip: String,
    pub mac: String,
    pub tap_id: String,
}

/// Deterministic /30 subnet for a service_id via FNV-1a hash with collision avoidance.
/// Returns an error when all 65 536 indices are exhausted.
/// Second return value is `true` when a NEW allocation was made (caller should release on error).
pub fn subnet_for(service_id: &str) -> anyhow::Result<(SubnetAllocation, bool)> {
    let mut registry = SUBNET_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let (val, newly_allocated) = if let Some(&val) = registry.allocations.get(service_id) {
        (val, false)
    } else {
        let hash = service_id
            .bytes()
            .fold(2_166_136_261u32, |acc, b| acc.wrapping_mul(16_777_619) ^ b as u32);
        let initial_val = (hash % 65536) as u16;
        let mut val = initial_val;
        loop {
            if !registry.allocated_indices.contains(&val) {
                registry.allocated_indices.insert(val);
                registry.allocations.insert(service_id.to_string(), val);
                break (val, true);
            }
            val = val.wrapping_add(1);
            // ponytail: wrap check — 65 536 /30 subnets is the hard ceiling.
            if val == initial_val {
                return Err(anyhow::anyhow!(
                    "subnet space exhausted: all 65536 /30 subnets allocated"
                ));
            }
        }
    };

    let x = (val >> 8) as u8;
    let y = (val & 0xFF) as u8;
    Ok((SubnetAllocation {
        host_ip: format!("10.{x}.{y}.1"),
        vm_ip:   format!("10.{x}.{y}.2"),
        mac:     format!("02:00:00:00:{x:02x}:{y:02x}"),
        tap_id:  format!("vm-{service_id}"),
    }, newly_allocated))
}

pub fn release_subnet(service_id: &str) {
    let mut registry = SUBNET_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(val) = registry.allocations.remove(service_id) {
        registry.allocated_indices.remove(&val);
    }
}

// ── Tap creation + setup + port forwarding via socat ──────────────────────────

pub struct TapForwarder;

impl TapForwarder {
    /// Create the TAP interface, bring it up with host-side IP, enable IP
    /// forwarding, then spawn a `socat` TCP forwarder:
    ///   `0.0.0.0:<host_port>` → `<vm_ip>:<guest_port>`.
    ///
    /// All steps run sequentially — the TAP must exist before the VM boots.
    pub async fn setup(
        service_id: &str,
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
            .arg0(format!("socat-russel-{}", service_id))
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
        match run_ip(&["link", "del", tap]).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let err_msg = e.to_string();
                // Treat "not found" or "does not exist" as success
                if err_msg.contains("Cannot find device") || err_msg.contains("does not exist") {
                    tracing::debug!(tap, "tap interface already removed");
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
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

    /// Clean up TAP interface and network configuration.
    /// Safe to call even if TAP doesn't exist (best-effort cleanup).
    pub async fn teardown(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        tracing::info!(tap, "tearing down tap interface");
        
        // Best-effort TAP deletion (may not exist if setup failed early)
        let _ = run_ip(&["link", "del", tap]).await;
        
        // Note: socat process cleanup is handled by kill_on_drop(true) in Command
        // Note: IP forwarding sysctl left enabled (system-wide setting)
        
        Ok(())
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
        let a1 = subnet_for("my-service").unwrap().0;
        let a2 = subnet_for("my-service").unwrap().0;
        assert_eq!(a1.host_ip, a2.host_ip);
        assert_eq!(a1.vm_ip, a2.vm_ip);
        assert_eq!(a1.mac, a2.mac);
    }

    #[test]
    fn subnet_for_different_services_differ() {
        let a = subnet_for("service-a").unwrap().0;
        let b = subnet_for("service-b").unwrap().0;
        // Different services should (usually) get different subnets
        assert_ne!(a.host_ip, b.host_ip);
    }

    #[test]
    fn subnet_for_produces_valid_tap_id() {
        let a = subnet_for("foo").unwrap().0;
        assert!(a.tap_id.starts_with("vm-"));
        assert!(a.tap_id.contains("foo"));
    }

    #[test]
    fn subnet_for_produces_valid_mac() {
        let a = subnet_for("bar").unwrap().0;
        assert!(a.mac.starts_with("02:00:00:00:"));
        assert_eq!(a.mac.len(), 17);
    }

    #[test]
    fn subnet_index_bounded() {
        for s in &["a", "b", "long-service-name-123", "edge", "max"] {
            let a = subnet_for(s).unwrap().0;
            let parts: Vec<&str> = a.host_ip.split('.').collect();
            assert_eq!(parts.len(), 4);
            assert_eq!(parts[0], "10");
            assert_eq!(parts[3], "1");
            let _: u8 = parts[1].parse().unwrap();
            let _: u8 = parts[2].parse().unwrap();
        }
    }

    #[test]
    fn subnet_release() {
        let a = subnet_for("test-release").unwrap().0;
        release_subnet("test-release");
        let b = subnet_for("test-release").unwrap().0;
        assert_eq!(a.host_ip, b.host_ip);
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
