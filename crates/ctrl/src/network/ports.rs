use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};

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
    let bind = publish_bind_addr();
    std::net::TcpListener::bind((bind.as_str(), port)).is_ok()
}

/// Publish/bind address for socat and port checks.
/// Default `127.0.0.1` (safer). Set `RUSSEL_PUBLISH_BIND=0.0.0.0` for wildcard.
pub fn publish_bind_addr() -> String {
    static BIND: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        std::env::var("RUSSEL_PUBLISH_BIND")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    });
    BIND.clone()
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

        // Port 0 is never a fixed publish port (bind(0) is ephemeral).
        let mut port: u32 = 3100;
        loop {
            if port > u16::MAX as u32 {
                anyhow::bail!("Port exhaustion: no ports available between 3100 and 65535");
            }
            let port_u16 = port as u16;
            // Starts at 3100; try_bind / port_is_available already reject 0.
            if !registry.busy_ports.contains(&port_u16) && port_is_available(port_u16) {
                // #40: re-verify availability to narrow the TOCTOU window.
                if port_is_available(port_u16) {
                    registry.busy_ports.insert(port_u16);
                    registry
                        .allocations
                        .insert(service_id.to_string(), port_u16);
                    return Ok(port_u16);
                }
            }
            port += 1;
        }
    }

    pub fn reserve(service_id: &str, port: u16) -> anyhow::Result<()> {
        if port == 0 {
            anyhow::bail!("port 0 is not a fixed publish port (ephemeral bind is not supported)");
        }
        let bind = publish_bind_addr();
        let mut registry = port_registry();
        let existing_port = registry.allocations.get(service_id).copied();
        if existing_port == Some(port) {
            // Do not trust the registry alone: the listener may have disappeared,
            // or another process may have claimed the port since the last deploy.
            if !port_is_available(port) {
                anyhow::bail!("Port {port} is reserved but cannot be bound on {bind}");
            }
            return Ok(());
        }
        if registry.busy_ports.contains(&port) {
            anyhow::bail!("Port {port} is already reserved or in use");
        }
        if !port_is_available(port) {
            anyhow::bail!("Port {port} cannot be bound on {bind}");
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
        if port == 0 {
            anyhow::bail!("port 0 is not a fixed publish port (ephemeral bind is not supported)");
        }
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
