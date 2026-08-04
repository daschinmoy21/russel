use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Default)]
struct CheckoutState {
    leases: usize,
    deleting: bool,
}

static ACTIVE_CHECKOUTS: LazyLock<Mutex<HashMap<PathBuf, CheckoutState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHECKOUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn active_checkouts() -> std::sync::MutexGuard<'static, HashMap<PathBuf, CheckoutState>> {
    ACTIVE_CHECKOUTS.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("ACTIVE_CHECKOUTS lock poisoned — recovering");
        poisoned.into_inner()
    })
}

/// Lease for a checkout that is still being read by a deployment.
#[derive(Debug)]
pub struct CheckoutLease {
    path: Option<PathBuf>,
}

impl CheckoutLease {
    fn inactive() -> Self {
        Self { path: None }
    }

    fn active(path: PathBuf) -> Self {
        loop {
            let mut active = active_checkouts();
            let deleting = active.get(&path).is_some_and(|state| state.deleting);
            if deleting {
                drop(active);
                std::thread::yield_now();
                continue;
            }
            active.entry(path.clone()).or_default().leases += 1;
            return Self { path: Some(path) };
        }
    }
}

impl Clone for CheckoutLease {
    fn clone(&self) -> Self {
        match &self.path {
            Some(path) => Self::active(path.clone()),
            None => Self::inactive(),
        }
    }
}

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let mut active = active_checkouts();
        if let Some(state) = active.get_mut(&path) {
            debug_assert!(state.leases > 0);
            state.leases = state.leases.saturating_sub(1);
            if state.leases == 0 && !state.deleting {
                active.remove(&path);
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct GitClient;

impl GitClient {
    pub async fn clone_or_use_local(&self, repo: &str) -> Result<(PathBuf, CheckoutLease)> {
        // Reject URLs starting with '-' (git option injection).
        if repo.starts_with('-') {
            anyhow::bail!("repository URL cannot start with '-' (looks like a git option)");
        }

        // Local path: must be absolute, exist, and not contain traversal.
        // Absolute local deploys are gated behind RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1
        // (default off) so a remote/shared control plane cannot treat the entire
        // host tree as a build root unless the operator opts in.
        let path = PathBuf::from(repo);
        if path.is_absolute() {
            if !local_path_deploy_allowed() {
                anyhow::bail!(
                    "local absolute path deploys are disabled; use a git URL \
                     (https://, http://, ssh://, or git@host:path) or set \
                     RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1 on the control plane for \
                     single-tenant trusted hosts"
                );
            }
            if !path.exists() {
                anyhow::bail!("local path does not exist: {}", repo);
            }
            // Reject '..' components in the path string.
            for component in path.components() {
                if component == std::path::Component::ParentDir {
                    anyhow::bail!("local path must not contain '..' components");
                }
            }
            return Ok((path, CheckoutLease::inactive()));
        }

        // If it looks like a local relative path that exists, reject — require absolute.
        if PathBuf::from(repo).exists() {
            anyhow::bail!(
                "local path '{}' is relative; use an absolute path for local deploys",
                repo
            );
        }

        // Remote: validate URL scheme and extract the host for SSRF checks.
        let scheme = if repo.starts_with("https://") {
            "https"
        } else if repo.starts_with("http://") {
            "http"
        } else if repo.starts_with("ssh://") {
            "ssh"
        } else if repo.starts_with("git@") {
            // git@host:path — allowed, no scheme prefix to strip.
            "git-scp"
        } else if repo.starts_with("file://") {
            anyhow::bail!("file:// URLs are not allowed for security");
        } else {
            anyhow::bail!(
                "unsupported repository URL scheme; must be https://, http://, ssh://, or git@host:path"
            );
        };

        // Block private/metadata/link-local/loopback hosts for all remote schemes.
        //
        // Residual risk — DNS rebinding TOCTOU:
        // We resolve hostnames and reject private addresses before clone, but a
        // malicious authoritative DNS server can return a public IP for our check
        // and a private IP for git's subsequent lookup. Full mitigation would
        // require pinning the resolved address for the whole clone (not practical
        // with stock git). Prefer HTTPS + known hosts; use
        // RUSSEL_GIT_HOST_ALLOWLIST only for trusted internal hostnames.
        //
        // HTTP redirects: we set `http.followRedirects=false` on http(s) clones so
        // an allowed host cannot 30x into a blocked one. Canonical clone URLs
        // (e.g. GitHub) do not require redirects.
        let host = extract_url_host(repo, scheme)?;
        validate_remote_host(&host)?;
        validate_remote_host_dns(&host).await?;

        let checkout_root = PathBuf::from("/tmp/russel/checkouts");
        fs::create_dir_all(&checkout_root).context("failed to create checkout directory")?;

        // Best-effort GC of old checkouts (age-based) before clone.
        if let Err(e) = gc_old_checkouts(&checkout_root, Duration::from_secs(24 * 3600)) {
            tracing::warn!(error = %e, "checkout GC failed (continuing)");
        }

        // Immutable per-deploy workdir: URL hash prefix + unique deploy stamp so
        // concurrent deploys / age-based GC cannot clobber an in-use checkout.
        let checkout = reserve_checkout_dir(&checkout_root, repo)?;

        let mut cmd = Command::new("git");
        for arg in clone_security_config_args(scheme) {
            cmd.arg(arg);
        }
        let output = cmd
            .arg("clone")
            .arg("--")
            .arg(repo)
            .arg(&checkout)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Err(cleanup_err) = fs::remove_dir_all(&checkout)
                && cleanup_err.kind() != std::io::ErrorKind::NotFound
            {
                anyhow::bail!(
                    "git clone failed for {repo}: {} (also failed to remove partial checkout {}: {})",
                    stderr.trim(),
                    checkout.display(),
                    cleanup_err
                );
            }
            anyhow::bail!(
                "git clone failed for {repo}: {}.\nUse an absolute path for local deploys.",
                stderr.trim()
            );
        }

        Ok((checkout.clone(), CheckoutLease::active(checkout)))
    }

    /// Keep a resolved checkout out of age-based GC until the caller finishes
    /// reading it. The checkout is immutable and may safely be used by other
    /// concurrent deployments, but must not be removed while this lease lives.
    pub fn hold_checkout(&self, path: &Path) -> CheckoutLease {
        CheckoutLease::active(path.to_path_buf())
    }
}

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
fn extract_url_host(repo: &str, scheme: &str) -> anyhow::Result<String> {
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

/// Git `-c` options applied to remote clones to reduce SSRF surface.
///
/// For http/https clones, disables following HTTP redirects so an allowlisted
/// public host cannot 30x into a private/metadata endpoint.
fn clone_security_config_args(scheme: &str) -> Vec<&'static str> {
    let mut args = Vec::new();
    if matches!(scheme, "http" | "https") {
        args.extend(["-c", "http.followRedirects=false"]);
    }
    args
}

/// Whether absolute local path deploys are permitted.
///
/// Default **off**. Set `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` on the control plane
/// only on single-tenant trusted hosts (local dev). When a remote client sends
/// an absolute path, the path is resolved on the **control-plane host**, not the
/// client — so leaving this enabled on a shared/remote ctrl exposes the host
/// filesystem as a build root.
fn local_path_deploy_allowed() -> bool {
    std::env::var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Hosts listed in `RUSSEL_GIT_HOST_ALLOWLIST` (comma-separated, case-insensitive)
/// skip the DNS private-IP check. Use only for trusted internal git hostnames
/// that intentionally resolve to RFC1918 / CGNAT addresses. Literal private IPs
/// are still rejected.
fn is_git_host_allowlisted(host: &str) -> bool {
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
fn validate_remote_host(host: &str) -> anyhow::Result<()> {
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
async fn validate_remote_host_dns(host: &str) -> anyhow::Result<()> {
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
fn reject_blocked_ipv4(host: &str, ipv4: Ipv4Addr) -> anyhow::Result<()> {
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
fn reject_blocked_ipv6(host: &str, ipv6: Ipv6Addr) -> anyhow::Result<()> {
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

fn fnv1a_u64(bytes: impl AsRef<[u8]>) -> u64 {
    bytes
        .as_ref()
        .iter()
        .fold(14_695_981_039_346_656_037u64, |acc, &b| {
            (acc ^ b as u64).wrapping_mul(1_099_511_628_211)
        })
}

/// Stable URL identity: the standard 64-bit FNV-1a result as 16 lowercase hex
/// digits.
fn url_hash_hex(repo: &str) -> String {
    format!("{:016x}", fnv1a_u64(repo.as_bytes()))
}

/// Stable, unique directory name stem for a remote repo URL.
///
/// Format: `{sanitized_prefix}-{16 hex FNV identity}`.
fn checkout_dir_name(repo: &str) -> String {
    let hash = url_hash_hex(repo);
    let prefix: String = repo
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    let prefix = prefix.trim_matches('-');
    if prefix.is_empty() {
        format!("repo-{hash}")
    } else {
        format!("{prefix}-{hash}")
    }
}

/// Per-deploy immutable directory under the URL stem.
fn unique_checkout_dir_name(repo: &str) -> String {
    unique_checkout_dir_name_with(
        repo,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        CHECKOUT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

fn unique_checkout_dir_name_with(repo: &str, nanos: u128, sequence: u64) -> String {
    let stamp = format!("{:x}", nanos ^ ((std::process::id() as u128) << 16));
    format!("{}-d{stamp}-{sequence:016x}", checkout_dir_name(repo))
}

/// Atomically reserve an empty destination directory for a clone. The empty
/// directory is accepted by `git clone`; `create_dir` makes name collisions
/// observable and allows a retry without deleting another deployment's tree.
fn reserve_checkout_dir(checkout_root: &Path, repo: &str) -> anyhow::Result<PathBuf> {
    for _ in 0..1024 {
        let checkout = checkout_root.join(unique_checkout_dir_name(repo));
        match fs::create_dir(&checkout) {
            Ok(()) => return Ok(checkout),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to reserve checkout directory {}",
                        checkout.display()
                    )
                });
            }
        }
    }
    anyhow::bail!("could not reserve a unique checkout directory after 1024 attempts")
}

/// Claim an unleased checkout and delete it while holding the active-checkout
/// mutex. A new lease therefore cannot appear between the eligibility check
/// and the deletion.
fn remove_unleased_checkout(path: &Path) -> std::io::Result<bool> {
    let mut active = active_checkouts();
    let state = active.entry(path.to_path_buf()).or_default();
    if state.leases != 0 || state.deleting {
        return Ok(false);
    }
    state.deleting = true;
    let result = fs::remove_dir_all(path);
    active.remove(path);
    result.map(|()| true)
}

/// Remove checkout directories older than `max_age`.
///
/// Filesystem errors on individual entries are logged and skipped so one
/// bad directory does not abort GC of the rest.
pub fn gc_old_checkouts(checkout_root: &Path, max_age: Duration) -> anyhow::Result<usize> {
    if !checkout_root.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0usize;
    let entries = match fs::read_dir(checkout_root) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read checkout root for GC");
            return Ok(0);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "skip unreadable checkout entry during GC");
                continue;
            }
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unreadable metadata during GC"
                );
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unknown mtime during GC"
                );
                continue;
            }
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > max_age {
            match remove_unleased_checkout(&entry.path()) {
                Ok(true) => {
                    tracing::info!(path = %entry.path().display(), "GC removed old git checkout");
                    removed += 1;
                }
                Ok(false) => {
                    tracing::debug!(path = %entry.path().display(), "skip active checkout during GC");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "failed to GC git checkout"
                    );
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    #[test]
    fn fnv1a_uses_xor_before_multiply() {
        assert_eq!(fnv1a_u64("hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn checkout_dir_name_is_unique_for_distinct_urls() {
        let a = checkout_dir_name("https://github.com/org/repo-a.git");
        let b = checkout_dir_name("https://github.com/org/repo-b.git");
        assert_ne!(a, b);
        // Same URL is stable.
        assert_eq!(a, checkout_dir_name("https://github.com/org/repo-a.git"));
        // 16 hex digits in the hash suffix.
        let hash = a.rsplit('-').next().unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn checkout_dir_name_avoids_sanitize_collision() {
        // Identical 48-char sanitized prefix; only the hash must distinguish them.
        let base = "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let a = checkout_dir_name(&format!("{base}/one"));
        let b = checkout_dir_name(&format!("{base}/two"));
        assert_ne!(a, b);
        // Prefixes before the 16-hex hash should match.
        let pa = a.rsplit_once('-').unwrap().0;
        let pb = b.rsplit_once('-').unwrap().0;
        assert_eq!(pa, pb);
    }

    #[test]
    fn gc_removes_only_stale_dirs() {
        let tmp = TempDir::new().unwrap();
        let stale = tmp.path().join("stale-checkout");
        let fresh = tmp.path().join("fresh-checkout");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&fresh).unwrap();

        let now = SystemTime::now();
        let stale_time = now - Duration::from_secs(2 * 3600);
        fs::File::open(&stale)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stale_time))
            .unwrap();
        fs::File::open(&fresh)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now))
            .unwrap();

        let removed = gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn unique_checkout_dirs_share_stem() {
        let stem = checkout_dir_name("https://github.com/org/repo.git");
        let a = unique_checkout_dir_name("https://github.com/org/repo.git");
        assert!(a.starts_with(&stem));
        assert!(a.contains("-d"));
    }

    #[test]
    fn unique_checkout_names_differ_with_equal_timestamps() {
        let repo = "https://github.com/org/repo.git";
        let a = unique_checkout_dir_name_with(repo, 42, 10);
        let b = unique_checkout_dir_name_with(repo, 42, 11);
        assert_ne!(a, b);
    }

    #[test]
    fn concurrent_checkout_names_are_unique() {
        let repo = "https://github.com/org/repo.git";
        let names: HashSet<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| unique_checkout_dir_name(repo)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(names.len(), 16);
    }

    #[test]
    fn checkout_leases_are_ref_counted() {
        let tmp = TempDir::new().unwrap();
        let checkout = tmp.path().join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        fs::File::open(&checkout)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(2 * 3600)),
            )
            .unwrap();

        let first = GitClient.hold_checkout(&checkout);
        let second = first.clone();
        assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 2);
        drop(first);
        assert_eq!(active_checkouts().get(&checkout).unwrap().leases, 1);
        assert_eq!(
            gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
            0
        );
        drop(second);
        assert!(!active_checkouts().contains_key(&checkout));
        assert_eq!(
            gc_old_checkouts(tmp.path(), Duration::from_secs(3600)).unwrap(),
            1
        );
    }

    #[test]
    fn checkout_destination_is_atomically_reserved() {
        let tmp = TempDir::new().unwrap();
        let repo = "https://github.com/org/repo.git";
        let a = reserve_checkout_dir(tmp.path(), repo).unwrap();
        let b = reserve_checkout_dir(tmp.path(), repo).unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir());
        assert!(b.is_dir());
    }

    #[test]
    fn extract_url_host_handles_userinfo_ipv6_and_ports() {
        assert_eq!(
            extract_url_host("https://x@169.254.169.254/repo.git", "https").unwrap(),
            "169.254.169.254"
        );
        assert_eq!(
            extract_url_host("https://user:pass@github.com:8443/repo.git", "https").unwrap(),
            "github.com"
        );
        assert_eq!(
            extract_url_host("http://[fe80::1]:8080/repo.git", "http").unwrap(),
            "fe80::1"
        );
        assert_eq!(
            extract_url_host("http://[::1]/repo.git", "http").unwrap(),
            "::1"
        );
        assert_eq!(
            extract_url_host("ssh://git@github.com/user/repo.git", "ssh").unwrap(),
            "github.com"
        );
        assert_eq!(
            extract_url_host("ssh://git@github.com:22/user/repo.git", "ssh").unwrap(),
            "github.com"
        );
        assert_eq!(
            extract_url_host("git@github.com:user/repo.git", "git-scp").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn userinfo_metadata_bypass_is_rejected() {
        let repo = "https://x@169.254.169.254/repo.git";
        let err = extract_url_host(repo, "https")
            .and_then(|h| validate_remote_host(&h))
            .unwrap_err();
        assert!(err.to_string().contains("link-local"));
    }

    #[test]
    fn ipv6_link_local_and_loopback_rejected() {
        for repo in [
            "http://[fe80::1]/repo.git",
            "http://[fe80::1%25eth0]/repo.git",
            "http://[::1]/repo.git",
            "http://[fd00:ec2::254]/repo.git",
        ] {
            let err = extract_url_host(repo, "http")
                .and_then(|h| validate_remote_host(&h))
                .unwrap_err();
            assert!(
                err.to_string().contains("link-local")
                    || err.to_string().contains("loopback")
                    || err.to_string().contains("EC2 metadata")
                    || err.to_string().contains("percent-encoded"),
                "unexpected error for {repo}: {err}"
            );
        }
    }

    #[test]
    fn cloud_metadata_ipv4_ranges_rejected() {
        for repo in [
            "https://169.254.169.254/repo.git",
            "https://169.254.0.1/repo.git",
            "https://100.100.100.100/repo.git",
            "https://100.100.2.34/repo.git",
            "https://0.0.0.0/repo.git",
        ] {
            assert!(
                extract_url_host(repo, "https")
                    .and_then(|h| validate_remote_host(&h))
                    .is_err(),
                "expected rejection for {repo}"
            );
        }
    }

    #[test]
    fn loopback_hosts_rejected_for_remote_schemes() {
        for repo in [
            "https://127.0.0.1/repo.git",
            "https://localhost/repo.git",
            "https://127.1.0.1/repo.git",
            "ssh://127.0.0.1/repo.git",
            "git@127.0.0.1:repo.git",
            "git@localhost:repo.git",
        ] {
            assert!(
                extract_url_host(
                    repo,
                    if repo.starts_with("https://") {
                        "https"
                    } else if repo.starts_with("ssh://") {
                        "ssh"
                    } else {
                        "git-scp"
                    }
                )
                .and_then(|h| validate_remote_host(&h))
                .is_err(),
                "expected rejection for {repo}"
            );
        }
    }

    #[test]
    fn legitimate_remote_hosts_accepted() {
        for repo in [
            "https://github.com/org/repo.git",
            "https://git.example.com:8443/org/repo.git",
            "ssh://git@github.com/org/repo.git",
            "git@github.com:org/repo.git",
        ] {
            assert!(
                extract_url_host(
                    repo,
                    if repo.starts_with("https://") {
                        "https"
                    } else if repo.starts_with("ssh://") {
                        "ssh"
                    } else {
                        "git-scp"
                    }
                )
                .and_then(|h| validate_remote_host(&h))
                .is_ok(),
                "expected acceptance for {repo}"
            );
        }
    }

    #[test]
    fn non_canonical_ipv4_forms_rejected() {
        for host in [
            "0177.0.0.1",
            "0x7f.0.0.1",
            "127.0.0.01",
            "2130706433",
            "127.0.0.1.",
        ] {
            assert!(
                validate_remote_host(host).is_err(),
                "expected rejection for {host}"
            );
        }
    }

    #[test]
    fn ipv4_mapped_and_unspecified_ipv6_rejected() {
        for host in [
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "::",
            "0:0:0:0:0:0:0:0",
        ] {
            assert!(
                validate_remote_host(host).is_err(),
                "expected rejection for {host}"
            );
        }
    }

    #[test]
    fn rfc1918_and_cgnat_literal_ips_rejected() {
        for host in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.1",
            "192.168.0.1",
            "192.168.255.255",
            "100.64.0.1",
            "100.127.255.254",
            "100.100.100.200",
        ] {
            let err = validate_remote_host(host).unwrap_err().to_string();
            assert!(
                err.contains("private")
                    || err.contains("carrier-grade")
                    || err.contains("metadata"),
                "expected private/CGNAT/metadata rejection for {host}, got: {err}"
            );
        }
    }

    #[test]
    fn ipv6_ula_rejected() {
        for host in ["fc00::1", "fd12:3456:789a::1", "fd00:ec2::254"] {
            let err = validate_remote_host(host).unwrap_err().to_string();
            assert!(
                err.contains("unique-local") || err.contains("EC2 metadata"),
                "expected ULA/EC2 rejection for {host}, got: {err}"
            );
        }
    }

    #[test]
    fn http_clone_disables_follow_redirects() {
        let https_args = clone_security_config_args("https");
        assert_eq!(https_args, vec!["-c", "http.followRedirects=false"]);
        let http_args = clone_security_config_args("http");
        assert_eq!(http_args, vec!["-c", "http.followRedirects=false"]);
        // ssh / scp clones do not set http.* config
        assert!(clone_security_config_args("ssh").is_empty());
        assert!(clone_security_config_args("git-scp").is_empty());
    }

    /// Serialize mutations of `RUSSEL_GIT_HOST_ALLOWLIST` and restore even on panic.
    fn with_git_host_allowlist<T>(value: &str, f: impl FnOnce() -> T) -> T {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        struct EnvRestore(Option<std::ffi::OsString>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                // SAFETY: exclusive LOCK held; only test code mutates this var.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("RUSSEL_GIT_HOST_ALLOWLIST", v),
                        None => std::env::remove_var("RUSSEL_GIT_HOST_ALLOWLIST"),
                    }
                }
            }
        }

        let prev = std::env::var_os("RUSSEL_GIT_HOST_ALLOWLIST");
        let _restore = EnvRestore(prev);
        // SAFETY: exclusive lock held for the duration of the mutation + body.
        unsafe {
            std::env::set_var("RUSSEL_GIT_HOST_ALLOWLIST", value);
        }
        f()
    }

    #[test]
    fn git_host_allowlist_parses_comma_separated() {
        with_git_host_allowlist("git.internal.example, Other.Git.Local", || {
            assert!(is_git_host_allowlisted("git.internal.example"));
            assert!(is_git_host_allowlisted("GIT.INTERNAL.EXAMPLE"));
            assert!(is_git_host_allowlisted("other.git.local"));
            assert!(!is_git_host_allowlisted("evil.example"));
            assert!(!is_git_host_allowlisted("10.0.0.1"));
        });
    }

    #[test]
    fn reject_blocked_helpers_cover_dns_path_ips() {
        // DNS results reuse the same helpers as literal hosts.
        assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(10, 1, 2, 3)).is_err());
        assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(100, 64, 0, 1)).is_err());
        assert!(reject_blocked_ipv4("resolved", Ipv4Addr::new(8, 8, 8, 8)).is_ok());
        assert!(reject_blocked_ipv6("resolved", Ipv6Addr::from_str("fd12::1").unwrap()).is_err());
        assert!(
            reject_blocked_ipv6(
                "resolved",
                Ipv6Addr::from_str("2001:4860:4860::8888").unwrap()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn dns_check_skips_literal_ips() {
        // Literals are not re-resolved; private ones already fail validate_remote_host.
        assert!(validate_remote_host_dns("8.8.8.8").await.is_ok());
        assert!(validate_remote_host_dns("10.0.0.1").await.is_ok());
    }

    // ── RUSSEL_ALLOW_LOCAL_PATH_DEPLOY gate (#196) ─────────────────────────
    // Serialize env mutations: cargo runs tests in parallel by default.

    static LOCAL_PATH_DEPLOY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn with_local_path_deploy_env<T, F, Fut>(value: Option<&str>, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = LOCAL_PATH_DEPLOY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY").ok();
        // SAFETY: exclusive lock held for the duration of the mutation + body.
        unsafe {
            match value {
                Some(v) => std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", v),
                None => std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
            }
        }
        let result = f().await;
        unsafe {
            match previous {
                Some(v) => std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", v),
                None => std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
            }
        }
        result
    }

    #[test]
    fn local_path_deploy_allowed_defaults_off() {
        let _guard = LOCAL_PATH_DEPLOY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY").ok();
        unsafe {
            std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY");
        }
        assert!(!local_path_deploy_allowed());
        unsafe {
            std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", "0");
        }
        assert!(!local_path_deploy_allowed());
        unsafe {
            std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", "true");
        }
        assert!(!local_path_deploy_allowed());
        unsafe {
            std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", "1");
        }
        assert!(local_path_deploy_allowed());
        unsafe {
            std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", " 1 ");
        }
        assert!(local_path_deploy_allowed());
        match previous {
            Some(v) => unsafe { std::env::set_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY", v) },
            None => unsafe { std::env::remove_var("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY") },
        }
    }

    #[tokio::test]
    async fn local_absolute_path_rejected_when_gate_off() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().canonicalize().unwrap();
        let path_str = path.display().to_string();

        with_local_path_deploy_env(None, || async {
            let err = GitClient
                .clone_or_use_local(&path_str)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY") && err.contains("disabled"),
                "unexpected error: {err}"
            );
        })
        .await;

        with_local_path_deploy_env(Some("0"), || async {
            let err = GitClient
                .clone_or_use_local(&path_str)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("RUSSEL_ALLOW_LOCAL_PATH_DEPLOY"),
                "unexpected error: {err}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn local_absolute_path_accepted_when_gate_on() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().canonicalize().unwrap();
        let path_str = path.display().to_string();

        with_local_path_deploy_env(Some("1"), || async {
            let (resolved, _lease) = GitClient
                .clone_or_use_local(&path_str)
                .await
                .expect("local path should be accepted when gate is on");
            assert_eq!(resolved, path);
        })
        .await;
    }

    #[tokio::test]
    async fn local_absolute_path_still_checks_exists_and_dotdot_when_gate_on() {
        with_local_path_deploy_env(Some("1"), || async {
            let missing = "/tmp/russel-local-path-gate-missing-xyz-196";
            let err = GitClient
                .clone_or_use_local(missing)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("does not exist"),
                "expected existence check, got: {err}"
            );

            // Absolute path containing '..' components is rejected even when allowed.
            let with_dotdot = "/tmp/../etc";
            let err = GitClient
                .clone_or_use_local(with_dotdot)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("..") || err.contains("does not exist"),
                "expected .. or existence rejection, got: {err}"
            );
        })
        .await;
    }
}
