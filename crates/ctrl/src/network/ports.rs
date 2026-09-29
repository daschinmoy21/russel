use std::{
    collections::{HashMap, HashSet},
    net::TcpListener,
    sync::{LazyLock, Mutex},
};

#[cfg(test)]
use std::sync::OnceLock;

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

/// Process-wide lock for tests that mutate the global port registry / holds.
#[cfg(test)]
static PORT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialize tests that reserve fixed ports or inspect holds.
#[cfg(test)]
pub fn port_test_lock() -> std::sync::MutexGuard<'static, ()> {
    PORT_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Ports for tests that drop a hold and later expect to re-bind the same port.
///
/// Such tests race anything else that binds the port in between, so the range
/// avoids both busy sources: 3100+ is the allocator's own range (a dev ctrl,
/// rootless Podman publishes, and in-process `next` calls all land there), and
/// the kernel hands out `bind(0)` / `connect()` ports from its ephemeral range
/// (32768–60999 on Linux). Only fixed-port listeners live here, and the probe
/// in [`reserve_test_port`] skips those.
#[cfg(test)]
const TEST_PORTS: std::ops::Range<u16> = 20000..30000;

/// Reserve a free port from [`TEST_PORTS`] for `service_id` (with a hold).
///
/// The probe start is spread by pid and a per-process counter so concurrent
/// `cargo test` runs and successive tests do not reuse the same ports.
#[cfg(test)]
pub fn reserve_test_port(service_id: &str) -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let len = u32::from(TEST_PORTS.end - TEST_PORTS.start);
    let start = std::process::id()
        .wrapping_mul(7919)
        .wrapping_add(NEXT.fetch_add(101, Ordering::Relaxed));
    let mut last_err = None;
    for i in 0..len {
        let port = TEST_PORTS.start + (start.wrapping_add(i) % len) as u16;
        match PortAllocator::reserve(service_id, port) {
            Ok(()) => return port,
            Err(e) => last_err = Some(e),
        }
    }
    panic!("no free test port in {TEST_PORTS:?} for {service_id}: {last_err:?}");
}

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
        if port < 1024 {
            anyhow::bail!("ingress.port {port} is privileged (< 1024); Traefik owns 80/443");
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

    /// Free control-plane port ownership for `service_id`.
    ///
    /// Always drops any held `TcpListener` so the OS port is bindable again —
    /// including residual holds left after `take_hold` was never called (e.g.
    /// destroy before publish). Idempotent.
    ///
    /// The hold is removed from the registry under the lock, then dropped
    /// *after* the lock is released so OS close is not deferred behind other
    /// port-registry waiters.
    pub fn release(service_id: &str) {
        let hold = {
            let mut registry = port_registry();
            let hold = registry.holds.remove(service_id);
            if let Some(port) = registry.allocations.remove(service_id) {
                registry.busy_ports.remove(&port);
            }
            hold
        };
        // Close the listening socket outside the registry mutex.
        drop(hold);
    }

    /// Release the primary publish port and any extra `[[ports]]` keys
    /// (`{id}::xN`) for this service.
    pub fn release_service(service_id: &str) {
        let keys: Vec<String> = {
            let registry = port_registry();
            let prefix = format!("{service_id}::x");
            registry
                .allocations
                .keys()
                .filter(|k| *k == service_id || k.starts_with(&prefix))
                .cloned()
                .collect()
        };
        for key in keys {
            Self::release(&key);
        }
    }

    /// Port currently registered for `service_id`, if any.
    pub fn allocated_port(service_id: &str) -> Option<u16> {
        let registry = port_registry();
        registry.allocations.get(service_id).copied()
    }

    /// Whether a hold listener is still open for `service_id`.
    #[cfg(test)]
    pub fn has_hold(service_id: &str) -> bool {
        let registry = port_registry();
        registry.holds.contains_key(service_id)
    }

    /// Release the held listener so socat/podman can bind the port.
    ///
    /// Residual race: another process may grab the port between this drop and
    /// the publisher bind. Holding until here still closes the long deploy window.
    /// Does **not** clear allocation/`busy_ports` — call [`release`] for full free.
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
                // Live publisher already owns the port — drop any residual hold
                // left by a prior reserve so the publisher can (re)bind.
                drop(registry.holds.remove(service_id));
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
