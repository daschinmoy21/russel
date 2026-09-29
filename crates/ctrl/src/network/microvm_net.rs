//! Per-VM networking for microVMs.
//!
//! Two modes, same guest view (`VM_IP/30`, default route via `HOST_IP`, the
//! app on `guest_port`, published on `publish_bind_addr():host_port`):
//!
//! - **tap**: a kernel TAP device, the RUSSEL-FORWARD filter, and a socat
//!   publish. Needs `CAP_NET_ADMIN`, so only a root (or capability-granted)
//!   ctrl can use it.
//! - **passt**: `passt --vhost-user` is the VM's NIC backend and publishes the
//!   port itself, the way rootless Podman uses pasta. Needs no privileges,
//!   so microVMs run under the same unprivileged ctrl as containers (#461).
//!
//! The mode is chosen per deploy (`RUSSEL_MICROVM_NET=tap|passt`, else tap
//! only when ctrl holds `CAP_NET_ADMIN`) and recorded in metadata as `net`, so
//! stop and destroy tear down what was actually set up.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::ports::{PortAllocator, publish_bind_addr};
use super::subnet::SubnetAllocation;
use super::tap::TapForwarder;
use russel_core::volumes::extra_port_key;

/// Metadata key recording the network mode of a microVM generation.
pub const NET_MODE_KEY: &str = "net";
/// Metadata key for the passt process id (passt mode's forwarder).
pub const PASST_PID_KEY: &str = "passt_pid";
/// Socket basename passt listens on inside the service dir.
pub const PASST_SOCKET: &str = "passt.sock";
/// Upper bound on waiting for a new microVM's app to accept connections.
/// Same budget as a container: the runtime must not decide whether a
/// slow-starting app deploys.
pub const MICROVM_READY_TIMEOUT: Duration = crate::container::CONTAINER_READY_TIMEOUT;
/// Written by the guest agent to its scratch share once the NIC is up.
const NET_READY_MARKER: &str = ".net_ready";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MicrovmNetMode {
    #[default]
    Tap,
    Passt,
}

impl MicrovmNetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::Passt => "passt",
        }
    }

    /// Mode recorded in metadata. Metadata from before #461 has none and
    /// always used a TAP.
    pub fn from_metadata(meta: &serde_json::Value) -> Self {
        match meta.get(NET_MODE_KEY).and_then(|v| v.as_str()) {
            Some("passt") => Self::Passt,
            _ => Self::Tap,
        }
    }

    /// The mode a new deploy uses on this host.
    pub fn for_host() -> anyhow::Result<Self> {
        mode_from(
            std::env::var("RUSSEL_MICROVM_NET").ok().as_deref(),
            crate::microvm::preflight::ctrl_has_net_admin(),
        )
    }
}

fn mode_from(env: Option<&str>, net_admin: bool) -> anyhow::Result<MicrovmNetMode> {
    match env.map(str::trim).filter(|s| !s.is_empty()) {
        Some("tap") => Ok(MicrovmNetMode::Tap),
        Some("passt") => Ok(MicrovmNetMode::Passt),
        Some(other) => {
            anyhow::bail!("RUSSEL_MICROVM_NET must be \"tap\" or \"passt\", got {other:?}")
        }
        None if net_admin => Ok(MicrovmNetMode::Tap),
        None => Ok(MicrovmNetMode::Passt),
    }
}

/// How Cloud Hypervisor attaches the VM's NIC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmNet {
    Tap(String),
    VhostUser(PathBuf),
}

impl VmNet {
    /// Value for `cloud-hypervisor --net`.
    pub fn ch_arg(&self, mac: &str) -> String {
        match self {
            Self::Tap(tap) => format!("tap={tap},mac={mac}"),
            Self::VhostUser(socket) => format!(
                "vhost_user=true,socket={},vhost_mode=client,mac={mac}",
                socket.display()
            ),
        }
    }
}

/// A VM's network, set up before boot.
pub struct MicrovmNet {
    pub mode: MicrovmNetMode,
    pub attach: VmNet,
    /// socat (tap) or passt: the process that publishes the host port.
    pub forwarder: tokio::process::Child,
    /// tap only: one more socat per `[[ports]]` row. passt publishes every
    /// port from its one process.
    pub extra_forwarders: Vec<tokio::process::Child>,
}

impl MicrovmNet {
    pub async fn setup(
        mode: MicrovmNetMode,
        service_id: &str,
        alloc: &SubnetAllocation,
        host_port: u16,
        guest_port: u16,
        extra_ports: &[(u16, u16)],
    ) -> anyhow::Result<Self> {
        match mode {
            MicrovmNetMode::Tap => {
                let forwarder =
                    TapForwarder::setup(service_id, alloc, host_port, guest_port).await?;
                let mut extra_forwarders = Vec::with_capacity(extra_ports.len());
                for (i, (host, guest)) in extra_ports.iter().enumerate() {
                    drop(PortAllocator::take_hold(&extra_port_key(service_id, i)));
                    match TapForwarder::spawn_socat(service_id, *host, &alloc.vm_ip, *guest).await {
                        Ok(child) => extra_forwarders.push(child),
                        Err(e) => {
                            let _ = TapForwarder::teardown(alloc).await;
                            return Err(e);
                        }
                    }
                }
                Ok(Self {
                    mode,
                    attach: VmNet::Tap(alloc.tap_id.clone()),
                    forwarder,
                    extra_forwarders,
                })
            }
            MicrovmNetMode::Passt => {
                // A marker left by a previous boot of this dir (rollback
                // restores it) would pass readiness before the guest is up.
                let marker = crate::paths::service_dir(service_id)
                    .join("scratch")
                    .join(NET_READY_MARKER);
                if let Err(e) = std::fs::remove_file(&marker)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    anyhow::bail!("remove stale {}: {e}", marker.display());
                }
                let socket = crate::paths::service_dir(service_id).join(PASST_SOCKET);
                let mut publishes = vec![(host_port, guest_port)];
                publishes.extend_from_slice(extra_ports);
                let forwarder = spawn_passt(service_id, &socket, alloc, &publishes).await?;
                Ok(Self {
                    mode,
                    attach: VmNet::VhostUser(socket),
                    forwarder,
                    extra_forwarders: Vec::new(),
                })
            }
        }
    }

    /// Wait until the app answers. On a TAP the guest IP is routable from the
    /// host; under passt it is not, so probe through the published port.
    ///
    /// Until the guest has configured its NIC, passt holds a probe
    /// connection open while its SYN goes unanswered, which looks like a
    /// listening app. So passt first waits for the agent's `.net_ready`
    /// marker in the scratch share; after that a closed port is reset at once.
    pub async fn wait_ready(
        mode: MicrovmNetMode,
        service_id: &str,
        alloc: &SubnetAllocation,
        host_port: u16,
        guest_port: u16,
        timeout: Duration,
    ) -> bool {
        match mode {
            MicrovmNetMode::Tap => {
                TapForwarder::wait_for_vm_port(&alloc.vm_ip, guest_port, timeout).await
            }
            MicrovmNetMode::Passt => {
                let deadline = Instant::now() + timeout;
                let marker = crate::paths::service_dir(service_id)
                    .join("scratch")
                    .join(NET_READY_MARKER);
                while !marker.exists() {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                TapForwarder::wait_for_host_port(host_port, left).await
            }
        }
    }

    /// Undo host-side network state. passt holds none besides its process
    /// and socket, which stop and the service dir removal take care of.
    pub async fn teardown(mode: MicrovmNetMode, alloc: &SubnetAllocation) -> anyhow::Result<()> {
        match mode {
            MicrovmNetMode::Tap => TapForwarder::teardown(alloc).await,
            MicrovmNetMode::Passt => Ok(()),
        }
    }

    /// Forwarder pid for the `socat_pid` metadata slot (tap only).
    pub fn socat_pid(&self) -> Option<u32> {
        match self.mode {
            MicrovmNetMode::Tap => self.forwarder.id(),
            MicrovmNetMode::Passt => None,
        }
    }

    /// TAP id for metadata (tap only).
    pub fn tap_id<'a>(&self, alloc: &'a SubnetAllocation) -> Option<&'a str> {
        match self.mode {
            MicrovmNetMode::Tap => Some(&alloc.tap_id),
            MicrovmNetMode::Passt => None,
        }
    }

    /// Record the mode (and the passt pid) in microVM metadata.
    pub fn record(&self, meta: &mut serde_json::Value) {
        meta[NET_MODE_KEY] = serde_json::json!(self.mode.as_str());
        if self.mode == MicrovmNetMode::Passt
            && let Some(pid) = self.forwarder.id()
        {
            meta[PASST_PID_KEY] = serde_json::json!(pid);
        }
    }
}

/// `passt` argv (after the program name) for one VM.
pub(crate) fn passt_args(
    socket: &Path,
    alloc: &SubnetAllocation,
    bind: &str,
    publishes: &[(u16, u16)],
) -> Vec<String> {
    let bind = bind
        .strip_prefix('[')
        .and_then(|b| b.strip_suffix(']'))
        .unwrap_or(bind);
    let mut args = vec![
        "--vhost-user".to_string(),
        "--foreground".to_string(),
        "--quiet".to_string(),
        "--socket".to_string(),
        socket.display().to_string(),
        // The guest configures itself statically (agent init); no DHCP/NDP.
        "--address".to_string(),
        alloc.vm_ip.clone(),
        "--netmask".to_string(),
        "30".to_string(),
        "--gateway".to_string(),
        alloc.host_ip.clone(),
        "--no-dhcp".to_string(),
        "--no-dhcpv6".to_string(),
        "--no-ndp".to_string(),
        "--no-ra".to_string(),
        // Without this the gateway address maps to the host's loopback, and
        // the guest could reach ctrl's API on 127.0.0.1 (pasta in rootless
        // Podman runs with the same flag).
        "--no-map-gw".to_string(),
        "--udp-ports".to_string(),
        "none".to_string(),
    ];
    if !bind.contains(':') {
        args.push("--ipv4-only".to_string());
    }
    for (host, guest) in publishes {
        args.push("--tcp-ports".to_string());
        args.push(match bind {
            "0.0.0.0" | "::" => format!("{host}:{guest}"),
            addr => format!("{addr}/{host}:{guest}"),
        });
    }
    args
}

async fn spawn_passt(
    service_id: &str,
    socket: &Path,
    alloc: &SubnetAllocation,
    publishes: &[(u16, u16)],
) -> anyhow::Result<tokio::process::Child> {
    crate::paths::check_unix_socket_path(socket)?;
    if let Some(dir) = socket.parent() {
        crate::microvm::ensure_private_dir(dir)?;
    }
    if let Err(e) = std::fs::remove_file(socket)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        anyhow::bail!("remove stale passt socket {}: {e}", socket.display());
    }
    let args = passt_args(socket, alloc, &publish_bind_addr(), publishes);
    tracing::info!(service_id, args = ?args, "spawning passt");

    // Free the held publish ports immediately before passt binds them.
    drop(PortAllocator::take_hold(service_id));
    for i in 1..publishes.len() {
        drop(PortAllocator::take_hold(&extra_port_key(service_id, i - 1)));
    }

    let mut child = Command::new("passt")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn passt: {e}. Unprivileged microVMs need passt \
                 (nixpkgs#passt, or the distro's passt package)"
            )
        })?;

    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() {
        if let Ok(Some(status)) = child.try_wait() {
            let stderr = read_exited_stderr(&mut child).await;
            let detail = if stderr.is_empty() {
                format!(
                    "is host port {} free?",
                    publishes.first().map(|p| p.0).unwrap_or_default()
                )
            } else {
                stderr
            };
            anyhow::bail!(
                "passt exited with {status} before creating {} ({detail})",
                socket.display(),
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!("passt did not create {} within 2s", socket.display());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Keep draining stderr for passt's lifetime so a full pipe never blocks it.
    if let Some(stderr) = child.stderr.take() {
        let service_id = service_id.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(service_id, "passt: {line}");
            }
        });
    }
    Ok(child)
}

/// Stderr of a passt that already exited, trimmed; empty if unreadable.
async fn read_exited_stderr(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::new();
    // passt has exited, so EOF is immediate unless it left a child holding
    // the pipe; don't let that hang the deploy.
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn alloc() -> SubnetAllocation {
        SubnetAllocation {
            host_ip: "10.200.3.1".into(),
            vm_ip: "10.200.3.2".into(),
            mac: "02:00:00:00:03:02".into(),
            tap_id: "rsl-0003".into(),
        }
    }

    #[test]
    fn mode_prefers_tap_only_with_net_admin() {
        assert_eq!(mode_from(None, true).unwrap(), MicrovmNetMode::Tap);
        assert_eq!(mode_from(None, false).unwrap(), MicrovmNetMode::Passt);
        assert_eq!(
            mode_from(Some("passt"), true).unwrap(),
            MicrovmNetMode::Passt
        );
        assert_eq!(mode_from(Some("tap"), false).unwrap(), MicrovmNetMode::Tap);
        assert_eq!(mode_from(Some(" "), false).unwrap(), MicrovmNetMode::Passt);
        assert!(mode_from(Some("slirp"), false).is_err());
    }

    #[test]
    fn mode_from_metadata_defaults_to_tap() {
        let legacy = serde_json::json!({"tap_id": "rsl-0003"});
        assert_eq!(MicrovmNetMode::from_metadata(&legacy), MicrovmNetMode::Tap);
        let passt = serde_json::json!({"net": "passt"});
        assert_eq!(MicrovmNetMode::from_metadata(&passt), MicrovmNetMode::Passt);
    }

    #[test]
    fn ch_arg_per_attachment() {
        assert_eq!(
            VmNet::Tap("rsl-0003".into()).ch_arg("02:00:00:00:03:02"),
            "tap=rsl-0003,mac=02:00:00:00:03:02"
        );
        assert_eq!(
            VmNet::VhostUser(PathBuf::from("/var/lib/russel/api/passt.sock"))
                .ch_arg("02:00:00:00:03:02"),
            "vhost_user=true,socket=/var/lib/russel/api/passt.sock,vhost_mode=client,mac=02:00:00:00:03:02"
        );
    }

    #[test]
    fn passt_args_match_guest_addressing_and_publish() {
        let args = passt_args(
            Path::new("/d/api/passt.sock"),
            &alloc(),
            "127.0.0.1",
            &[(3101, 3000)],
        );
        let joined = args.join(" ");
        assert!(joined.contains("--vhost-user"), "{joined}");
        assert!(joined.contains("--socket /d/api/passt.sock"), "{joined}");
        assert!(joined.contains("--address 10.200.3.2 --netmask 30 --gateway 10.200.3.1"));
        assert!(joined.contains("--no-map-gw"), "{joined}");
        assert!(joined.contains("--ipv4-only"), "{joined}");
        assert!(
            joined.ends_with("--tcp-ports 127.0.0.1/3101:3000"),
            "{joined}"
        );
    }

    #[test]
    fn passt_args_wildcard_and_ipv6_binds() {
        let any = passt_args(Path::new("/s"), &alloc(), "0.0.0.0", &[(8080, 80)]);
        assert!(any.ends_with(&["--tcp-ports".to_string(), "8080:80".to_string()]));
        let v6 = passt_args(Path::new("/s"), &alloc(), "[::1]", &[(8080, 80)]);
        assert!(!v6.contains(&"--ipv4-only".to_string()));
        assert!(v6.ends_with(&["--tcp-ports".to_string(), "::1/8080:80".to_string()]));
    }
}
