use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr as _,
};

use anyhow::Context as _;

/// Extract the host part of a remote repository URL.
///
/// Supported forms:
/// - `http://[user[:pass]@]host[:port]/path`
/// - `https://[user[:pass]@]host[:port]/path`
/// - `ssh://[user@]host[:port]/path`
/// - `git@host:path` (scp-like syntax; port is not supported)
///
/// The returned host is lowercased. IPv6 addresses are returned without
/// brackets so they can be parsed with `Ipv6Addr::from_str`.
pub(super) fn extract_url_host(repo: &str, scheme: &str) -> anyhow::Result<String> {
    let host = match scheme {
        "http" | "https" | "ssh" => {
            let prefix = format!("{scheme}://");
            let rest = repo
                .strip_prefix(&prefix)
                .ok_or_else(|| anyhow::anyhow!("missing {scheme}:// prefix"))?;
            let authority = rest.split('/').next().unwrap_or(rest);
            if authority.is_empty() {
                anyhow::bail!("repository URL has empty host");
            }
            // Strip `[user[:pass]@]`; the host is after the final '@'.
            let host_port = authority
                .rsplit_once('@')
                .map(|(_, hp)| hp)
                .unwrap_or(authority);
            parse_authority_host(host_port)?
        }
        "git-scp" => {
            // SCP syntax: [user@]host:path.  We keep only the host part.
            let after_user = repo.rsplit_once('@').map(|(_, rest)| rest).unwrap_or(repo);
            let host = after_user
                .split(':')
                .next()
                .ok_or_else(|| anyhow::anyhow!("repository URL has empty host"))?;
            if host.is_empty() {
                anyhow::bail!("repository URL has empty host");
            }
            host.to_string()
        }
        _ => anyhow::bail!("unsupported URL scheme for host extraction: {scheme}"),
    };
    Ok(host.to_lowercase())
}

/// Parse `host[:port]` and return the host.
///
/// Bracketed IPv6 (`[::1]:22`) is supported.  A non-bracketed address with
/// more than one colon is rejected as ambiguous because it could be an IPv6
/// address with an indistinguishable port.
fn parse_authority_host(host_port: &str) -> anyhow::Result<String> {
    if let Some(inside_bracket) = host_port.strip_prefix('[') {
        let (host, after) = inside_bracket
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("unclosed IPv6 bracket in URL"))?;
        if !after.is_empty() && !after.starts_with(':') {
            anyhow::bail!("unexpected characters after IPv6 bracket in URL");
        }
        return Ok(host.to_string());
    }

    if let Some((host, _port)) = host_port.rsplit_once(':') {
        if host.contains(':') {
            // Ambiguous: looks like an unbracketed IPv6 address.  Reject it
            // rather than misclassifying a port.
            anyhow::bail!("non-bracketed IPv6 address is ambiguous: {host_port}");
        }
        if host.is_empty() {
            anyhow::bail!("repository URL has empty host");
        }
        return Ok(host.to_string());
    }

    Ok(host_port.to_string())
}

/// Hosts listed in `RUSSEL_GIT_HOST_ALLOWLIST` (comma-separated, case-insensitive)
/// skip the DNS private-IP check. Use only for trusted internal git hostnames
/// that intentionally resolve to RFC1918 / CGNAT addresses. Literal private IPs
/// are still rejected.
pub(super) fn is_git_host_allowlisted(host: &str) -> bool {
    let Ok(list) = std::env::var("RUSSEL_GIT_HOST_ALLOWLIST") else {
        return false;
    };
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|entry| entry.eq_ignore_ascii_case(host))
}

/// Validate that a remote clone host is not a private/metadata/link-local/loopback
/// address when given as a literal IP, and apply hostname format checks.
///
/// Hostnames that pass this check are still subject to
/// [`validate_remote_host_dns`] before clone.
pub(super) fn validate_remote_host(host: &str) -> anyhow::Result<()> {
    if host.eq_ignore_ascii_case("localhost") {
        anyhow::bail!("repository URL host 'localhost' is not allowed for remote clones");
    }

    // IPv4: parse and also reject non-canonical forms (hex, octal, decimal).
    if let Ok(ipv4) = Ipv4Addr::from_str(host) {
        if !is_canonical_ipv4(host) {
            anyhow::bail!("non-canonical IPv4 address form: {host}");
        }
        return reject_blocked_ipv4(host, ipv4);
    }

    // IPv6: unspecified, IPv4-mapped (via IPv4 blocklist), loopback, link-local,
    // ULA, EC2 meta.
    if let Ok(ipv6) = Ipv6Addr::from_str(host) {
        return reject_blocked_ipv6(host, ipv6);
    }

    // Reject hostnames containing '%' — percent-encoding may be decoded by
    // the HTTP layer (libcurl) and used to smuggle IPv6 scope IDs or other
    // characters past this check.
    if host.contains('%') {
        anyhow::bail!("repository URL host contains percent-encoded characters: {host}");
    }

    // Reject hostnames that look like non-canonical numeric IP addresses (e.g.
    // the decimal form `2130706433` or a trailing-dotted `127.0.0.1.`). These
    // are not valid DNS names and may be interpreted as an IP by git or libc.
    if host
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ':')
    {
        anyhow::bail!("non-canonical numeric host is not allowed: {host}");
    }

    // Reject dotted forms with hex/octal octets that libc resolvers may
    // interpret as IP addresses (e.g. 0x7f.0.0.1 → 127.0.0.1).
    if host.contains('.') {
        for part in host.split('.') {
            if part.starts_with("0x") || part.starts_with("0X") {
                anyhow::bail!("non-canonical numeric host is not allowed: {host}");
            }
            if part.starts_with('0') && part.len() > 1 && part.chars().all(|c| c.is_ascii_digit()) {
                anyhow::bail!("non-canonical numeric host is not allowed: {host}");
            }
        }
    }

    // Hostname format is acceptable; DNS private-IP check happens asynchronously.
    Ok(())
}

/// Best-effort DNS resolution check: reject hostnames that resolve to any
/// private/link-local/loopback/metadata address.
///
/// Residual DNS rebinding TOCTOU: the address seen here may differ from the
/// address git later connects to. Documented at the clone call site.
///
/// Hosts in `RUSSEL_GIT_HOST_ALLOWLIST` skip this check (internal git only).
/// Literal IPs are skipped (already validated by [`validate_remote_host`]).
/// Resolution failure is fail-closed (clone cannot proceed without DNS anyway).
pub(super) async fn validate_remote_host_dns(host: &str) -> anyhow::Result<()> {
    // Literals already fully checked.
    if Ipv4Addr::from_str(host).is_ok() || Ipv6Addr::from_str(host).is_ok() {
        return Ok(());
    }

    if is_git_host_allowlisted(host) {
        tracing::debug!(
            host,
            "skipping DNS private-IP check (RUSSEL_GIT_HOST_ALLOWLIST)"
        );
        return Ok(());
    }

    // Port is required by lookup_host but ignored for the blocklist; use 443.
    let addrs = tokio::net::lookup_host((host, 443))
        .await
        .with_context(|| {
            format!("DNS resolution failed for repository host '{host}' (refusing clone)")
        })?;

    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        match addr.ip() {
            IpAddr::V4(ipv4) => reject_blocked_ipv4(host, ipv4)?,
            IpAddr::V6(ipv6) => reject_blocked_ipv6(host, ipv6)?,
        }
    }

    if !saw_any {
        anyhow::bail!("DNS resolution returned no addresses for repository host '{host}'");
    }

    Ok(())
}

/// Return true if `s` is the canonical dotted-decimal form of an IPv4 address.
///
/// This rejects hex/octal/decimal variants that git (or the OS resolver) may
/// interpret differently, e.g. `0x7f.0.0.1`, `0177.0.0.1`, `2130706433`.
fn is_canonical_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    for part in parts {
        match part.parse::<u8>() {
            Ok(n) if part == format!("{n}") => {}
            _ => return false,
        }
    }
    true
}

fn is_link_local_ipv4(ip: Ipv4Addr) -> bool {
    // 169.254.0.0/16 (also covered by Ipv4Addr::is_link_local)
    let octets = ip.octets();
    octets[0] == 169 && octets[1] == 254
}

/// Carrier-grade NAT (RFC 6598) 100.64.0.0/10.
fn is_cgnat_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xc0) == 64
}

fn is_metadata_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    // GCP (100.100.2.0/24), Azure/DO/Oracle (100.100.100.0/24), etc.
    // Subset of CGNAT; kept for a clearer error message.
    octets[0] == 100 && octets[1] == 100
}

/// Shared IPv4 blocklist (also used for IPv4-mapped IPv6 hosts and DNS results).
pub(super) fn reject_blocked_ipv4(host: &str, ipv4: Ipv4Addr) -> anyhow::Result<()> {
    if ipv4.is_unspecified() {
        anyhow::bail!("repository URL host {host} is unspecified (0.0.0.0)");
    }
    if ipv4.is_loopback() {
        anyhow::bail!("repository URL host {host} is loopback");
    }
    // RFC1918: 10/8, 172.16/12, 192.168/16
    if ipv4.is_private() {
        anyhow::bail!("repository URL host {host} is a private (RFC1918) address");
    }
    if is_link_local_ipv4(ipv4) {
        anyhow::bail!("repository URL host {host} is link-local");
    }
    // Prefer the more specific metadata message when applicable.
    if is_metadata_ipv4(ipv4) {
        anyhow::bail!("repository URL host {host} is a cloud metadata service");
    }
    if is_cgnat_ipv4(ipv4) {
        anyhow::bail!("repository URL host {host} is carrier-grade NAT (100.64/10)");
    }
    if ipv4.is_broadcast() {
        anyhow::bail!("repository URL host {host} is broadcast");
    }
    if ipv4.is_multicast() {
        anyhow::bail!("repository URL host {host} is multicast");
    }
    Ok(())
}

fn is_link_local_ipv6(ip: Ipv6Addr) -> bool {
    // fe80::/10
    ip.is_unicast_link_local() || {
        let segments = ip.segments();
        (segments[0] & 0xffc0) == 0xfe80
    }
}

fn is_unique_local_ipv6(ip: Ipv6Addr) -> bool {
    // fc00::/7 (ULA)
    ip.is_unique_local()
}

fn is_ec2_metadata_ipv6(ip: Ipv6Addr) -> bool {
    // fd00:ec2::/32 (also ULA; kept for a clearer error message)
    let segments = ip.segments();
    segments[0] == 0xfd00 && segments[1] == 0x0ec2
}

/// Shared IPv6 blocklist (literals and DNS results). Handles IPv4-mapped form.
pub(super) fn reject_blocked_ipv6(host: &str, ipv6: Ipv6Addr) -> anyhow::Result<()> {
    match ipv6.to_canonical() {
        IpAddr::V4(ipv4) => {
            // ::ffff:169.254.169.254 / ::ffff:127.0.0.1 etc. must not bypass the
            // IPv4 blocklist.
            reject_blocked_ipv4(host, ipv4)
        }
        IpAddr::V6(ipv6) => {
            if ipv6.is_unspecified() {
                anyhow::bail!("repository URL host {host} (::) is unspecified");
            }
            if ipv6.is_loopback() {
                anyhow::bail!("repository URL host {host} (::1) is loopback");
            }
            if is_link_local_ipv6(ipv6) {
                anyhow::bail!("repository URL host {host} is link-local");
            }
            if is_ec2_metadata_ipv6(ipv6) {
                anyhow::bail!("repository URL host {host} is EC2 metadata");
            }
            if is_unique_local_ipv6(ipv6) {
                anyhow::bail!("repository URL host {host} is IPv6 unique-local (fc00::/7)");
            }
            if ipv6.is_multicast() {
                anyhow::bail!("repository URL host {host} is multicast");
            }
            Ok(())
        }
    }
}
