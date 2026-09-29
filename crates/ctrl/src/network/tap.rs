use std::time::{Duration, Instant};

use tokio::process::Command;

use super::forward::{ensure_forward_filter, restore_ip_forward, sysctl};
use super::ports::{PortAllocator, publish_bind_addr};
use super::subnet::SubnetAllocation;

pub struct TapForwarder;

impl TapForwarder {
    /// Create the TAP interface, bring it up with host-side IP, install the
    /// guest FORWARD filter, enable IP forwarding, then spawn a wildcard-bound
    /// socat TCP forwarder.
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

        // #187: install default-deny FORWARD for rsl-* *before* enabling
        // ip_forward so there is no window where L3 forwarding is on without
        // the filter (especially if host FORWARD already has ACCEPT jumps).
        // Publish stays on userspace socat (OUTPUT), so DROP does not break ports.
        ensure_forward_filter().await;
        sysctl("net.ipv4.ip_forward", "1").await;

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
        let listen = socat_listen_arg(&publish_bind_addr(), host_port);
        let connect = format!("TCP:{}:{}", vm_ip, guest_port);
        tracing::info!(
            host_port,
            vm_ip,
            guest_port,
            "spawning socat: {listen} -> {connect}"
        );

        // Free the held port immediately before socat binds.
        // Residual race: another process may grab the port between drop and bind.
        drop(PortAllocator::take_hold(service_id));

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

    /// Poll until the app behind the published host port accepts a
    /// connection. A bare connect is not enough: the port forwarder (pasta,
    /// rootlessport, socat) accepts whether or not the app listens (#462).
    pub async fn wait_for_host_port(host_port: u16, timeout: Duration) -> bool {
        let addr = Self::host_port_addr(host_port);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if super::app_accepts(&addr).await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Loopback (or bind-IP) dial address for a published host port.
    pub fn host_port_addr(host_port: u16) -> String {
        let bind = publish_bind_addr();
        tcp_dial_addr(host_port_probe_host(&bind), host_port)
    }
}

/// Strip one pair of surrounding brackets so `[::1]` is treated as `::1`.
fn unbracket_ip(addr: &str) -> &str {
    addr.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(addr)
}

/// Socat listen argument for the publish bind.
///
/// IPv6 literals need `TCP6-LISTEN` and a bracketed `bind=` or socat rejects
/// the address. IPv4 and hostnames stay on `TCP-LISTEN`.
fn socat_listen_arg(bind: &str, port: u16) -> String {
    let bind = unbracket_ip(bind);
    if bind.contains(':') {
        format!("TCP6-LISTEN:{port},fork,reuseaddr,bind=[{bind}]")
    } else {
        format!("TCP-LISTEN:{port},fork,reuseaddr,bind={bind}")
    }
}

/// TCP connect target. Bracket hosts that contain `:` so `TcpStream::connect`
/// can parse IPv6 (`::1:3100` is not a valid socket address).
fn tcp_dial_addr(host: &str, port: u16) -> String {
    let host = unbracket_ip(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Loopback probe host for a publish bind. `::` may not accept IPv4, so it
/// maps to `::1` rather than `127.0.0.1`.
fn host_port_probe_host(bind: &str) -> &str {
    match unbracket_ip(bind) {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        other => other,
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

pub(crate) async fn run_ip(args: &[&str]) -> anyhow::Result<()> {
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
    use super::{host_port_probe_host, socat_listen_arg, tcp_dial_addr};

    #[test]
    fn tcp_dial_addr_brackets_ipv6_only() {
        assert_eq!(tcp_dial_addr("::1", 3100), "[::1]:3100");
        assert_eq!(tcp_dial_addr("[::1]", 3100), "[::1]:3100");
        assert_eq!(tcp_dial_addr("127.0.0.1", 3100), "127.0.0.1:3100");
    }

    #[test]
    fn socat_listen_arg_uses_tcp6_for_ipv6_bind() {
        assert_eq!(
            socat_listen_arg("::1", 3100),
            "TCP6-LISTEN:3100,fork,reuseaddr,bind=[::1]"
        );
        assert_eq!(
            socat_listen_arg("[::1]", 3100),
            "TCP6-LISTEN:3100,fork,reuseaddr,bind=[::1]"
        );
        assert_eq!(
            socat_listen_arg("127.0.0.1", 3100),
            "TCP-LISTEN:3100,fork,reuseaddr,bind=127.0.0.1"
        );
    }

    #[test]
    fn host_port_probe_host_maps_wildcards() {
        assert_eq!(host_port_probe_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(host_port_probe_host("::"), "::1");
        assert_eq!(host_port_probe_host("[::]"), "::1");
        assert_eq!(host_port_probe_host("::1"), "::1");
        assert_eq!(host_port_probe_host("127.0.0.1"), "127.0.0.1");
    }
}
