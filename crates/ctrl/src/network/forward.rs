use std::sync::LazyLock;

use tokio::process::Command;

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

//
// Architecture: host→guest publish uses userspace `socat` (OUTPUT path), not
// kernel FORWARD. Guests only need L2 on their TAP + host→guest L3 for socat.
// Enabling `ip_forward=1` without a FORWARD policy lets guest A route to guest B
// (and off-host) via host L3. We install a dedicated chain that default-denies
// all forwarded traffic involving `rsl-*` interfaces *before* turning on
// ip_forward. Never flush built-in chains.

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
        std::env::var("RUSSEL_DISABLE_FORWARD_FILTER")
            .ok()
            .as_deref(),
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
    russel_core::env_util::env_bool(value).unwrap_or(false)
}

/// Spec of one iptables filter rule Russel manages (for tests + docs).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardRuleSpec {
    /// Role within the install plan.
    pub(crate) role: &'static str,
    /// Arguments after `iptables` (excluding the binary name).
    pub(crate) args: Vec<&'static str>,
}

/// Pure description of the filter rules we install (#187).
///
/// Order: create dedicated chain → flush only that chain → DROP terminal →
/// jump from built-in FORWARD for `-i rsl-+` and `-o rsl-+`.
/// Jumps use `-I FORWARD 1` at install time; presence is checked with `-C`.
#[cfg(test)]
pub(crate) fn russel_forward_rule_plan() -> Vec<ForwardRuleSpec> {
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
/// When [`forward_filter_disabled`] is true, logs residual risk and best-effort
/// removes any previously installed Russel-owned FORWARD rules so an operator
/// toggle to allow mode actually takes effect.
pub async fn ensure_forward_filter() {
    if forward_filter_disabled() {
        tracing::warn!(target: "russel_ctrl::network", "{}", FORWARD_FILTER_ALLOW_RISK);
        // Escape hatch must clear lingering deny rules from a prior secure run.
        if let Err(e) = remove_forward_filter().await {
            tracing::warn!(
                error = %e,
                "failed to remove RUSSEL-FORWARD while filter disabled (manual cleanup may be needed)"
            );
        }
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

/// Remove `RUSSEL-FORWARD` jumps and chain when appropriate.
///
/// - Secure default: only clean when no `rsl-` TAPs remain (mirrors
///   [`restore_ip_forward`]).
/// - Allow/disable escape hatch: always best-effort remove Russel-owned rules
///   so toggling `RUSSEL_FORWARD=allow` (or `RUSSEL_DISABLE_FORWARD_FILTER=1`)
///   after a prior secure run actually lifts the deny (even with live TAPs).
///
/// Only cleans Russel-owned rules; never flushes host built-in chains.
/// Fail-soft on errors.
pub async fn restore_forward_filter() {
    if forward_filter_disabled() {
        // Operator opted out: lingering RUSSEL-FORWARD would still deny traffic.
        tracing::info!("forward filter disabled; removing any existing RUSSEL-FORWARD rules");
        if let Err(e) = remove_forward_filter().await {
            tracing::warn!(
                error = %e,
                "failed to remove RUSSEL-FORWARD while filter disabled (manual cleanup may be needed)"
            );
        }
        return;
    }
    let count = count_rsl_taps().await;
    if count > 0 {
        tracing::debug!(
            count,
            "rsl- TAPs still present; leaving RUSSEL-FORWARD in place"
        );
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
        let del = [
            "-w",
            "-D",
            "FORWARD",
            direction,
            RSL_IFACE_MATCH,
            "-j",
            RUSSEL_FORWARD_CHAIN,
        ];
        // Best-effort: keep deleting while -D succeeds.
        for _ in 0..16 {
            if run_iptables(&del).await.is_err() {
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

pub(super) async fn sysctl(key: &str, val: &str) {
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
