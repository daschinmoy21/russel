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

// ── Tap setup + port forwarding via socat ─────────────────────────────────────

pub struct TapForwarder;

impl TapForwarder {
    /// Bring up the tap interface with host-side IP, then spawn a `socat`
    /// TCP forwarder:  `0.0.0.0:<host_port>` → `<vm_ip>:<guest_port>`.
    pub async fn setup(
        alloc: &SubnetAllocation,
        host_port: u16,
        guest_port: u16,
    ) -> anyhow::Result<tokio::process::Child> {
        let tap = &alloc.tap_id;
        let host_ip = &alloc.host_ip;
        let vm_ip = &alloc.vm_ip;

        // 1. Wait for cloud-hypervisor to create the tap.
        tracing::info!(tap, "waiting for tap interface");
        Self::wait_for_tap(tap, Duration::from_secs(15)).await?;
        
        // Give the interface a moment to settle after hypervisor attachment
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 2. Bring tap up and assign host-side IP.
        run_ip(&["link", "set", tap, "up"]).await?;
        // Use 'replace' instead of 'add' to ensure we own the IP and route
        run_ip(&["addr", "replace", &format!("{host_ip}/30"), "dev", tap]).await?;
        tracing::info!(tap, host_ip, "tap configured");

        // 3. Enable IP forwarding (needed for host → VM traffic via tap).
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

    /// Poll until guest_port is reachable at vm_ip.
    pub async fn wait_for_vm_port(vm_ip: &str, guest_port: u16, timeout: Duration) -> bool {
        let addr = format!("{vm_ip}:{guest_port}");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    async fn wait_for_tap(tap: &str, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let ok = Command::new("ip").args(["link", "show", tap])
                .output().await.map(|o| o.status.success()).unwrap_or(false);
            if ok { return Ok(()); }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("tap '{}' did not appear within timeout", tap)
    }
}

async fn sysctl(key: &str, val: &str) {
    let kv = format!("{key}={val}");
    let _ = Command::new("sysctl").args(["-w", &kv]).output().await;
}

async fn run_ip(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("ip").args(args).output().await?;
    if !out.status.success() {
        anyhow::bail!("ip {} failed: {}", args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
