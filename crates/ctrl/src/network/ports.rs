use std::{
    collections::{HashMap, HashSet},
    net::TcpListener,
    sync::{LazyLock, Mutex},
};

// ── Port allocator ────────────────────────────────────────────────────────────

struct PortRegistry {
    allocations: HashMap<String, u16>,
    busy_ports: HashSet<u16>,
    /// Bound listeners holding ports until the real publisher (socat/podman) binds.
    holds: HashMap<String, TcpListener>,
}

static PORT_REGISTRY: LazyLock<Mutex<PortRegistry>> = LazyLock::new(|| {
    Mutex::new(PortRegistry {
        allocations: HashMap::new(),
        busy_ports: HashSet::new(),
        holds: HashMap::new(),
    })
});

#[derive(Debug, Clone, Default)]
pub struct PortAllocator;

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

/// Bind `port` on the publish address and return the listener, or `None` if busy/invalid.
fn try_bind(port: u16) -> Option<TcpListener> {
    if port == 0 {
        return None;
    }
    let bind = publish_bind_addr();
    TcpListener::bind((bind.as_str(), port)).ok()
}

impl PortAllocator {
    pub fn next(&self, service_id: &str) -> anyhow::Result<u16> {
        let mut registry = port_registry();
        if let Some(old_port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&old_port);
            drop(registry.holds.remove(service_id));
        }

        // Port 0 is never a fixed publish port (bind(0) is ephemeral).
        let mut port: u32 = 3100;
        loop {
            if port > u16::MAX as u32 {
                anyhow::bail!("Port exhaustion: no ports available between 3100 and 65535");
            }
            let port_u16 = port as u16;
            // Starts at 3100; try_bind already rejects 0.
            if !registry.busy_ports.contains(&port_u16)
                && let Some(listener) = try_bind(port_u16)
            {
                registry.busy_ports.insert(port_u16);
                registry
                    .allocations
                    .insert(service_id.to_string(), port_u16);
                registry.holds.insert(service_id.to_string(), listener);
                return Ok(port_u16);
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
            if registry.holds.contains_key(service_id) {
                return Ok(());
            }
            // Registered without a hold (e.g. claim_existing). Hold if free;
            // if already bound by our publisher, keep the unheld claim.
            if let Some(listener) = try_bind(port) {
                registry.holds.insert(service_id.to_string(), listener);
            }
            return Ok(());
        }
        if registry.busy_ports.contains(&port) {
            anyhow::bail!("Port {port} is already reserved or in use");
        }
        let listener = try_bind(port)
            .ok_or_else(|| anyhow::anyhow!("Port {port} cannot be bound on {bind}"))?;
        if let Some(old_port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&old_port);
            drop(registry.holds.remove(service_id));
        }
        registry.busy_ports.insert(port);
        registry.allocations.insert(service_id.to_string(), port);
        registry.holds.insert(service_id.to_string(), listener);
        Ok(())
    }

    pub fn release(service_id: &str) {
        let mut registry = port_registry();
        if let Some(port) = registry.allocations.remove(service_id) {
            registry.busy_ports.remove(&port);
        }
        drop(registry.holds.remove(service_id));
    }

    /// Release the held listener so socat/podman can bind the port.
    ///
    /// Residual race: another process may grab the port between this drop and
    /// the publisher bind. Holding until here still closes the long deploy window.
    pub fn take_hold(service_id: &str) -> Option<TcpListener> {
        let mut registry = port_registry();
        registry.holds.remove(service_id)
    }

    /// Register a port that is already bound by a live container (e.g. after ctrl
    /// restart). Does NOT bind — the port is already in use.
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
            drop(registry.holds.remove(service_id));
        }
        registry.busy_ports.insert(port);
        registry.allocations.insert(service_id.to_string(), port);
        // No hold: port is already published by the live process.
        drop(registry.holds.remove(service_id));
        Ok(())
    }
}
