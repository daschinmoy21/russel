//! `russel-ctrl` binary — thin entry point over the `russel_ctrl` library.

use anyhow::Result;
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use russel_ctrl::state::AppState;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_timer(tracing_subscriber::fmt::time::uptime())
                .with_target(false),
        )
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,russel_ctrl=debug")),
        )
        .init();

    // Single-instance guard (F-30): /var/lib/russel state (service dirs, port
    // registry, TAP names) is not safe for concurrent controllers. Held for
    // the lifetime of main; process exit releases the flock.
    let _instance_lock = acquire_instance_lock()?;

    // Detect Nix system triple once at startup — cached for all builds/deploys.
    russel_ctrl::build::init_current_system().await?;
    // Remove only Russel-owned stale TAP interfaces from previous sessions.
    // Never flush host-global iptables chains (Docker/VPN/admin rules).
    cleanup_stale_resources().await;
    // #187: re-install default-deny FORWARD for any live rsl-* TAPs after restart.
    russel_ctrl::network::ensure_forward_filter_if_taps_present().await;
    // Hybrid privileges: microVM uses this process (often root/sudo for TAP/KVM);
    // containers use rootless podman as RUSSEL_PODMAN_USER or SUDO_USER.
    russel_ctrl::container::log_podman_identity();

    // Build state early so reconcile can rehydrate services from disk
    // before the router starts serving requests.
    let state = AppState::default();

    // Rehydrate observed service state from on-disk metadata so status
    // endpoints work without waiting for GET /vms lazy discovery.
    let report = russel_ctrl::reconcile::reconcile_startup(&state).await;
    tracing::info!(?report, "startup reconcile complete");

    // Start warm pool prepare in the background so the first deploy after
    // ctrl restart can restore from a paused snapshot instead of cold booting.
    // Failures are logged but never block the control plane from serving.
    tokio::spawn(async move {
        let pool = russel_ctrl::warm_pool::shared_warm_pool();
        if let Err(e) = pool.prepare().await {
            tracing::warn!(
                error = %e,
                "warm pool prepare failed — cold boot will be used for deploys"
            );
        }
    });

    // Same AppState that reconcile filled — do not re-default.
    // All subsequent consumers (health loop, router, wait_for_deploys,
    // detach_all_processes) share this Arc so rehydrated services are visible.

    // Periodic TCP health probes; set RUSSEL_HEALTH_RESTART=1 to redeploy
    // after 3 consecutive failures when metadata records repo_url.
    russel_ctrl::health::spawn_health_loop(state.clone());

    let app: Router = russel_ctrl::api::router(state.clone());
    let bind_addr = std::env::var("RUSSEL_CTRL_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".into());

    // Bind first, then decide auth from the *actual* bound address (F-36).
    // Resolving via to_socket_addrs() and binding separately can disagree when
    // the host has multiple A/AAAA records (loopback first, non-loopback bind).
    let listener = TcpListener::bind(&bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let is_loopback = ip_is_loopback_for_auth(local_addr.ip());

    // Auth + bind policy:
    // - RUSSEL_API_TOKEN: non-empty after trim enables Bearer auth; must be
    //   ≥ MIN_API_TOKEN_LEN (32) chars. Empty/whitespace = unset.
    // - RUSSEL_REQUIRE_AUTH=1|true|yes: fail closed without a valid token even
    //   on loopback (production packaging).
    // - Non-loopback bind always requires a valid token.
    let token = match russel_ctrl::api::configured_api_token() {
        Some(t) => {
            if let Err(msg) = russel_ctrl::api::check_api_token_min_length(&t) {
                anyhow::bail!("{msg}");
            }
            Some(t)
        }
        None => None,
    };
    let require_auth = russel_ctrl::api::require_auth_from_env(
        std::env::var("RUSSEL_REQUIRE_AUTH").ok().as_deref(),
    );
    if token.is_some() {
        info!("RUSSEL_API_TOKEN set — requiring Bearer auth on all routes");
    } else if require_auth {
        anyhow::bail!(
            "RUSSEL_REQUIRE_AUTH is set but RUSSEL_API_TOKEN is missing or blank; \
             set a token of at least {} characters (e.g. openssl rand -hex 32)",
            russel_ctrl::api::MIN_API_TOKEN_LEN
        );
    } else if is_loopback {
        // F-35: loopback is not an isolation boundary — any local user and
        // any SSH/Docker port-forward into the host reaches this socket.
        tracing::warn!(
            "dev mode: no auth — any local user or forwarded port can control the API; \
             set RUSSEL_API_TOKEN (min {} chars) or RUSSEL_REQUIRE_AUTH=1",
            russel_ctrl::api::MIN_API_TOKEN_LEN
        );
    } else {
        anyhow::bail!(
            "RUSSEL_API_TOKEN must be set when binding to non-loopback address '{}' (bound {}); \
             use at least {} characters (e.g. openssl rand -hex 32)",
            bind_addr,
            local_addr,
            russel_ctrl::api::MIN_API_TOKEN_LEN
        );
    }

    info!("russel control plane listening on {}", local_addr);
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    // Wait for in-flight deploy tasks to complete before detaching.
    // Deploy tasks may still be creating VMs; if we detach now, they'd
    // register handles after detach and get SIGKILLed on drop.
    state.wait_for_deploys().await;

    // Workload teardown is an explicit admin action (`russel destroy` /
    // DELETE /vm/{id}). Controller restart must not take the fleet down.
    let detached = state.detach_all_processes();
    info!(
        detached,
        "control plane stopped; left workloads running \
         (restart the control plane, then use `russel vms` / `russel destroy <id>` to manage them)"
    );

    serve_result?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let sigterm = signal(SignalKind::terminate());

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down control plane...");
            }
            _ = async {
                if let Ok(mut sigterm) = sigterm {
                    sigterm.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::info!("received SIGTERM, shutting down control plane...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received SIGINT, shutting down control plane...");
    }
}

/// F-30: single-instance guard. All russel-ctrl state (service directories,
/// port registry, TAP name allocation) under /var/lib/russel assumes one
/// writer, so refuse to start a second controller.
///
/// Opens /var/lib/russel/ctrl.lock and takes an exclusive non-blocking
/// `flock`. The returned fd must be held for the process lifetime; exiting
/// releases the lock automatically, so no unlock path is needed.
fn acquire_instance_lock() -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, OwnedFd};

    const LOCK_PATH: &str = "/var/lib/russel/ctrl.lock";
    std::fs::create_dir_all("/var/lib/russel")?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Never written to — the fd exists only to carry the flock.
        .truncate(false)
        .open(LOCK_PATH)?;
    // Safety: `file` is a valid open fd; flock does not retain it beyond the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // Only EWOULDBLOCK/EAGAIN means another holder; surface other errno
        // (permissions, NFS, interrupted) with their real cause.
        if err.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::bail!("another russel-ctrl instance is running (lock: {LOCK_PATH})");
        }
        return Err(anyhow::Error::new(err)
            .context(format!("failed to acquire exclusive lock on {LOCK_PATH}")));
    }
    Ok(OwnedFd::from(file))
}

/// F-36: true when the *bound* IP is loopback (used for auth policy).
///
/// Uses `to_canonical()` so IPv4-mapped IPv6 loopback (`::ffff:127.0.0.1`)
/// is treated as loopback the same as `127.0.0.1` / `::1`.
fn ip_is_loopback_for_auth(ip: std::net::IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Clean up stale Russel-owned resources from previous controller sessions.
///
/// Intentionally does **not** flush host-global iptables chains (`OUTPUT`,
/// `POSTROUTING`, `FORWARD`). Those belong to Docker, VPN, and the host admin.
/// When Russel needs firewall rules it must install dedicated `RUSSEL_*` chains
/// and only remove those.
///
/// Only deletes TAP interfaces with no corresponding live service directory
/// (`/var/lib/russel/<id>/metadata.json`).
async fn cleanup_stale_resources() {
    use tokio::process::Command;

    // Collect all expected TAP ids from live service directories.
    let live_taps = live_service_tap_ids();

    // Remove stale TAP interfaces owned by Russel (`rsl-<8 hex chars>`).
    // See `network::subnet_for` for the naming scheme.
    match Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let Some(raw) = line.split_whitespace().nth(1) else {
                    continue;
                };
                // `ip -o link show` names look like `rsl-a1b2c3d4@NONE:` — strip
                // trailing colon and optional `@peer` suffix.
                let base = raw.trim_end_matches(':').split('@').next().unwrap_or(raw);
                if !is_russel_tap(base) {
                    continue;
                }
                // Only delete if no live service maps to this TAP.
                if live_taps.contains(base) {
                    tracing::debug!(tap = base, "keeping TAP — live service exists");
                    continue;
                }
                match Command::new("ip")
                    .args(["link", "delete", base])
                    .output()
                    .await
                {
                    Ok(del) if del.status.success() => {
                        tracing::info!(tap = base, "deleted stale Russel TAP interface");
                    }
                    Ok(del) => {
                        tracing::warn!(
                            tap = base,
                            stderr = %String::from_utf8_lossy(&del.stderr).trim(),
                            "failed to delete stale TAP interface"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(tap = base, error = %e, "failed to delete stale TAP interface");
                    }
                }
            }
        }
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "ip link show failed with status {}",
                out.status,
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list network interfaces during cleanup");
        }
    }

    tracing::info!(
        "stale resource cleanup finished (Russel TAPs only; host built-in iptables chains untouched)"
    );
}

/// Collect tap_ids for all live services that have metadata on disk.
///
/// Also re-primes `SUBNET_REGISTRY` from saved host_ip so collision leases
/// survive control-plane restarts (stable mapping, not rehash-from-id).
fn live_service_tap_ids() -> std::collections::HashSet<String> {
    use russel_ctrl::network::{
        allocation_from_network_key, claim_subnet_key, network_key_from_host_ip, preferred_subnet,
    };
    let mut taps = std::collections::HashSet::new();
    let mut services = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/var/lib/russel") {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type()
                && ft.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                if russel_core::reserved::is_reserved_service_dir(name) {
                    continue;
                }
                let meta_path = entry.path().join("metadata.json");
                if !meta_path.exists() {
                    continue;
                }
                let record = russel_ctrl::metadata::load_service_disk_record_from(&meta_path);
                let tap = record.as_ref().and_then(|r| r.tap_id.clone());
                let host_ip = record.as_ref().and_then(|r| r.host_ip.clone());
                services.push((
                    name.to_string(),
                    tap,
                    host_ip.and_then(|ip| network_key_from_host_ip(&ip)),
                ));
            }
        }
    }
    // Restore all saved assignments before asking the allocator for any
    // legacy/missing mapping. Sorting removes read_dir order from the result.
    services.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, _, key) in &services {
        if let Some(key) = key
            && let Err(e) = claim_subnet_key(name, *key)
        {
            tracing::warn!(service_id = %name, error = %e, "failed to restore subnet lease");
        }
    }
    for (name, tap, key) in services {
        if let Some(t) = tap {
            taps.insert(t);
        } else if let Some(key) = key {
            taps.insert(allocation_from_network_key(key).tap_id);
        } else {
            // Legacy metadata without a persisted network identity.
            // Prefer unregistered preferred key — do not allocate leases during cleanup.
            taps.insert(preferred_subnet(&name).tap_id);
        }
    }
    taps
}

/// True for current Russel TAP names: `rsl-` + exactly 8 lowercase hex digits.
fn is_russel_tap(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("rsl-") else {
        return false;
    };
    suffix.len() == 8
        && suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{ip_is_loopback_for_auth, is_russel_tap};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_ips_are_detected() {
        assert!(ip_is_loopback_for_auth(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(ip_is_loopback_for_auth(IpAddr::V4(Ipv4Addr::new(
            127, 0, 0, 255
        ))));
        assert!(ip_is_loopback_for_auth(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // IPv4-mapped IPv6 loopback must count as loopback (to_canonical).
        assert!(ip_is_loopback_for_auth(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0x7f00, 1
        ))));
    }

    #[test]
    fn non_loopback_ips_are_detected() {
        assert!(!ip_is_loopback_for_auth(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!ip_is_loopback_for_auth(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        assert!(!ip_is_loopback_for_auth(IpAddr::V4(Ipv4Addr::new(
            192, 168, 1, 10
        ))));
        // IPv4-mapped non-loopback must NOT count as loopback.
        assert!(!ip_is_loopback_for_auth(IpAddr::V6(Ipv6Addr::new(
            0, 0, 0, 0, 0, 0xffff, 0xc0a8, 0x010a
        ))));
    }

    #[test]
    fn russel_tap_names_match_hash_scheme() {
        assert!(is_russel_tap("rsl-a1b2c3d4"));
        assert!(is_russel_tap("rsl-00000000"));
        assert!(!is_russel_tap("rsl-short"));
        assert!(!is_russel_tap("rsl-a1b2c3d4e")); // too long
        assert!(!is_russel_tap("vm-api"));
        assert!(!is_russel_tap("docker0"));
        assert!(!is_russel_tap("rsl-A1B2C3D4")); // uppercase not lowercase hex
        assert!(!is_russel_tap("rsl-gggggggg")); // not hex
    }
}
