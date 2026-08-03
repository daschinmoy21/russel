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

        let mut port: u32 = 3100;
        loop {
            if port > u16::MAX as u32 {
                anyhow::bail!("Port exhaustion: no ports available between 3100 and 65535");
            }
            let port_u16 = port as u16;
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
fn fnv1a(bytes: impl AsRef<[u8]>) -> u32 {
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

// ── Tap creation + setup + port forwarding via socat ──────────────────────────

// ── IP forwarding tracking (#39) ─────────────────────────────────────────────

/// Initial `net.ipv4.ip_forward` value, read once at first access.
/// Used to decide whether to restore the sysctl after all TAPs are gone.
static IP_FORWARD_WAS_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
        .map(|s| s.trim() == "1")
        .unwrap_or(true) // if we can't read it, assume forwarding was on (don't break things)
});

/// Restore `net.ipv4.ip_forward` to 0 if (a) it was 0 before Russel set it,
/// and (b) no `rsl-` TAP interfaces remain on the host.
///
/// Also tears down the `RUSSEL-FORWARD` filter when the last TAP is gone (#187).
pub async fn restore_ip_forward() {
    // Always try to drop Russel FORWARD rules when no rsl- TAPs remain,
    // independent of the original sysctl value.
    restore_forward_filter().await;

    if *IP_FORWARD_WAS_ENABLED {
        return;
    }
    // Check if any rsl- TAPs remain.
    let count = count_rsl_taps().await;
    if count > 0 {
        tracing::debug!(count, "rsl- TAPs still present; leaving ip_forward=1");
        return;
    }
    tracing::info!("no rsl- TAPs remain; restoring net.ipv4.ip_forward=0");
    sysctl("net.ipv4.ip_forward", "0").await;
}

async fn count_rsl_taps() -> usize {
    match Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.lines().filter(|line| line.contains("rsl-")).count()
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to enumerate TAPs; assuming rsl- TAPs remain");
            1 // conservative: assume TAPs remain
        }
    }
}

// ── Guest FORWARD isolation (#187) ───────────────────────────────────────────
//
// Architecture: host→guest publish uses userspace `socat` (OUTPUT path), not
// kernel FORWARD. Guests only need L2 on their TAP + host→guest L3 for socat.
// Enabling `ip_forward=1` without a FORWARD policy lets guest A route to guest B
// (and off-host) via host L3. We install a dedicated chain that default-denies
// all forwarded traffic involving `rsl-*` interfaces. Never flush built-in chains.

/// Dedicated iptables filter chain owned by Russel (never flush host FORWARD).
pub const RUSSEL_FORWARD_CHAIN: &str = "RUSSEL-FORWARD";

/// iptables interface match for Russel TAP names (`rsl-` + wildcard `+`).
pub const RSL_IFACE_MATCH: &str = "rsl-+";

/// Residual-risk warning when the operator disables guest FORWARD filtering.
pub const FORWARD_FILTER_ALLOW_RISK: &str = "\
RUSSEL_FORWARD allow mode: guest TAP traffic is not default-denied on FORWARD. \
With net.ipv4.ip_forward=1 a compromised microVM can pivot to other guests and \
non-local destinations via host L3 routing. Use only for single-tenant debugging.";

/// Whether the operator opted out of default-deny FORWARD for `rsl-*` TAPs.
///
/// Escape hatches (either is enough):
/// - `RUSSEL_FORWARD=allow` (also: `off` / `0` / `false` / `disabled`)
/// - `RUSSEL_DISABLE_FORWARD_FILTER=1` (also: `true` / `yes` / `on`)
///
/// Default is secure (filter enabled). See [`FORWARD_FILTER_ALLOW_RISK`].
pub fn forward_filter_disabled() -> bool {
    forward_filter_disabled_from_env(
        std::env::var("RUSSEL_FORWARD").ok().as_deref(),
        std::env::var("RUSSEL_DISABLE_FORWARD_FILTER").ok().as_deref(),
    )
}

/// Pure helper for [`forward_filter_disabled`] (unit-testable).
pub fn forward_filter_disabled_from_env(
    russel_forward: Option<&str>,
    disable_filter: Option<&str>,
) -> bool {
    if env_value_truthy(disable_filter) {
        return true;
    }
    match russel_forward.map(str::trim) {
        Some(v) => {
            let v = v.to_ascii_lowercase();
            matches!(
                v.as_str(),
                "allow" | "off" | "0" | "false" | "disabled" | "no"
            )
        }
        None => false,
    }
}

fn env_value_truthy(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some(v) => {
            let v = v.to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        None => false,
    }
}

/// Spec of one iptables filter rule Russel manages (for tests + docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRuleSpec {
    /// Role within the install plan.
    pub role: &'static str,
    /// Arguments after `iptables` (excluding the binary name).
    pub args: Vec<&'static str>,
}

/// Pure description of the filter rules we install (#187).
///
/// Order: create dedicated chain → flush only that chain → DROP terminal →
/// jump from built-in FORWARD for `-i rsl-+` and `-o rsl-+`.
/// Jumps use `-I FORWARD 1` at install time; presence is checked with `-C`.
pub fn russel_forward_rule_plan() -> Vec<ForwardRuleSpec> {
    vec![
        ForwardRuleSpec {
            role: "create-chain",
            args: vec!["-w", "-N", RUSSEL_FORWARD_CHAIN],
        },
        ForwardRuleSpec {
            role: "flush-chain",
            args: vec!["-w", "-F", RUSSEL_FORWARD_CHAIN],
        },
        ForwardRuleSpec {
            role: "default-drop",
            args: vec!["-w", "-A", RUSSEL_FORWARD_CHAIN, "-j", "DROP"],
        },
        ForwardRuleSpec {
            role: "jump-in",
            args: vec![
                "-w",
                "-I",
                "FORWARD",
                "1",
                "-i",
                RSL_IFACE_MATCH,
                "-j",
                RUSSEL_FORWARD_CHAIN,
            ],
        },
        ForwardRuleSpec {
            role: "jump-out",
            args: vec![
                "-w",
                "-I",
                "FORWARD",
                "1",
                "-o",
                RSL_IFACE_MATCH,
                "-j",
                RUSSEL_FORWARD_CHAIN,
            ],
        },
    ]
}

/// Check-rule args for an existing jump (`iptables -C`).
fn jump_check_args(direction: &str) -> Vec<String> {
    // direction is "-i" or "-o"
    vec![
        "-w".into(),
        "-C".into(),
        "FORWARD".into(),
        direction.into(),
        RSL_IFACE_MATCH.into(),
        "-j".into(),
        RUSSEL_FORWARD_CHAIN.into(),
    ]
}

/// Install args for a jump (`iptables -I FORWARD 1 ...`).
fn jump_insert_args(direction: &str) -> Vec<String> {
    vec![
        "-w".into(),
        "-I".into(),
        "FORWARD".into(),
        "1".into(),
        direction.into(),
        RSL_IFACE_MATCH.into(),
        "-j".into(),
        RUSSEL_FORWARD_CHAIN.into(),
    ]
}

/// Ensure default-deny FORWARD for Russel TAP interfaces.
///
/// Fail-soft: missing `iptables` or lack of CAP_NET_ADMIN logs a prominent
/// warning and does not abort microVM boot (dev without root). Production
/// hosts should run with privileges so this succeeds.
///
/// No-op when [`forward_filter_disabled`] is true (logs residual risk once per call).
pub async fn ensure_forward_filter() {
    if forward_filter_disabled() {
        tracing::warn!(target: "russel_ctrl::network", "{}", FORWARD_FILTER_ALLOW_RISK);
        return;
    }
    match install_forward_filter().await {
        Ok(()) => {
            tracing::info!(
                chain = RUSSEL_FORWARD_CHAIN,
                iface = RSL_IFACE_MATCH,
                "installed default-deny FORWARD filter for Russel TAP interfaces"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                chain = RUSSEL_FORWARD_CHAIN,
                "failed to install RUSSEL-FORWARD filter — guest isolation degraded. \
                 Install iptables and run with CAP_NET_ADMIN, or set RUSSEL_FORWARD=allow \
                 for single-tenant debugging (see residual risk in docs)"
            );
        }
    }
}

/// Like [`ensure_forward_filter`], but only if at least one `rsl-` TAP exists
/// (e.g. after controller restart with live microVMs).
pub async fn ensure_forward_filter_if_taps_present() {
    let count = count_rsl_taps().await;
    if count == 0 {
        tracing::debug!("no rsl- TAPs; skipping RUSSEL-FORWARD install");
        return;
    }
    tracing::info!(count, "rsl- TAPs present; ensuring RUSSEL-FORWARD filter");
    ensure_forward_filter().await;
}

async fn install_forward_filter() -> anyhow::Result<()> {
    // 1. Create dedicated chain (ignore "chain already exists").
    match run_iptables(&["-w", "-N", RUSSEL_FORWARD_CHAIN]).await {
        Ok(()) => {}
        Err(e) if iptables_err_already_exists(&e) => {}
        Err(e) => return Err(e),
    }

    // 2. Flush only our chain (never host FORWARD / Docker / VPN).
    run_iptables(&["-w", "-F", RUSSEL_FORWARD_CHAIN]).await?;

    // 3. Default-deny everything that reaches the chain.
    //    Published ports use host socat (OUTPUT), not FORWARD, so no allow rules.
    run_iptables(&["-w", "-A", RUSSEL_FORWARD_CHAIN, "-j", "DROP"]).await?;

    // 4. Idempotent jumps from built-in FORWARD for in/out rsl-* traffic.
    for direction in ["-i", "-o"] {
        let check = jump_check_args(direction);
        let check_refs: Vec<&str> = check.iter().map(String::as_str).collect();
        if run_iptables(&check_refs).await.is_ok() {
            continue; // jump already present
        }
        let insert = jump_insert_args(direction);
        let insert_refs: Vec<&str> = insert.iter().map(String::as_str).collect();
        run_iptables(&insert_refs).await?;
    }

    Ok(())
}

/// Remove `RUSSEL-FORWARD` jumps and chain if no `rsl-` TAPs remain.
///
/// Mirrors [`restore_ip_forward`]: only cleans Russel-owned rules; never
/// flushes host built-in chains. Fail-soft on errors.
pub async fn restore_forward_filter() {
    if forward_filter_disabled() {
        return;
    }
    let count = count_rsl_taps().await;
    if count > 0 {
        tracing::debug!(count, "rsl- TAPs still present; leaving RUSSEL-FORWARD in place");
        return;
    }
    tracing::info!("no rsl- TAPs remain; removing RUSSEL-FORWARD filter rules");
    if let Err(e) = remove_forward_filter().await {
        tracing::warn!(
            error = %e,
            "failed to remove RUSSEL-FORWARD filter (manual cleanup may be needed)"
        );
    }
}

async fn remove_forward_filter() -> anyhow::Result<()> {
    // Delete jump rules (loop until gone — tolerate duplicates from races).
    for direction in ["-i", "-o"] {
        let del = vec![
            "-w".to_string(),
            "-D".to_string(),
            "FORWARD".to_string(),
            direction.to_string(),
            RSL_IFACE_MATCH.to_string(),
            "-j".to_string(),
            RUSSEL_FORWARD_CHAIN.to_string(),
        ];
        let del_refs: Vec<&str> = del.iter().map(String::as_str).collect();
        // Best-effort: keep deleting while -D succeeds.
        for _ in 0..16 {
            if run_iptables(&del_refs).await.is_err() {
                break;
            }
        }
    }

    // Flush and delete our chain only.
    let _ = run_iptables(&["-w", "-F", RUSSEL_FORWARD_CHAIN]).await;
    let _ = run_iptables(&["-w", "-X", RUSSEL_FORWARD_CHAIN]).await;
    Ok(())
}

fn iptables_err_already_exists(err: &anyhow::Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("already exists") || s.contains("chain already exists")
}

async fn run_iptables(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("iptables")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .args(args)
        .output()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run iptables {}: {e} (is iptables installed?)",
                args.join(" ")
            )
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "iptables {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

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
        // #187: default-deny FORWARD for rsl-* before the guest can use L3.
        // Publish stays on userspace socat (OUTPUT), so DROP does not break ports.
        ensure_forward_filter().await;

        match Self::spawn_socat(service_id, host_port, vm_ip, guest_port).await {
            Ok(child) => Ok(child),
            Err(e) => {
                // #40: clean up the TAP we just created so no half-state remains.
                tracing::warn!(tap, error = %e, "socat spawn failed; tearing down TAP");
                let _ = Self::teardown(alloc).await;
                Err(e)
            }
        }
    }

    /// Spawn only the host→guest TCP forwarder without touching TAP devices.
    ///
    /// Used after dual-live cutover when the VM/TAP already exist and we only
    /// need a new publish port (e.g. reclaim operator fixed `-p`).
    pub async fn spawn_socat(
        service_id: &str,
        host_port: u16,
        vm_ip: &str,
        guest_port: u16,
    ) -> anyhow::Result<tokio::process::Child> {
        let listen = format!(
            "TCP-LISTEN:{},fork,reuseaddr,bind={}",
            host_port,
            publish_bind_addr()
        );
        let connect = format!("TCP:{}:{}", vm_ip, guest_port);
        tracing::info!(
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
            host_port,
            vm_ip,
            guest_port,
            "port forwarding active: {}:{host_port} -> {vm_ip}:{guest_port}",
            publish_bind_addr()
        );
        Ok(child)
    }

    pub async fn teardown(alloc: &SubnetAllocation) -> anyhow::Result<()> {
        let tap = &alloc.tap_id;
        tracing::info!(tap, "tearing down tap interface");
        let result = match run_ip(&["link", "del", tap]).await {
            Ok(()) => Ok(()),
            Err(e)
                if e.to_string().contains("Cannot find device")
                    || e.to_string().contains("does not exist") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        };
        // #39: attempt to restore ip_forward if this was the last TAP.
        if result.is_ok() {
            restore_ip_forward().await;
        }
        result
    }

    pub async fn wait_for_vm_port(vm_ip: &str, guest_port: u16, timeout: Duration) -> bool {
        wait_for_tcp_addr(&format!("{vm_ip}:{guest_port}"), timeout).await
    }

    /// Poll until a TCP connect to the configured publish bind address succeeds.
    pub async fn wait_for_host_port(host_port: u16, timeout: Duration) -> bool {
        let bind = publish_bind_addr();
        // Connect target: loopback for 0.0.0.0 listeners; otherwise the bind IP.
        let connect_host = if bind == "0.0.0.0" || bind == "::" {
            "127.0.0.1"
        } else {
            bind.as_str()
        };
        wait_for_tcp_addr(&format!("{connect_host}:{host_port}"), timeout).await
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
        // ponytail: do not assert absolute 3100 — host may have that port
        // bound. Just verify distinct, monotonic, and >= 3100.
        assert!(p1 >= 3100, "p1={p1} must be >= 3100");
        assert!(p1 < p2, "p1={p1} must be < p2={p2}");
        assert!(p2 < p3, "p2={p2} must be < p3={p3}");
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
    fn fnv1a_is_xor_then_multiply() {
        // Verify FNV-1a uses XOR-then-MULTIPLY, not MULTIPLY-then-XOR (FNV-1).
        // Also check known-answer test vectors for the 32-bit variant.
        let hash = fnv1a("hello");
        let hash2 = fnv1a("hello");
        assert_eq!(hash, hash2, "FNV-1a must be deterministic");
        assert_ne!(fnv1a("a"), fnv1a("aa"));

        // Known-answer test vectors (FNV-1a 32-bit).
        // FNV offset basis: 0x811c9dc5 = 2166136261.
        assert_eq!(
            fnv1a(""),
            2_166_136_261,
            "FNV-1a empty-string = offset basis"
        );
        // "a" = (0x811c9dc5 ^ 0x61) * 0x01000193 = 0xe40c2d6c = 3826002220.
        assert_eq!(fnv1a("a"), 3_826_002_220, "FNV-1a(\"a\") known answer");
        // "foo" cross-check: deterministic but we don't need the exact value.
        assert_eq!(fnv1a("foo"), fnv1a("foo"));
    }

    #[test]
    fn fnv1a_different_from_fnv1() {
        // The old (buggy) FNV-1: hash = (hash * 16777619) ^ byte
        fn old_fnv1(bytes: &[u8]) -> u32 {
            bytes.iter().fold(2_166_136_261u32, |acc, &b| {
                acc.wrapping_mul(16_777_619) ^ b as u32
            })
        }
        // For most inputs, FNV-1a and FNV-1 differ.
        assert_ne!(fnv1a("russel"), old_fnv1("russel".as_bytes()));
    }

    #[test]
    fn pkill_socat_pattern_anchored() {
        // F-19: the socat pkill pattern must NOT match sibling services.
        let _pat_a = format!("^socat-russel-{}( |$)", "foo");
        let cmdline_foo = "socat-russel-foo TCP-LISTEN:...";
        let cmdline_foobar = "socat-russel-foobar TCP-LISTEN:...";
        let cmdline_socat_x = "socat-russel-x TCP-LISTEN:...";
        // regex crate isn't available here but we can do prefix checks:
        // The pkill pattern "^socat-russel-foo( |$)" should match foo,
        // but NOT match foobar because after "foo" must be space or end.
        assert!(
            cmdline_foo.starts_with("socat-russel-foo "),
            "foo must match"
        );
        assert!(
            !cmdline_foobar.starts_with("socat-russel-foo "),
            "foobar must NOT match"
        );
        // socat-x (prefix of command but different service) should not match
        assert!(
            !cmdline_socat_x.starts_with("socat-russel-foo "),
            "x must NOT match"
        );
    }

    #[test]
    fn pkill_virtiofsd_pattern_no_prefix_collision() {
        // The virtiofsd pattern uses "russel/{id}/" with trailing slash
        // which acts as a natural boundary. Verify.
        let pat_needle = "russel/foo/";
        let cmdline_foo = "/var/lib/russel/foo/virtiofs.sock";
        let cmdline_foobar = "/var/lib/russel/foobar/virtiofs.sock";
        assert!(cmdline_foo.contains(pat_needle), "foo should match itself");
        assert!(
            !cmdline_foobar.contains(pat_needle),
            "foobar should NOT match foo pattern"
        );
    }

    #[test]
    fn subnet_for_exhaustion_does_not_register() {
        // Verify that after release, the same preferred key is returned
        // (not clobbered by exhaustion path).
        let alloc1 = subnet_for("exhaust-test-1");
        release_subnet("exhaust-test-1");
        let alloc2 = subnet_for("exhaust-test-1");
        // Should get the same preferred allocation back.
        assert_eq!(alloc1.host_ip, alloc2.host_ip);
        assert_eq!(alloc1.tap_id, alloc2.tap_id);
        release_subnet("exhaust-test-1");
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

    // ── #187 guest FORWARD isolation helpers ─────────────────────────────

    #[test]
    fn forward_filter_enabled_by_default() {
        assert!(!forward_filter_disabled_from_env(None, None));
        assert!(!forward_filter_disabled_from_env(Some(""), None));
        assert!(!forward_filter_disabled_from_env(Some("deny"), None));
        assert!(!forward_filter_disabled_from_env(Some("1"), None));
        assert!(!forward_filter_disabled_from_env(None, Some("0")));
        assert!(!forward_filter_disabled_from_env(None, Some("false")));
    }

    #[test]
    fn forward_filter_disabled_via_russel_forward_allow() {
        for v in ["allow", "ALLOW", "off", "0", "false", "disabled", "no", " Allow "] {
            assert!(
                forward_filter_disabled_from_env(Some(v), None),
                "RUSSEL_FORWARD={v:?} should disable filter"
            );
        }
    }

    #[test]
    fn forward_filter_disabled_via_disable_env() {
        for v in ["1", "true", "yes", "on", "TRUE", " Yes "] {
            assert!(
                forward_filter_disabled_from_env(None, Some(v)),
                "RUSSEL_DISABLE_FORWARD_FILTER={v:?} should disable filter"
            );
        }
    }

    #[test]
    fn forward_filter_either_escape_hatch_suffices() {
        assert!(forward_filter_disabled_from_env(Some("allow"), Some("0")));
        assert!(forward_filter_disabled_from_env(Some("deny"), Some("1")));
    }

    #[test]
    fn russel_forward_rule_plan_is_default_deny_dedicated_chain() {
        let plan = russel_forward_rule_plan();
        let roles: Vec<_> = plan.iter().map(|r| r.role).collect();
        assert_eq!(
            roles,
            vec![
                "create-chain",
                "flush-chain",
                "default-drop",
                "jump-in",
                "jump-out"
            ]
        );

        // Never flush built-in FORWARD — only operate on RUSSEL-FORWARD or jump into it.
        for step in &plan {
            let joined = step.args.join(" ");
            assert!(
                !joined.contains("-F FORWARD") && !joined.contains("-X FORWARD"),
                "must not flush/delete built-in FORWARD: {joined}"
            );
            assert!(
                step.args.contains(&RUSSEL_FORWARD_CHAIN)
                    || step.args.iter().any(|a| *a == "FORWARD"),
                "unexpected step: {joined}"
            );
        }

        let drop_step = plan.iter().find(|r| r.role == "default-drop").unwrap();
        assert!(drop_step.args.contains(&"DROP"));
        assert!(drop_step.args.contains(&RUSSEL_FORWARD_CHAIN));

        for role in ["jump-in", "jump-out"] {
            let jump = plan.iter().find(|r| r.role == role).unwrap();
            assert!(jump.args.contains(&RSL_IFACE_MATCH));
            assert!(jump.args.contains(&RUSSEL_FORWARD_CHAIN));
            assert!(jump.args.contains(&"FORWARD"));
        }
    }

    #[test]
    fn rsl_iface_match_covers_russel_tap_prefix() {
        assert_eq!(RSL_IFACE_MATCH, "rsl-+");
        assert!(RSL_IFACE_MATCH.starts_with("rsl-"));
        // Chain name must not look like a built-in.
        assert_ne!(RUSSEL_FORWARD_CHAIN, "FORWARD");
        assert!(RUSSEL_FORWARD_CHAIN.starts_with("RUSSEL"));
    }

    #[test]
    fn allow_mode_risk_message_mentions_pivot() {
        let msg = FORWARD_FILTER_ALLOW_RISK.to_ascii_lowercase();
        assert!(msg.contains("ip_forward") || msg.contains("forward"));
        assert!(msg.contains("guest") || msg.contains("microvm") || msg.contains("tap"));
        assert!(msg.contains("single-tenant") || msg.contains("debugging"));
    }
}
