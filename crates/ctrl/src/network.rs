use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use tokio::process::Command;

// ── Port allocator ────────────────────────────────────────────────────────────

struct PortRegistry {
    allocations: HashMap<String, u16>,
    busy_ports: HashSet<u16>,
}

static PORT_REGISTRY: LazyLock<Mutex<PortRegistry>> = LazyLock::new(|| {
    Mutex::new(PortRegistry {
        allocations: HashMap::new(),
        busy_ports: HashSet::new(),
    })
});

#[derive(Debug, Clone, Default)]
pub struct PortAllocator;

fn port_is_available(port: u16) -> bool {
    // socat listens on the wildcard address, so check the same address here.
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

fn port_registry() -> std::sync::MutexGuard<'static, PortRegistry> {
    PORT_REGISTRY.lock().unwrap_or_else(|e| {
        tracing::warn!("PORT_REGISTRY lock poisoned — recovering");
        e.into_inner()
    })
}

impl PortAllocator {
    pub fn next(&self, service_id: &str) -> anyhow::Result<u16> {
        let mut registry = port_registry();
        if let Some(old_port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&old_port);
        }

        let mut port: u32 = 3100;
        loop {
            if port > u16::MAX as u32 {
                anyhow::bail!("Port exhaustion: no ports available between 3100 and 65535");
            }
            let port_u16 = port as u16;
            if !registry.busy_ports.contains(&port_u16) && port_is_available(port_u16) {
                registry.busy_ports.insert(port_u16);
                registry
                    .allocations
                    .insert(service_id.to_string(), port_u16);
                return Ok(port_u16);
            }
            port += 1;
        }
    }

    pub fn reserve(service_id: &str, port: u16) -> anyhow::Result<()> {
        let mut registry = port_registry();
        let existing_port = registry.allocations.get(service_id).copied();
        if existing_port == Some(port) {
            // Do not trust the registry alone: the listener may have disappeared,
            // or another process may have claimed the port since the last deploy.
            if !port_is_available(port) {
                anyhow::bail!("Port {} is reserved but cannot be bound on 0.0.0.0", port);
            }
            return Ok(());
        }
        if registry.busy_ports.contains(&port) {
            anyhow::bail!("Port {} is already reserved or in use", port);
        }
        if !port_is_available(port) {
            anyhow::bail!("Port {} cannot be bound on 0.0.0.0", port);
        }
        if let Some(old_port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&old_port);
        }
        registry.busy_ports.insert(port);
        registry.allocations.insert(service_id.to_string(), port);
        Ok(())
    }

    pub fn release(service_id: &str) {
        let mut registry = port_registry();
        if let Some(port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&port);
        }
    }

    /// Register a port that is already bound by a live container (e.g. after ctrl
    /// restart). Does NOT check port_is_available — the port is already in use.
    pub fn claim_existing(service_id: &str, port: u16) -> anyhow::Result<()> {
        let mut registry = port_registry();
        // Reject if a different service already owns this port.
        if let Some(other_id) = registry.allocations.iter().find_map(|(id, &p)| {
            if p == port && id != service_id {
                Some(id.clone())
            } else {
                None
            }
        }) {
            anyhow::bail!("port {port} already claimed by service '{other_id}'");
        }
        // If this service already has a different port, release it first.
        if let Some(&old_port) = registry.allocations.get(service_id) {
            if old_port == port {
                return Ok(());
            }
            registry.busy_ports.remove(&old_port);
        }
        registry.busy_ports.insert(port);
        registry.allocations.insert(service_id.to_string(), port);
        Ok(())
    }
}

// ── Subnet allocation (deterministic per service_id + collision registry) ────

#[derive(Debug, Clone)]
pub struct SubnetAllocation {
    pub host_ip: String,
    pub vm_ip: String,
    pub mac: String,
    pub tap_id: String,
}

struct SubnetRegistry {
    /// service_id → full 32-bit hash used for the /30.
    by_service: HashMap<String, u32>,
    /// hash → service_id (collision detection).
    by_hash: HashMap<u32, String>,
}

static SUBNET_REGISTRY: LazyLock<Mutex<SubnetRegistry>> = LazyLock::new(|| {
    Mutex::new(SubnetRegistry {
        by_service: HashMap::new(),
        by_hash: HashMap::new(),
    })
});

fn subnet_registry() -> std::sync::MutexGuard<'static, SubnetRegistry> {
    SUBNET_REGISTRY.lock().unwrap_or_else(|e| {
        tracing::warn!("SUBNET_REGISTRY lock poisoned — recovering");
        e.into_inner()
    })
}

fn fnv1a(bytes: impl AsRef<[u8]>) -> u32 {
    bytes.as_ref().iter().fold(2_166_136_261u32, |acc, &b| {
        acc.wrapping_mul(16_777_619) ^ b as u32
    })
}

fn allocation_from_hash(hash: u32) -> SubnetAllocation {
    let x = ((hash >> 8) & 0xFF) as u8;
    let y = (hash & 0xFF) as u8;
    SubnetAllocation {
        host_ip: format!("10.{x}.{y}.1"),
        vm_ip: format!("10.{x}.{y}.2"),
        mac: format!("02:00:00:00:{x:02x}:{y:02x}"),
        // Linux limits interface names to IFNAMSIZ - 1 (15) bytes.
        tap_id: format!("rsl-{hash:08x}"),
    }
}

/// Allocate a unique /30 for `service_id`.
///
/// Preferred hash is FNV-1a of the service id. On collision with another
/// service, rehash with a salt until a free slot is found (or bail).
pub fn subnet_for(service_id: &str) -> SubnetAllocation {
    let mut reg = subnet_registry();
    if let Some(&hash) = reg.by_service.get(service_id) {
        return allocation_from_hash(hash);
    }

    let mut hash = fnv1a(service_id.as_bytes());
    for attempt in 0u32..1024 {
        if let Some(owner) = reg.by_hash.get(&hash) {
            if owner == service_id {
                break;
            }
            // Collision — rehash with salt.
            hash = fnv1a(format!("{service_id}\0salt{attempt}").as_bytes());
            continue;
        }
        reg.by_hash.insert(hash, service_id.to_string());
        reg.by_service.insert(service_id.to_string(), hash);
        return allocation_from_hash(hash);
    }

    // Exhausted probes — fall back to preferred hash (best-effort; rare).
    tracing::error!(
        service_id,
        "subnet registry exhausted probes; using preferred hash (possible collision)"
    );
    allocation_from_hash(fnv1a(service_id.as_bytes()))
}

/// Release the subnet lease for a service so another can reuse the slot.
pub fn release_subnet(service_id: &str) {
    let mut reg = subnet_registry();
    if let Some(hash) = reg.by_service.remove(service_id) {
        reg.by_hash.remove(&hash);
    }
}

// ── Tap creation + setup + port forwarding via socat ──────────────────────────

pub struct TapForwarder;

impl TapForwarder {
    /// Create the TAP interface, bring it up with host-side IP, enable IP
    /// forwarding, then spawn a wildcard-bound socat TCP forwarder.
    pub async fn setup(
        service_id: &str,
        alloc: &SubnetAllocation,
        host_port: u16,
        guest_port: u16,
    ) -> anyhow::Result<tokio::process::Child> {
        let tap = &alloc.tap_id;
        let host_ip = &alloc.host_ip;
        let vm_ip = &alloc.vm_ip;

        tracing::info!(tap, "creating tap interface");
        let _ = run_ip(&["link", "del", tap]).await;
        run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"])
            .await
            .map_err(|e| anyhow::anyhow!("create TAP interface {tap}: {e}"))?;
        run_ip(&["link", "set", tap, "up"]).await?;
        run_ip(&["addr", "replace", &format!("{host_ip}/30"), "dev", tap]).await?;
        tracing::info!(tap, host_ip, "tap configured");

        sysctl("net.ipv4.ip_forward", "1").await;

        let listen = format!("TCP-LISTEN:{},fork,reuseaddr,bind=0.0.0.0", host_port);
        let connect = format!("TCP:{}:{}", vm_ip, guest_port);
        tracing::info!(
            tap,
            host_port,
            vm_ip,
            guest_port,
            "spawning socat: {listen} -> {connect}"
        );

        let child = Command::new("socat")
            .arg0(format!("socat-russel-{service_id}"))
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
            tap,
            host_port,
            vm_ip,
            guest_port,
            "port forwarding active: 0.0.0.0:{host_port} -> {vm_ip}:{guest_port}"
        );
        Ok(child)
    }

    pub async fn teardown(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        tracing::info!(tap, "tearing down tap interface");
        match run_ip(&["link", "del", tap]).await {
            Ok(()) => Ok(()),
            Err(e)
                if e.to_string().contains("Cannot find device")
                    || e.to_string().contains("does not exist") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn wait_for_vm_port(vm_ip: &str, guest_port: u16, timeout: Duration) -> bool {
        wait_for_tcp_addr(&format!("{vm_ip}:{guest_port}"), timeout).await
    }

    /// Poll until a TCP connect to `127.0.0.1:host_port` succeeds (container port publish).
    pub async fn wait_for_host_port(host_port: u16, timeout: Duration) -> bool {
        wait_for_tcp_addr(&format!("127.0.0.1:{host_port}"), timeout).await
    }
}

async fn wait_for_tcp_addr(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn sysctl(key: &str, val: &str) {
    let kv = format!("{key}={val}");
    match Command::new("sysctl").args(["-w", &kv]).output().await {
        Ok(out) if !out.status.success() => tracing::warn!(
            "sysctl {kv} exited with status {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!(error = %e, "failed to run sysctl {kv}"),
        _ => {}
    }
}

async fn run_ip(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("ip")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "ip {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
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
        release_subnet("my-service");
    }

    #[test]
    fn subnet_for_different_services_differ() {
        let a = subnet_for("service-a");
        let b = subnet_for("service-b");
        assert_ne!(a.host_ip, b.host_ip);
        release_subnet("service-a");
        release_subnet("service-b");
    }

    #[test]
    fn release_subnet_frees_lease() {
        let a = subnet_for("lease-svc");
        release_subnet("lease-svc");
        // After release, re-allocate should succeed with same preferred hash.
        let b = subnet_for("lease-svc");
        assert_eq!(a.tap_id, b.tap_id);
        release_subnet("lease-svc");
    }

    #[test]
    fn subnet_for_produces_valid_tap_id() {
        let a = subnet_for("a-service-id-that-is-much-longer-than-a-linux-interface-name");
        assert!(a.tap_id.starts_with("rsl-"));
        assert!(a.tap_id.len() <= 15);
        assert!(a.tap_id.bytes().all(|byte| byte.is_ascii_hexdigit()
            || byte == b'-'
            || byte == b'r'
            || byte == b's'
            || byte == b'l'));
    }

    #[test]
    fn subnet_for_produces_valid_mac() {
        assert!(subnet_for("bar").mac.starts_with("02:00:00:00:"));
        assert_eq!(subnet_for("bar").mac.len(), 17);
    }

    #[test]
    fn subnet_index_bounded() {
        for s in &["a", "b", "long-service-name-123", "edge", "max"] {
            let allocation = subnet_for(s);
            let parts: Vec<&str> = allocation.host_ip.split('.').collect();
            assert_eq!(parts.len(), 4);
            assert_eq!(parts[0], "10");
            assert_eq!(parts[3], "1");
            let x: u8 = parts[1].parse().unwrap();
            let y: u8 = parts[2].parse().unwrap();
            // Both octets are valid (full 16-bit space)
            let _ = (x, y);
        }
    }

    #[test]
    fn port_allocator_increments() {
        let alloc = PortAllocator;
        let p1 = alloc.next("service-1").unwrap();
        let p2 = alloc.next("service-2").unwrap();
        let p3 = alloc.next("service-3").unwrap();
        assert_eq!((p1, p2, p3), (3100, 3101, 3102));
        PortAllocator::release("service-1");
        PortAllocator::release("service-2");
        PortAllocator::release("service-3");
    }

    #[test]
    fn port_allocator_reserve_and_release() {
        PortAllocator::reserve("custom-service", 4000).unwrap();
        assert!(PortAllocator::reserve("another-service", 4000).is_err());
        PortAllocator::release("custom-service");
        PortAllocator::reserve("another-service", 4000).unwrap();
        PortAllocator::release("another-service");
    }

    #[test]
    fn port_allocator_claim_existing_registers_port() {
        PortAllocator::release("claimed-svc");
        PortAllocator::claim_existing("claimed-svc", 9000).unwrap();
        // Same service, same port is idempotent.
        PortAllocator::claim_existing("claimed-svc", 9000).unwrap();
        // Different service claiming same port is rejected.
        let err = PortAllocator::claim_existing("other-svc", 9000).unwrap_err();
        assert!(err.to_string().contains("already claimed"));
        // Same service with a different port moves the claim.
        PortAllocator::claim_existing("claimed-svc", 9001).unwrap();
        // Old port should now be available for another service.
        PortAllocator::release("claimed-svc");
        PortAllocator::claim_existing("other-svc", 9000).unwrap();
        PortAllocator::release("other-svc");
    }
}
