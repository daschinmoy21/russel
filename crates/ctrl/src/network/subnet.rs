// ── Subnet allocation (deterministic per service_id + collision registry) ────

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[derive(Debug, Clone)]
pub struct SubnetAllocation {
    pub host_ip: String,
    pub vm_ip: String,
    pub mac: String,
    pub tap_id: String,
}
struct SubnetRegistry {
    /// service_id → 16-bit network key (lower 16 of FNV; drives IP/MAC/TAP).
    by_service: HashMap<String, u16>,
    /// network key → service_id (collision detection on actual identity).
    by_key: HashMap<u16, String>,
}

static SUBNET_REGISTRY: LazyLock<Mutex<SubnetRegistry>> = LazyLock::new(|| {
    Mutex::new(SubnetRegistry {
        by_service: HashMap::new(),
        by_key: HashMap::new(),
    })
});

fn subnet_registry() -> std::sync::MutexGuard<'static, SubnetRegistry> {
    SUBNET_REGISTRY.lock().unwrap_or_else(|e| {
        tracing::warn!("SUBNET_REGISTRY lock poisoned — recovering");
        e.into_inner()
    })
}

/// FNV-1a hash (xor-then-multiply, per the FNV-1a spec).
/// Note: the prior implementation was misnamed — it implemented FNV-1
/// (multiply-then-xor). TAP names for newly-deployed services will change.
pub(crate) fn fnv1a(bytes: impl AsRef<[u8]>) -> u32 {
    bytes.as_ref().iter().fold(2_166_136_261u32, |acc, &b| {
        (acc ^ b as u32).wrapping_mul(16_777_619)
    })
}

/// Truncate FNV to the 16-bit identity used for host_ip/vm_ip/mac/tap.
fn network_key(hash: u32) -> u16 {
    (hash & 0xFFFF) as u16
}

pub fn allocation_from_network_key(key: u16) -> SubnetAllocation {
    let x = ((key >> 8) & 0xFF) as u8;
    let y = (key & 0xFF) as u8;
    SubnetAllocation {
        host_ip: format!("10.{x}.{y}.1"),
        vm_ip: format!("10.{x}.{y}.2"),
        mac: format!("02:00:00:00:{x:02x}:{y:02x}"),
        // Linux limits interface names to IFNAMSIZ - 1 (15) bytes.
        // Zero-pad the 16-bit key to 8 hex so existing `rsl-` + 8-hex parsers match.
        tap_id: format!("rsl-{key:08x}"),
    }
}

/// Allocate a unique /30 for `service_id`.
///
/// Preferred key is the lower 16 bits of FNV-1a(service_id). On collision
/// with another service, rehash with a salt until a free 16-bit key is found.
pub fn subnet_for(service_id: &str) -> SubnetAllocation {
    let mut reg = subnet_registry();
    if let Some(&key) = reg.by_service.get(service_id) {
        return allocation_from_network_key(key);
    }

    let mut key = network_key(fnv1a(service_id.as_bytes()));
    for attempt in 0u32..1024 {
        if let Some(owner) = reg.by_key.get(&key) {
            if owner == service_id {
                break;
            }
            // Collision on the 16-bit network identity — rehash with salt.
            key = network_key(fnv1a(format!("{service_id}\0salt{attempt}").as_bytes()));
            continue;
        }
        reg.by_key.insert(key, service_id.to_string());
        reg.by_service.insert(service_id.to_string(), key);
        return allocation_from_network_key(key);
    }

    // Exhausted probes — do NOT clobber another service's mapping (F-45).
    // Return the preferred key without registering it; the allocation may
    // collide but the registry stays intact.
    tracing::error!(
        service_id,
        "subnet registry exhausted probes; using preferred key (possible collision, not registered)"
    );
    allocation_from_network_key(network_key(fnv1a(service_id.as_bytes())))
}

/// Release the subnet lease for a service so another can reuse the slot.
pub fn release_subnet(service_id: &str) {
    let mut reg = subnet_registry();
    if let Some(key) = reg.by_service.remove(service_id) {
        reg.by_key.remove(&key);
    }
}

/// Restore a previously allocated network key (e.g. after ctrl restart from
/// on-disk metadata). Idempotent for the same service_id; rejects conflicts.
pub fn claim_subnet_key(service_id: &str, key: u16) -> anyhow::Result<()> {
    let mut reg = subnet_registry();
    if let Some(owner) = reg.by_key.get(&key) {
        if owner != service_id {
            anyhow::bail!("subnet key {key:#06x} already claimed by {owner}");
        }
        reg.by_service.insert(service_id.to_string(), key);
        return Ok(());
    }
    if let Some(&old) = reg.by_service.get(service_id) {
        reg.by_key.remove(&old);
    }
    reg.by_key.insert(key, service_id.to_string());
    reg.by_service.insert(service_id.to_string(), key);
    Ok(())
}

/// Parse a 16-bit network key from `host_ip` like `10.x.y.1`.
pub fn network_key_from_host_ip(host_ip: &str) -> Option<u16> {
    let parts: Vec<&str> = host_ip.split('.').collect();
    if parts.len() != 4 || parts[0] != "10" || parts[3] != "1" {
        return None;
    }
    let x: u16 = parts[1].parse().ok()?;
    let y: u16 = parts[2].parse().ok()?;
    if x > 255 || y > 255 {
        return None;
    }
    Some((x << 8) | y)
}
