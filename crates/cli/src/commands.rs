use std::collections::HashMap;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use reqwest::header::{AUTHORIZATION, HeaderMap};
use russel_core::{
    RuntimeKind,
    api::{
        DeployEvent, DeployRequest, DeployResponse, LogsResponse, PortMapping, StatusResponse,
        VmsResponse,
    },
    config::{Russelfile, merge_env_maps, resolve_runtime, validate_env_map},
};

/// Shared HTTP client that attaches Bearer auth when RUSSEL_API_TOKEN is set.
///
/// Token handling matches ctrl `normalize_api_token`: trim whitespace; blank → no auth.
///
/// When a token is present and the control-plane URL is plain `http://`:
/// - **non-loopback host** → hard-fail (F-05 / #189) unless `--insecure` or
///   `RUSSEL_INSECURE_CLEARTEXT=1|true|yes`
/// - **loopback host** → one-time stderr warning only
///
/// Returns an error when the token value cannot be parsed into a valid HTTP
/// header value (F-47).
fn http_client(control_plane: &str) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Ok(raw) = std::env::var("RUSSEL_API_TOKEN") {
        let token = raw.trim();
        if !token.is_empty() {
            let header_value = format!("Bearer {token}").parse().map_err(|_| {
                anyhow!("RUSSEL_API_TOKEN contains characters invalid in an HTTP header")
            })?;
            headers.insert(AUTHORIZATION, header_value);
            // F-05 / #189: refuse cleartext Bearer to non-loopback; warn on loopback.
            ensure_cleartext_token_ok(control_plane)?;
        }
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))
}

// ── F-05 / #189: cleartext Bearer policy ───────────────────────────────────

static CLEARTEXT_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// CLI `--insecure` latch (set once from `main` before any subcommand runs).
static CLI_INSECURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the global `--insecure` flag for cleartext Bearer policy.
pub fn set_cli_insecure(insecure: bool) {
    CLI_INSECURE.store(insecure, std::sync::atomic::Ordering::Relaxed);
}

fn insecure_cleartext_allowed() -> bool {
    if CLI_INSECURE.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    match std::env::var("RUSSEL_INSECURE_CLEARTEXT") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes")
        }
        Err(_) => false,
    }
}

/// Enforce cleartext Bearer policy when a non-empty token will be sent.
///
/// - `https://` → ok
/// - `http://` + loopback → warn once, ok
/// - `http://` + non-loopback → error unless insecure escape hatch (then warn once)
fn ensure_cleartext_token_ok(control_plane: &str) -> Result<()> {
    let rest = match control_plane.strip_prefix("http://") {
        Some(r) => r,
        None => return Ok(()), // https:// or other schemes
    };
    let host = extract_http_host(rest);
    if is_loopback_host(host) {
        // Warn-only on loopback (local dev still cleartext, but not on-path WAN risk).
        if !CLEARTEXT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "\x1b[1;33mwarning:\x1b[0m RUSSEL_API_TOKEN is sent in cleartext over \
                 plain HTTP to loopback host \x1b[1m{host}\x1b[0m"
            );
        }
        return Ok(());
    }
    if insecure_cleartext_allowed() {
        if !CLEARTEXT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "\x1b[1;33mwarning:\x1b[0m RUSSEL_API_TOKEN is sent in cleartext to \
                 non-loopback host \x1b[1m{host}\x1b[0m over plain HTTP \
                 (--insecure / RUSSEL_INSECURE_CLEARTEXT)"
            );
        }
        return Ok(());
    }
    Err(anyhow!(
        "refusing to send RUSSEL_API_TOKEN over cleartext HTTP to non-loopback host `{host}`\n\
         \n\
         Use HTTPS (terminate TLS at a reverse proxy in front of russel-ctrl — see docs/security-tls.md),\n\
         or target a loopback URL (e.g. http://127.0.0.1:7878),\n\
         or override with --insecure / RUSSEL_INSECURE_CLEARTEXT=1 (not recommended)."
    ))
}

/// Extract host from the authority part of an `http://` URL (no scheme prefix).
///
/// Handles `host:port/path`, bare `host`, and bracketed IPv6 (`[::1]:7878/...`).
fn extract_http_host(rest: &str) -> &str {
    let authority = rest.split('/').next().unwrap_or("");
    if let Some(inner) = authority.strip_prefix('[')
        && let Some(end) = inner.find(']')
    {
        return &inner[..end];
    }
    authority.split(':').next().unwrap_or("")
}

fn is_loopback_host(host: &str) -> bool {
    // Strip surrounding brackets if a caller passed `[::1]` whole.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    // Check 127.0.0.0/8.
    if let Some(rest) = host.strip_prefix("127.") {
        return rest.split('.').all(|octet| octet.parse::<u8>().is_ok());
    }
    // Bracketed IPv6 loopback.
    if host == "[::1]" {
        return true;
    }
    false
}

/// Warn when deploying a local absolute path against a non-loopback control plane.
///
/// The path is resolved on the **control-plane host**, not the client machine.
/// Ctrl also requires `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1` (default off).
fn warn_remote_local_path_deploy(control_plane: &str, repo_url: &str) {
    let path = std::path::Path::new(repo_url);
    if !path.is_absolute() {
        return;
    }
    let host = control_plane_host(control_plane);
    if is_loopback_host(host) {
        return;
    }
    eprintln!(
        "\x1b[1;33mwarning:\x1b[0m deploying local path to non-loopback control plane \
         \x1b[1m{host}\x1b[0m — path is resolved on the \x1b[1mcontrol-plane host\x1b[0m, \
         not this machine. Ctrl rejects local paths unless \
         RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1 (single-tenant trusted hosts only). \
         Prefer a git URL for remote deploys."
    );
}

/// Extract host from a control-plane URL (`http://host:port` / `https://host/...`).
fn control_plane_host(control_plane: &str) -> &str {
    let rest = control_plane
        .strip_prefix("https://")
        .or_else(|| control_plane.strip_prefix("http://"))
        .unwrap_or(control_plane);
    let authority = rest.split('/').next().unwrap_or(rest);
    // Drop userinfo if present.
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    // Bracketed IPv6: [::1]:7878
    if let Some(inside) = host_port.strip_prefix('[') {
        return inside.split(']').next().unwrap_or(inside);
    }
    host_port.split(':').next().unwrap_or(host_port)
}

/// Map a reqwest error into a user-friendly message when the control plane is unreachable.
fn map_control_plane_error(err: reqwest::Error, control_plane: &str) -> anyhow::Error {
    if err.is_connect() {
        anyhow::anyhow!(
            "cannot reach control plane at {control_plane} (is russel-ctrl running? restart it, then retry)"
        )
    } else {
        anyhow::Error::from(err)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "russel",
    about = "Deploy Nix-built services into microVMs/containers"
)]
pub struct Cli {
    #[arg(
        long,
        env = "RUSSEL_CONTROL_PLANE",
        default_value = "http://127.0.0.1:7878"
    )]
    pub control_plane: String,

    /// Allow sending Bearer token over plain HTTP to non-loopback hosts.
    /// Prefer HTTPS (TLS reverse proxy) — see docs/security-tls.md.
    /// Also accepted via `RUSSEL_INSECURE_CLEARTEXT=1|true|yes`.
    #[arg(long)]
    pub insecure: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Deploy(DeployArgs),
    Status(StatusArgs),
    Logs(LogsArgs),
    Vms,
    Stop(StopArgs),
    Destroy(DestroyArgs),
    /// Re-apply desired state from the recorded Russelfile source (or override).
    Update(UpdateArgs),
    /// Manage host-side secrets (stored on the control plane, not in Russelfile).
    Secrets {
        #[command(subcommand)]
        action: SecretsCommand,
    },
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Service id to update.
    #[arg(value_name = "ID")]
    pub id: String,

    /// Override repo URL / local path (default: value recorded at last deploy).
    #[arg(long)]
    pub repo: Option<String>,

    /// Override config path relative to the repo (default: recorded or Russelfile.toml).
    #[arg(long)]
    pub config: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Store a secret value on the control plane.
    ///
    /// Value is read from stdin (not argv) so it does not appear in process lists.
    /// Example: `printf '%s' "$VAL" | russel secrets set NAME`
    Set { name: String },
    /// List secret names (values are never shown).
    List,
    /// Delete a secret.
    Delete { name: String },
}

#[derive(Debug, Args)]
pub struct DeployArgs {
    #[arg(value_name = "REPO")]
    pub repo: String,

    /// Publish a host port (e.g. 8080:3000). Optional: when omitted, Traefik
    /// provides the primary HTTP ingress via `http://<service_id>.russel.local`.
    #[arg(short = 'p', long = "publish", value_name = "HOST:GUEST")]
    pub port: Option<String>,

    #[arg(long, value_name = "ID")]
    pub vm_id: Option<String>,

    #[arg(long, default_value = "Russelfile.toml")]
    pub config: String,

    /// Runtime kind (`microvm` or `container`). Must match Russelfile `service.type` for local repos.
    #[arg(long, value_name = "RUNTIME")]
    pub runtime: Option<String>,

    /// Extra `podman run` arguments (container runtime only). Use `--` before flags if needed.
    #[arg(
        long_help = "Extra arguments forwarded to `podman run` when using container runtime. \
                     Russel sets detach, name, rootfs, port publish, memory, and entrypoint. \
                     Use `--` before flags if needed (e.g. `-- -v /data:/data:ro`).",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..
    )]
    pub podman_args: Vec<String>,

    /// Set an environment variable for the deployed service (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE", num_args = 1)]
    pub env: Vec<String>,

    /// Path to a file with KEY=VALUE lines (comments with #, blank lines skipped).
    #[arg(long = "env-file", value_name = "PATH")]
    pub env_file: Option<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    #[arg(value_name = "ID")]
    pub id: String,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(value_name = "ID")]
    pub service_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[arg(value_name = "ID")]
    pub service_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct DestroyArgs {
    #[arg(value_name = "ID")]
    pub id: String,
}

pub async fn deploy(args: DeployArgs, control_plane: &str) -> Result<()> {
    let wall = Instant::now();
    let repo_url = normalize_repo_arg(&args.repo)?;
    warn_remote_local_path_deploy(control_plane, &repo_url);
    let runtime = resolve_deploy_runtime(&args.repo, &args.config, args.runtime.as_deref())?;
    match runtime {
        Some(RuntimeKind::Microvm) if !args.podman_args.is_empty() => {
            anyhow::bail!(
                "podman passthrough args require container runtime (effective runtime is microvm)"
            );
        }
        None if !args.podman_args.is_empty() => {
            anyhow::bail!(
                "podman passthrough args require --runtime container (or a local Russelfile with type = \"container\")"
            );
        }
        _ => {}
    }
    let port = args.port.as_deref().map(parse_port_mapping).transpose()?;

    // ── Pre-flight banner ──────────────────────────────────────────────────
    println!();
    println!("  \x1b[1;36mrussel deploy\x1b[0m");
    println!("  \x1b[2m{}\x1b[0m", repo_url);
    println!();

    if let Some(vm_id) = &args.vm_id {
        step("vm-id", &format!("\x1b[1m{vm_id}\x1b[0m"), "");
    }
    if let Some(p) = &port {
        step(
            "publish",
            &format!(
                "localhost:\x1b[1m{}\x1b[0m → guest:\x1b[1m{}\x1b[0m",
                p.host, p.guest
            ),
            "",
        );
    }
    if let Some(runtime) = runtime {
        step("runtime", &runtime.to_string(), "");
    }
    if !args.podman_args.is_empty() {
        step("podman-args", &args.podman_args.join(" "), "");
    }
    println!();

    // ── Build env map from CLI args ───────────────────────────────────────
    let mut cli_env: HashMap<String, String> = HashMap::new();
    if let Some(ref env_file_path) = args.env_file {
        let file_env = parse_env_file(env_file_path)?;
        cli_env = merge_env_maps(&cli_env, &file_env);
    }
    for raw in &args.env {
        let (key, value) = parse_env_kv(raw)?;
        cli_env.insert(key, value);
    }
    // Validate CLI env before sending.
    validate_env_map(&cli_env)?;

    let client = http_client(control_plane)?;
    let mut response = client
        .post(format!("{control_plane}/deploy"))
        .json(&DeployRequest {
            repo_url,
            config_path: args.config,
            vm_id: args.vm_id,
            port,
            runtime,
            podman_args: args.podman_args,
            env: cli_env,
        })
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?;

    let response = stream_deploy_events(&mut response, "deploy").await?;

    println!();
    let status = response.status.clone();
    let message = response.message.clone();
    print_deploy_response(response, wall.elapsed());

    if !deploy_status_is_success(&status) {
        anyhow::bail!("deploy failed (status={status}): {message}");
    }

    Ok(())
}

/// Control-plane success status for a completed deploy stream.
///
/// Anything other than exact `"deployed"` is treated as failure so CI/scripts
/// get a non-zero process exit code (see issue #68).
fn deploy_status_is_success(status: &str) -> bool {
    status == "deployed"
}

fn step(label: &str, value: &str, suffix: &str) {
    println!("  \x1b[2m{label:>10}\x1b[0m  {value}{suffix}");
}

fn phase(label: &str, desc: &str) {
    println!(
        "  \x1b[2m{:>10}\x1b[0m  \x1b[2m· {}\x1b[0m",
        sanitize_terminal(label),
        sanitize_terminal(desc)
    );
}

fn ms(v: u128) -> String {
    if v >= 1000 {
        format!("{:.1}s", v as f64 / 1000.0)
    } else {
        format!("{v}ms")
    }
}

// ── Security: truncate raw NDJSON in error contexts (F-50) ──────────────

/// Truncate an embedded NDJSON line to 256 chars + length suffix for error
/// messages, so a hostile or buggy control plane cannot flood the terminal.
fn truncate_for_error(s: &str) -> String {
    const LIMIT: usize = 256;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    let mut out = String::with_capacity(LIMIT + 64);
    // Try to truncate at a valid char boundary.
    let trunc = if let Some((idx, _)) = s.char_indices().nth(LIMIT) {
        &s[..idx]
    } else {
        s
    };
    out.push_str(trunc);
    out.push_str(&format!("…[{} bytes total]", s.len()));
    out
}

// ── Security: strip terminal control sequences (F-51) ───────────────────

/// Strip C0 and C1 control characters (except `\t`, `\n`, `\r`) from
/// server-supplied strings before printing.  This prevents terminal escape
/// injection when the control plane (or a MITM on plain HTTP) is hostile.
fn sanitize_terminal(s: &str) -> String {
    s.chars().filter(|&c| !is_terminal_control(c)).collect()
}

fn is_terminal_control(c: char) -> bool {
    let u = c as u32;
    // C0: 0x00-0x1F (keep \t=0x09, \n=0x0A, \r=0x0D)
    if u <= 0x1F && u != 0x09 && u != 0x0A && u != 0x0D {
        return true;
    }
    // C1: 0x7F (DEL) and 0x80-0x9F
    if u == 0x7F || (0x80..=0x9F).contains(&u) {
        return true;
    }
    false
}

fn print_deploy_response(r: DeployResponse, wall: Duration) {
    let ok = r.status == "deployed";
    let rolled_back = r.status == "rolled_back";
    let icon = if ok {
        "\x1b[1;32m✓\x1b[0m"
    } else if rolled_back {
        "\x1b[1;33m↩\x1b[0m"
    } else {
        "\x1b[1;31m✗\x1b[0m"
    };
    let label = if ok {
        "Deployed"
    } else if rolled_back {
        "Rolled back"
    } else {
        "Failed"
    };

    println!(
        "  {icon} {label} in \x1b[1m{}\x1b[0m  (server: {})",
        ms(wall.as_millis()),
        ms(r.elapsed_ms)
    );
    println!();

    step("vm-id", &sanitize_terminal(&r.vm_id), "");
    step("status", &r.status, "");

    // ── Traefik route (primary ingress) ──────────────────────────────────
    if let Some(route_host) = &r.route_host {
        println!(
            "  \x1b[2m{:>10}\x1b[0m  \x1b[1mhttp://{}\x1b[0m  \x1b[2m(Traefik Host rule)\x1b[0m",
            "route",
            sanitize_terminal(route_host)
        );
    }

    if let Some(p) = &r.port {
        let guest = p.guest;
        let backend_label = format!("localhost:\x1b[1m{}\x1b[0m → guest:{}", p.host, guest);
        step(
            if r.route_host.is_some() {
                "backend"
            } else {
                "port"
            },
            &backend_label,
            "",
        );
    }
    if let Some(ip) = &r.vm_ip {
        let gp = r
            .port
            .as_ref()
            .map(|p| p.guest.to_string())
            .unwrap_or_default();
        step(
            "vm-ip",
            &format!("\x1b[2m{}\x1b[0m", sanitize_terminal(ip)),
            &format!(
                "  \x1b[2m(direct: curl {}:{})\x1b[0m",
                sanitize_terminal(ip),
                gp
            ),
        );
    }
    if let Some(store) = &r.store_path {
        step(
            "store",
            &format!("\x1b[2m{}\x1b[0m", sanitize_terminal(store)),
            "",
        );
    }
    if let Some(artifact) = &r.microvm_config_path {
        let label = if r
            .runtime
            .as_ref()
            .is_some_and(|rt| matches!(rt, RuntimeKind::Container))
        {
            "rootfs"
        } else {
            "initramfs"
        };
        step(
            label,
            &format!("\x1b[2m{}\x1b[0m", sanitize_terminal(artifact)),
            "",
        );
    }

    // ── Timing breakdown ───────────────────────────────────────────────────
    if let Some(t) = &r.timing {
        println!();
        println!("  \x1b[1;2mPhase timing:\x1b[0m");
        timing_row("resolve", t.resolve_ms, "repo + Russelfile");

        timing_row("build", t.build_ms, "build (package)");

        let is_container = r
            .runtime
            .as_ref()
            .is_some_and(|rt| matches!(rt, RuntimeKind::Container));
        if is_container {
            timing_row("create", t.create_ms, "prepare container rootfs");
            timing_row("start", t.start_ms, "start rootless Podman container");
            timing_row("ready", t.ready_ms, "container service reachable");
        } else {
            timing_row(
                "create",
                t.create_ms,
                "build minimal initramfs (BusyBox + modules)",
            );
            timing_row("network", t.network_ms, "TAP + socat port forwarding setup");
            timing_row(
                "start",
                t.start_ms,
                "spawn virtiofsd + boot cloud-hypervisor",
            );
            timing_row(
                "ready",
                t.ready_ms,
                "guest app network socket ready (VM is live)",
            );
        }
    }

    println!();
    println!("  \x1b[2mnote: {}\x1b[0m", sanitize_terminal(&r.message));

    if ok && let Some(p) = &r.port {
        println!();
        println!(
            "  \x1b[1mTest:\x1b[0m  curl -I http://127.0.0.1:{}/",
            p.host
        );
    }
    println!();
}

fn timing_row(label: &str, val_ms: u128, desc: &str) {
    let bar_len = ((val_ms / 200).min(30)) as usize;
    let bar = "█".repeat(bar_len);
    println!(
        "  \x1b[2m{label:>10}\x1b[0m  \x1b[1m{:>6}\x1b[0m  \x1b[32m{bar}\x1b[0m  \x1b[2m{desc}\x1b[0m",
        ms(val_ms)
    );
}

// ponytail: tests inline, no test framework ceremony for pure functions

fn resolve_deploy_runtime(
    repo: &str,
    config_path: &str,
    cli_runtime: Option<&str>,
) -> Result<Option<RuntimeKind>> {
    let cli = cli_runtime
        .map(|value| value.parse::<RuntimeKind>())
        .transpose()?;
    let path = PathBuf::from(repo);
    if path.exists() {
        let repo_root = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize local repo path {repo}"))?;
        let russelfile_path = repo_root.join(config_path);
        let config = Russelfile::load(&russelfile_path)
            .with_context(|| format!("failed to load {}", russelfile_path.display()))?;
        let resolved = resolve_runtime(config.service.runtime, cli)?;
        return Ok(Some(resolved));
    }
    Ok(cli)
}

fn normalize_repo_arg(repo: &str) -> Result<String> {
    // Expand leading '~' via $HOME.
    let repo = if let Some(rest) = repo.strip_prefix('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            anyhow::bail!("cannot expand '~' in repo path: $HOME is not set");
        }
        format!("{home}{rest}")
    } else {
        repo.to_string()
    };
    // Path-like: starts with '.', '~', '/', or contains a path separator.
    // Exclude URLs (http://, https://, ssh://, git@) from path-like detection.
    let is_url_like = repo.starts_with("http://")
        || repo.starts_with("https://")
        || repo.starts_with("ssh://")
        || repo.starts_with("git@");
    let is_path_like = !is_url_like
        && (repo.starts_with('.')
            || repo.starts_with('~')
            || repo.starts_with('/')
            || repo.contains(std::path::MAIN_SEPARATOR));
    if is_path_like {
        let path = PathBuf::from(&repo);
        if !path.exists() {
            anyhow::bail!("repo path does not exist: {repo}");
        }
        return Ok(path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize local repo path {}", repo))?
            .display()
            .to_string());
    }
    Ok(repo)
}

/// Parse a single `KEY=VALUE` string into a (key, value) pair.
fn parse_env_kv(raw: &str) -> Result<(String, String)> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("env must be KEY=VALUE, got: {raw}"))?;
    if key.is_empty() {
        anyhow::bail!("env key must not be empty (argument starts with =)");
    }
    Ok((key.to_string(), value.to_string()))
}

/// Parse a `--env-file` path.
///
/// Each non-empty line is KEY=VALUE.  Rules:
/// - UTF-8 BOM at the start of the file is stripped.
/// - Lines are trimmed; blank lines and `#`-only comments are skipped.
/// - Inline comments (` # …`) are stripped from the value *outside* quotes.
/// - Single- and double-quoted values are supported; the matching quote pair is
///   removed and inner `#` is kept.
/// - CRLF line endings are handled.
/// - `=` inside a quoted value is kept.
fn parse_env_file(path: &str) -> Result<HashMap<String, String>> {
    let mut contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read env file: {path}"))?;
    // Strip UTF-8 BOM if present.
    if contents.starts_with('\u{FEFF}') {
        contents = contents[3..].to_string();
    }
    let mut map = HashMap::new();
    for (i, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, rest) = trimmed
            .split_once('=')
            .ok_or_else(|| anyhow!("{}:{}: env line missing '=' separator", path, i + 1))?;
        if key.is_empty() {
            anyhow::bail!(
                "{}:{}: env key must not be empty (argument starts with =)",
                path,
                i + 1
            );
        }
        let value = parse_env_value(rest);
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

/// Strip surrounding quotes and inline comments from a raw RHS value.
fn parse_env_value(raw: &str) -> String {
    let raw = raw.trim();
    // Check for quoted value.
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.to_string();
    }
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.to_string();
    }
    // Strip inline comment: find first ` #` that is preceded by whitespace
    // (or at start of value after trimming).
    if let Some(pos) = raw.find(" #") {
        // Only strip if the space before # is preceded by a non-hash char
        // or is at position 0 (for values like `#comment`).
        // Actually: standard convention is ` # ` at word boundary.
        // We're already outside quotes, so any ` #` starts a comment.
        return raw[..pos].trim_end().to_string();
    }
    raw.to_string()
}

fn parse_port_mapping(value: &str) -> Result<PortMapping> {
    let (host, guest) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("port mapping must be HOST:GUEST, e.g. 8080:3000"))?;
    let host: u16 = host
        .parse()
        .with_context(|| format!("invalid host port in {value}"))?;
    if host == 0 {
        anyhow::bail!("host port must not be 0 in {value}");
    }
    let guest: u16 = guest
        .parse()
        .with_context(|| format!("invalid guest port in {value}"))?;
    Ok(PortMapping { host, guest })
}

// ponytail: only status/logs/vms/stop/destroy use HTTP — tested via unit tests
// on pure functions below.

pub async fn status(args: StatusArgs, control_plane: &str) -> Result<()> {
    let url = match args.service_id {
        Some(id) => format!("{control_plane}/vm/{id}/status"),
        None => format!("{control_plane}/status"),
    };
    let r = http_client(control_plane)?
        .get(&url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?
        .json::<StatusResponse>()
        .await?;
    println!("service_id={}", r.service_id);
    println!("status={}", r.status);
    println!("vm_state={}", r.vm_state);
    println!("uptime_seconds={}", r.uptime_seconds);
    if let Some(runtime) = r.runtime {
        println!("runtime={runtime}");
    }
    if let Some(host_port) = r.host_port {
        println!("host_port={host_port}");
    }
    if let Some(guest_port) = r.guest_port {
        println!("guest_port={guest_port}");
    }
    Ok(())
}

pub async fn logs(args: LogsArgs, control_plane: &str) -> Result<()> {
    let url = match args.service_id {
        Some(id) => format!("{control_plane}/vm/{id}/logs"),
        None => format!("{control_plane}/logs"),
    };
    let r = http_client(control_plane)?
        .get(&url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?
        .json::<LogsResponse>()
        .await?;
    print!("{}", r.output);
    Ok(())
}

pub async fn vms(control_plane: &str) -> Result<()> {
    let r = http_client(control_plane)?
        .get(format!("{control_plane}/vms"))
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?
        .json::<VmsResponse>()
        .await?;
    if r.vms.is_empty() {
        println!("no services registered");
        return Ok(());
    }

    if !r.services.is_empty() {
        for svc in &r.services {
            let runtime = svc
                .runtime
                .as_ref()
                .map(|r| format!("{r}"))
                .unwrap_or_else(|| "unknown".to_string());
            println!("{} runtime={runtime} status={}", svc.service_id, svc.status);
        }
    } else {
        for vm in r.vms {
            println!("{vm}");
        }
    }
    Ok(())
}

pub async fn stop_vm(id: &str, control_plane: &str) -> Result<()> {
    let r = http_client(control_plane)?
        .post(format!("{control_plane}/vm/{id}/stop"))
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?
        .json::<String>()
        .await?;
    println!("{}", r);
    Ok(())
}

pub async fn update(args: UpdateArgs, control_plane: &str) -> Result<()> {
    let wall = Instant::now();
    println!();
    println!("  \x1b[1;36mrussel update\x1b[0m  {}", args.id);
    println!();

    let mut body = serde_json::Map::new();
    if let Some(repo) = args.repo {
        let repo_url = normalize_repo_arg(&repo)?;
        warn_remote_local_path_deploy(control_plane, &repo_url);
        body.insert("repo_url".into(), serde_json::json!(repo_url));
    }
    if let Some(config) = args.config {
        body.insert("config_path".into(), serde_json::json!(config));
    }

    let client = http_client(control_plane)?;
    let mut response = client
        .post(format!("{control_plane}/vm/{}/update", args.id))
        .json(&body)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?
        .error_for_status()?;

    let response = stream_deploy_events(&mut response, "update").await?;
    let status = response.status.clone();
    print_deploy_response(response, wall.elapsed());
    if !deploy_status_is_success(&status) {
        anyhow::bail!("update finished with status {status}");
    }
    Ok(())
}

async fn stream_deploy_events(
    response: &mut reqwest::Response,
    operation: &str,
) -> Result<DeployResponse> {
    const MAX_NDJSON_LINE: usize = 8 * 1024 * 1024;
    let mut buffer = Vec::new();
    let mut final_response = None;

    while let Some(chunk) = response.chunk().await? {
        buffer.extend_from_slice(&chunk);
        while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
            if i > MAX_NDJSON_LINE {
                anyhow::bail!("control plane NDJSON line exceeded {MAX_NDJSON_LINE} bytes");
            }
            let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
            let line_bytes = &line_bytes[..line_bytes.len().saturating_sub(1)];
            if line_bytes.is_empty() || line_bytes.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let line = std::str::from_utf8(line_bytes)
                .context("control plane sent non-UTF-8 NDJSON line")?;
            let event: DeployEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse event from control plane: {}",
                    truncate_for_error(line)
                )
            })?;
            handle_deploy_event(event, operation, &mut final_response)?;
        }
        if buffer.len() > MAX_NDJSON_LINE {
            anyhow::bail!(
                "control plane NDJSON line exceeded {MAX_NDJSON_LINE} bytes without a newline"
            );
        }
    }

    if !buffer.is_empty() {
        if buffer.len() > MAX_NDJSON_LINE {
            anyhow::bail!(
                "control plane NDJSON line exceeded {MAX_NDJSON_LINE} bytes without a newline"
            );
        }
        let line = std::str::from_utf8(&buffer)
            .context("control plane sent non-UTF-8 final NDJSON record")?
            .trim();
        if !line.is_empty() {
            let event: DeployEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse final event from control plane: {}",
                    truncate_for_error(line)
                )
            })?;
            handle_deploy_event(event, operation, &mut final_response)?;
        }
    }

    final_response
        .map(|response| *response)
        .ok_or_else(|| anyhow!("control plane closed connection before {operation} completed"))
}

fn handle_deploy_event(
    event: DeployEvent,
    operation: &str,
    final_response: &mut Option<Box<DeployResponse>>,
) -> Result<()> {
    match event {
        DeployEvent::Progress {
            phase: p,
            description: d,
        } => phase(&p, &d),
        DeployEvent::Complete(response) => *final_response = Some(response),
        DeployEvent::Error(error) => anyhow::bail!("{operation} failed: {error}"),
    }
    Ok(())
}

pub async fn destroy_vm(id: &str, control_plane: &str) -> Result<()> {
    let resp = http_client(control_plane)?
        .delete(format!("{control_plane}/vm/{id}"))
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?;
    // Do not treat bare 404 as success: unmatched routes and "not in memory"
    // can 404 while runtime resources still exist. Server returns 200 with an
    // "already gone" body when destroy was intentionally idempotent.
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("destroy failed ({status}): {body}");
    }
    let r = resp.json::<String>().await?;
    println!("{}", r);
    Ok(())
}

pub async fn secrets(action: SecretsCommand, control_plane: &str) -> Result<()> {
    match action {
        SecretsCommand::Set { name } => {
            use std::io::Read;
            let mut value = String::new();
            std::io::stdin()
                .read_to_string(&mut value)
                .context("read secret value from stdin")?;
            // Trim a single trailing newline from terminal pipes.
            if value.ends_with('\n') {
                value.pop();
                if value.ends_with('\r') {
                    value.pop();
                }
            }
            if value.is_empty() {
                anyhow::bail!("secret value is empty (read value from stdin)");
            }
            let resp = http_client(control_plane)?
                .post(format!("{control_plane}/secrets/{name}"))
                .json(&serde_json::json!({ "value": value }))
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("secrets set failed ({status}): {body}");
            }
            println!("secret {name} stored");
        }
        SecretsCommand::List => {
            let resp = http_client(control_plane)?
                .get(format!("{control_plane}/secrets"))
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))?
                .error_for_status()?;
            let body: serde_json::Value = resp.json().await?;
            if let Some(arr) = body.get("secrets").and_then(|v| v.as_array()) {
                if arr.is_empty() {
                    println!("(no secrets)");
                } else {
                    for name in arr {
                        if let Some(s) = name.as_str() {
                            println!("{s}");
                        }
                    }
                }
            } else {
                println!("{body}");
            }
        }
        SecretsCommand::Delete { name } => {
            let resp = http_client(control_plane)?
                .delete(format!("{control_plane}/secrets/{name}"))
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("secrets delete failed ({status}): {body}");
            }
            println!("secret {name} deleted");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn deploy_status_success_only_deployed() {
        assert!(deploy_status_is_success("deployed"));
        assert!(!deploy_status_is_success("rolled_back"));
        assert!(!deploy_status_is_success("failed"));
        assert!(!deploy_status_is_success("building"));
        assert!(!deploy_status_is_success(""));
        assert!(!deploy_status_is_success("Deployed")); // case-sensitive
    }

    #[test]
    fn parse_port_mapping_valid() {
        let pm = parse_port_mapping("8080:3000").unwrap();
        assert_eq!(pm.host, 8080);
        assert_eq!(pm.guest, 3000);
    }

    #[test]
    fn parse_port_mapping_invalid_format() {
        assert!(parse_port_mapping("8080").is_err());
        assert!(parse_port_mapping("").is_err());
    }

    #[test]
    fn parse_port_mapping_host_zero_rejected() {
        let err = parse_port_mapping("0:3000").unwrap_err();
        assert!(err.to_string().contains("must not be 0"));
    }

    #[test]
    fn parse_port_mapping_non_numeric() {
        assert!(parse_port_mapping("abc:3000").is_err());
        assert!(parse_port_mapping("8080:xyz").is_err());
    }

    #[test]
    fn ms_under_second() {
        assert_eq!(ms(500), "500ms");
        assert_eq!(ms(0), "0ms");
        assert_eq!(ms(999), "999ms");
    }

    #[test]
    fn ms_over_second() {
        assert_eq!(ms(1000), "1.0s");
        assert_eq!(ms(1500), "1.5s");
        assert_eq!(ms(12345), "12.3s");
    }

    #[test]
    fn control_plane_host_parses_urls() {
        assert_eq!(control_plane_host("http://127.0.0.1:7878"), "127.0.0.1");
        assert_eq!(
            control_plane_host("https://ctrl.example.com/v1"),
            "ctrl.example.com"
        );
        assert_eq!(control_plane_host("http://[::1]:7878"), "::1");
        assert_eq!(control_plane_host("http://localhost"), "localhost");
    }

    #[test]
    fn is_loopback_host_covers_common_forms() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.1.2.3"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.1"));
        assert!(!is_loopback_host("ctrl.example.com"));
    }

    #[test]
    fn normalize_repo_arg_is_identity_for_remote() {
        let url = "https://github.com/user/repo.git";
        assert_eq!(normalize_repo_arg(url).unwrap(), url);
    }

    #[test]
    fn normalize_repo_arg_resolves_local_path() {
        let result = normalize_repo_arg(".").unwrap();
        // Should resolve to an absolute path
        assert!(result.starts_with('/'), "got: {result}");
        assert!(
            std::path::Path::new(&result).is_dir(),
            "path not found: {result}"
        );
    }

    #[test]
    fn normalize_repo_arg_pathlike_nonexistent_errors() {
        // Path-like arg that doesn't exist → error (F-52).
        let err = normalize_repo_arg("./nonexistent-dir-xyz").unwrap_err();
        assert!(
            err.to_string().contains("repo path does not exist"),
            "got: {err}"
        );
        // Non-pathlike names still pass through.
        let url = "https://github.com/user/repo.git";
        assert_eq!(normalize_repo_arg(url).unwrap(), url);
    }

    #[test]
    fn ms_edge_cases() {
        assert_eq!(ms(1000), "1.0s");
        assert_eq!(ms(999), "999ms");
        assert_eq!(ms(0), "0ms");
    }

    #[test]
    fn deploy_parses_trailing_podman_args_after_double_dash() {
        let cli = Cli::try_parse_from([
            "russel",
            "deploy",
            ".",
            "--runtime",
            "container",
            "--",
            "-v",
            "/a:/b",
        ])
        .unwrap();
        match cli.command {
            Command::Deploy(args) => {
                assert_eq!(args.podman_args, vec!["-v", "/a:/b"]);
            }
            _ => panic!("expected deploy subcommand"),
        }
    }

    #[test]
    fn parse_env_kv_valid() {
        let (k, v) = parse_env_kv("FOO=bar").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "bar");
    }

    #[test]
    fn parse_env_kv_equals_in_value() {
        let (k, v) = parse_env_kv("FOO=bar=baz").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "bar=baz");
    }

    #[test]
    fn parse_env_kv_no_equals() {
        assert!(parse_env_kv("FOOBAR").is_err());
    }

    #[test]
    fn parse_env_kv_empty_key() {
        let err = parse_env_kv("=value").unwrap_err();
        assert!(
            err.to_string().contains("argument starts with ="),
            "got: {}",
            err
        );
    }

    #[test]
    fn parse_env_file_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "LOG_LEVEL=info\n# comment\n\nFEATURE_X=1\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("LOG_LEVEL"), Some(&"info".to_string()));
        assert_eq!(map.get("FEATURE_X"), Some(&"1".to_string()));
        assert_eq!(map.len(), 2);
    }

    // ── F-49: parse_env_file with BOM, quotes, inline comments, CRLF ──────

    #[test]
    fn parse_env_file_bom() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        let bom = "\u{FEFF}";
        std::fs::write(&path, format!("{bom}KEY=val\n")).unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"val".to_string()));
    }

    #[test]
    fn parse_env_file_double_quoted_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY=\"value with spaces\"\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"value with spaces".to_string()));
    }

    #[test]
    fn parse_env_file_single_quoted_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY='value with spaces'\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"value with spaces".to_string()));
    }

    #[test]
    fn parse_env_file_inline_comment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY=value # this is a comment\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"value".to_string()));
    }

    #[test]
    fn parse_env_file_quoted_value_preserves_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY=\"value # not a comment\"\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"value # not a comment".to_string()));
    }

    #[test]
    fn parse_env_file_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY=val\r\nOTHER=foo\r\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"val".to_string()));
        assert_eq!(map.get("OTHER"), Some(&"foo".to_string()));
    }

    #[test]
    fn parse_env_file_equals_in_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("env.txt");
        std::fs::write(&path, "KEY=val=ue\n").unwrap();
        let map = parse_env_file(&path.to_string_lossy()).unwrap();
        assert_eq!(map.get("KEY"), Some(&"val=ue".to_string()));
    }

    // ── F-51: sanitize_terminal ───────────────────────────────────────────

    #[test]
    fn sanitize_terminal_strips_c0_controls() {
        assert_eq!(sanitize_terminal("hello\x00world"), "helloworld");
        assert_eq!(sanitize_terminal("a\x1b[31mred\x1b[0mb"), "a[31mred[0mb");
        // Tab, newline, carriage-return are kept.
        assert_eq!(sanitize_terminal("a\tb\nc\r"), "a\tb\nc\r");
    }

    #[test]
    fn sanitize_terminal_strips_c1_controls() {
        // DEL (0x7F) and 0x80-0x9F are stripped.
        assert_eq!(sanitize_terminal("x\x7fy"), "xy");
        assert_eq!(sanitize_terminal("a\u{0090}b"), "ab");
    }

    #[test]
    fn sanitize_terminal_passes_normal_text() {
        let normal = "Hello, world! 123 / path/to/file";
        assert_eq!(sanitize_terminal(normal), normal);
    }

    // ── F-50: truncate_for_error ──────────────────────────────────────────

    #[test]
    fn truncate_for_error_short() {
        assert_eq!(truncate_for_error("hello"), "hello");
    }

    #[test]
    fn truncate_for_error_long() {
        let long = "x".repeat(300);
        let truncated = truncate_for_error(&long);
        assert!(truncated.contains("…[300 bytes total]"));
        assert!(truncated.len() < 300);
    }

    // ── F-05 / #189: loopback detection + cleartext policy ────────────────

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.255.255.255"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("192.168.1.1"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host("127")); // not a full octet
    }

    #[test]
    fn extract_http_host_ipv4_and_ipv6() {
        assert_eq!(extract_http_host("127.0.0.1:7878/vms"), "127.0.0.1");
        assert_eq!(extract_http_host("example.com/foo"), "example.com");
        assert_eq!(extract_http_host("[::1]:7878"), "::1");
        assert_eq!(extract_http_host("[2001:db8::1]:443/x"), "2001:db8::1");
        assert_eq!(extract_http_host("192.168.1.1"), "192.168.1.1");
    }

    #[test]
    fn cleartext_policy_https_ok() {
        // No panic / no error for https regardless of host.
        ensure_cleartext_token_ok("https://example.com:7878").unwrap();
        ensure_cleartext_token_ok("https://192.168.1.1").unwrap();
    }

    #[test]
    fn cleartext_policy_loopback_http_ok() {
        // Loopback is warn-only (should not error).
        ensure_cleartext_token_ok("http://127.0.0.1:7878").unwrap();
        ensure_cleartext_token_ok("http://localhost:7878").unwrap();
        ensure_cleartext_token_ok("http://[::1]:7878").unwrap();
    }

    #[test]
    fn cleartext_policy_non_loopback_http_refuses() {
        // Reset escape hatch for isolation.
        set_cli_insecure(false);
        // Ensure env is not set for this test.
        // SAFETY: test process; we restore below.
        let prev = std::env::var("RUSSEL_INSECURE_CLEARTEXT").ok();
        unsafe { std::env::remove_var("RUSSEL_INSECURE_CLEARTEXT") };

        let err = ensure_cleartext_token_ok("http://192.168.1.10:7878").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to send RUSSEL_API_TOKEN"),
            "got: {msg}"
        );
        assert!(msg.contains("192.168.1.10"), "got: {msg}");
        assert!(msg.contains("--insecure"), "got: {msg}");

        let err = ensure_cleartext_token_ok("http://example.com/api").unwrap_err();
        assert!(err.to_string().contains("example.com"), "got: {}", err);

        match prev {
            Some(v) => unsafe { std::env::set_var("RUSSEL_INSECURE_CLEARTEXT", v) },
            None => unsafe { std::env::remove_var("RUSSEL_INSECURE_CLEARTEXT") },
        }
    }

    #[test]
    fn cleartext_policy_insecure_flag_allows_non_loopback() {
        set_cli_insecure(true);
        ensure_cleartext_token_ok("http://10.0.0.5:7878").unwrap();
        set_cli_insecure(false);
    }

    #[test]
    fn cleartext_policy_env_escape_allows_non_loopback() {
        set_cli_insecure(false);
        let prev = std::env::var("RUSSEL_INSECURE_CLEARTEXT").ok();
        unsafe { std::env::set_var("RUSSEL_INSECURE_CLEARTEXT", "1") };
        ensure_cleartext_token_ok("http://10.0.0.5:7878").unwrap();
        match prev {
            Some(v) => unsafe { std::env::set_var("RUSSEL_INSECURE_CLEARTEXT", v) },
            None => unsafe { std::env::remove_var("RUSSEL_INSECURE_CLEARTEXT") },
        }
    }

    #[test]
    fn insecure_flag_parses_on_cli() {
        let cli = Cli::try_parse_from([
            "russel",
            "--insecure",
            "--control-plane",
            "http://10.0.0.1:7878",
            "vms",
        ])
        .unwrap();
        assert!(cli.insecure);
        assert_eq!(cli.control_plane, "http://10.0.0.1:7878");
    }

    #[tokio::test]
    async fn map_control_plane_error_connect() {
        // Trigger a real connect error by hitting an unroutable port.
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        let mapped = map_control_plane_error(err, "http://127.0.0.1:7878");
        let msg = mapped.to_string();
        assert!(msg.contains("cannot reach control plane at"), "got: {msg}");
        assert!(msg.contains("is russel-ctrl running?"), "got: {msg}");
    }

    // Non-connect branch is just Error::from(err) — nothing worth unit-testing.

    #[test]
    fn deploy_parses_mount_style_podman_args() {
        let cli = Cli::try_parse_from([
            "russel",
            "deploy",
            ".",
            "--runtime",
            "container",
            "--",
            "--mount",
            "type=bind,source=/tmp/x,destination=/data",
        ])
        .unwrap();
        match cli.command {
            Command::Deploy(args) => {
                assert_eq!(
                    args.podman_args,
                    vec!["--mount", "type=bind,source=/tmp/x,destination=/data"]
                );
            }
            _ => panic!("expected deploy subcommand"),
        }
    }
}
