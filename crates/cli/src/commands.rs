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
        DeployEvent, DeployRequest, DeployResponse, LogsResponse, ServiceStatus, StatusResponse,
        VmsResponse,
    },
};

use crate::config::{self, Resolved};
use crate::init::InitArgs;
use crate::ui;

/// Shared HTTP client that attaches Bearer auth when RUSSEL_API_TOKEN is set.
///
/// Token handling matches ctrl `normalize_api_token`: trim whitespace; blank → no auth.
///
/// When a token is present and the control-plane URL uses scheme `http`
/// (any case — `HTTP://` is cleartext too):
/// - **non-loopback host** → hard-fail (F-05 / #189) unless `--insecure` or
///   `RUSSEL_INSECURE_CLEARTEXT=1|true|yes`
/// - **loopback host** → one-time stderr warning only
///
/// `https` (any case) is allowed. Any other `scheme://` is refused so a
/// bearer token is not sent on a scheme this client does not treat as TLS.
///
/// Returns an error when the token value cannot be parsed into a valid HTTP
/// header value (F-47).
/// Token from `RUSSEL_API_TOKEN` (any origin) or the saved config token when
/// the request URL matches the origin stored at login.
fn bearer_token(control_plane: &str) -> Option<String> {
    config::token_for(control_plane).map(|(token, _source)| token)
}

fn http_client(control_plane: &str) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(token) = bearer_token(control_plane) {
        let header_value = format!("Bearer {token}")
            .parse()
            .map_err(|_| anyhow!("API token contains characters invalid in an HTTP header"))?;
        headers.insert(AUTHORIZATION, header_value);
        // F-05 / #189: refuse cleartext Bearer to non-loopback; warn on loopback.
        ensure_cleartext_token_ok(control_plane)?;
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))
}

fn unauthorized_message(control_plane: &str) -> String {
    format!(
        "unauthorized (HTTP 401) at {control_plane}\n\
         \n\
         The control plane expects a Bearer token. Run:\n\
           russel login {control_plane}\n\
         or set RUSSEL_API_TOKEN.\n\
         Fish does not load KEY=VALUE files; `russel login` writes\n\
         ~/.config/russel/config.toml (mode 0600) so later shells work."
    )
}

fn ok_status(resp: reqwest::Response, control_plane: &str) -> Result<reqwest::Response> {
    if resp.status().as_u16() == 401 {
        anyhow::bail!("{}", unauthorized_message(control_plane));
    }
    Ok(resp.error_for_status()?)
}

fn fail_if_unauthorized(status: reqwest::StatusCode, control_plane: &str) -> Result<()> {
    if status.as_u16() == 401 {
        anyhow::bail!("{}", unauthorized_message(control_plane));
    }
    Ok(())
}

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
/// Scheme match is case-insensitive (`HTTP:` is cleartext; `HTTPS:` is not).
/// - no RFC 3986 scheme → ok (unclassified URL)
/// - `https` → ok
/// - `http` + loopback → ok (same host, or an SSH tunnel's local end)
/// - `http` + non-loopback → error unless insecure escape hatch (then warn once)
/// - any other scheme → error
fn ensure_cleartext_token_ok(control_plane: &str) -> Result<()> {
    let Some((scheme, rest)) = split_url_scheme(control_plane) else {
        return Ok(());
    };
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(anyhow!(
            "refusing to send RUSSEL_API_TOKEN over non-HTTP(S) scheme `{scheme}`\n\
             \n\
             Use HTTPS (terminate TLS at a reverse proxy in front of russel-ctrl — see docs/security-tls.md),\n\
             or target a loopback URL (e.g. http://127.0.0.1:7878)."
        ));
    }
    // `control_plane_host` splits on `/` first, so `//host` would yield empty.
    let rest = rest
        .strip_prefix("//")
        .or_else(|| rest.strip_prefix('/'))
        .unwrap_or(rest);
    let host = control_plane_host(rest);
    if is_loopback_host(host) {
        // Loopback never leaves the machine: the documented same-host setup, or
        // the local end of an SSH tunnel. A warning here fired on every command
        // and only taught people to ignore warnings (#527).
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

/// Split on the first `:` after an RFC 3986 scheme token.
///
/// Scheme is ALPHA, then ALPHA / DIGIT / "+" / "-" / ".". Rest is whatever
/// follows the colon (`://` is the authority marker, not the scheme).
/// Returned as written; callers compare it case-insensitively.
fn split_url_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once(':')?;
    let bytes = scheme.as_bytes();
    let first = bytes.first()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some((scheme, rest))
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

/// Extract the port from a control-plane URL (`http://host:port` /
/// `https://host:port/...`). Returns the default `7878` when the URL carries
/// no explicit port (covers `RUSSEL_CTRL_ADDR` overrides and custom binds).
fn control_plane_port(control_plane: &str) -> &str {
    let rest = control_plane
        .strip_prefix("https://")
        .or_else(|| control_plane.strip_prefix("http://"))
        .unwrap_or(control_plane);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    // Bracketed IPv6: [::1]:7878
    if let Some(after_bracket) = host_port
        .strip_prefix('[')
        .and_then(|inside| inside.split_once(']'))
        .map(|(_, rest)| rest)
    {
        return after_bracket
            .strip_prefix(':')
            .filter(|p| !p.is_empty())
            .unwrap_or("7878");
    }
    match host_port.rsplit_once(':') {
        Some((_, port)) if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) => port,
        _ => "7878",
    }
}

/// Map a reqwest error into a user-friendly message when the control plane is unreachable.
fn map_control_plane_error(err: reqwest::Error, control_plane: &str) -> anyhow::Error {
    if err.is_connect() {
        let host = control_plane_host(control_plane);
        if is_loopback_host(host) {
            let port = control_plane_port(control_plane);
            anyhow::anyhow!(
                "cannot reach control plane at {control_plane}: nothing is listening on this machine.\n\
                 \n\
                 Inspect the local service with:\n\
                   systemctl status russel-ctrl  # install.sh host\n\
                   systemctl status russel       # NixOS (services.russel)\n\
                 If the controller runs on another machine, create a local tunnel with:\n\
                   ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:{port}:127.0.0.1:{port} <user@host>"
            )
        } else {
            anyhow::anyhow!(
                "cannot reach control plane at {control_plane}: host cannot be reached.\n\
                 \n\
                 Check routing/VPN connectivity and the HTTPS/TLS reverse proxy in front of russel-ctrl."
            )
        }
    } else {
        anyhow::Error::from(err)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "russel",
    version,
    about = "Talk to russel-ctrl: deploy, list, logs, secrets"
)]
pub struct Cli {
    /// Control plane URL. Overrides `RUSSEL_CONTROL_PLANE` and `russel login`.
    #[arg(long, env = "RUSSEL_CONTROL_PLANE")]
    pub control_plane: Option<String>,

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
    /// Create a starter Russelfile.toml (and optionally flake.nix).
    Init(InitArgs),
    /// Save control-plane URL and API token to ~/.config/russel/config.toml.
    Login(LoginArgs),
    /// Remove the saved token from the config file (URL is kept).
    Logout,
    /// Show which control plane this CLI will talk to.
    Origin,
    /// Build and run the service a Russelfile describes. Applying a source the
    /// service already runs (same commit and Russelfile) is a no-op.
    #[command(visible_alias = "deploy")]
    Apply(DeployArgs),
    Status(StatusArgs),
    Logs(LogsArgs),
    /// List services (`list` is a visible alias; `vms` still works).
    #[command(visible_alias = "list", alias = "vms")]
    Ps,
    Stop(StopArgs),
    Destroy(DestroyArgs),
    /// Redeploy the recorded commit (or, with --refresh, the source's latest).
    Update(UpdateArgs),
    /// Redeploy an earlier generation from the deployment history.
    Rollback(RollbackArgs),
    /// Manage host-side secrets (stored on the control plane, not in Russelfile).
    Secrets {
        #[command(subcommand)]
        action: SecretsCommand,
    },
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Control plane URL (default: --control-plane, env, saved config, or loopback).
    #[arg(value_name = "URL")]
    pub url: Option<String>,

    /// Read the token from a file. Accepts a bare token or `RUSSEL_API_TOKEN=...`.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
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

    /// Build the source's current HEAD instead of the recorded commit.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// Service id to roll back.
    #[arg(value_name = "ID")]
    pub id: String,

    /// History version to redeploy (default: the previous generation).
    #[arg(long)]
    pub version: Option<u32>,
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

    #[arg(long, default_value = "Russelfile.toml")]
    pub config: String,

    /// Redeploy even when the service already runs this commit and Russelfile.
    #[arg(long)]
    pub force: bool,

    /// Trailing tokens are not accepted. Caught so the error can say where
    /// process argv (`service.args`) and podman flags (`service.podman_args`) go.
    #[arg(
        hide = true,
        trailing_var_arg = true,
        num_args = 0..
    )]
    pub trailing: Vec<String>,
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
    /// Keep all managed volume directories on destroy.
    #[arg(long, conflicts_with = "delete_volumes")]
    pub keep_volumes: bool,
    /// Delete all managed volume directories on destroy.
    #[arg(long, conflicts_with = "keep_volumes")]
    pub delete_volumes: bool,
}

pub async fn deploy(args: DeployArgs, control_plane: &str) -> Result<()> {
    let wall = Instant::now();
    reject_trailing_deploy_args(&args.trailing)?;
    let repo_url = normalize_repo_arg(&args.repo)?;
    warn_remote_local_path_deploy(control_plane, &repo_url);
    println!();
    println!("  \x1b[1;36mrussel apply\x1b[0m");
    println!("  \x1b[2m{}\x1b[0m", repo_url);
    println!();

    let client = http_client(control_plane)?;
    let mut response = client
        .post(format!("{control_plane}/deploy"))
        .json(&DeployRequest {
            repo_url,
            config_path: args.config,
            vm_id: None,
            rev: None,
            force: args.force,
        })
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?;

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
/// Only `"deployed"` and `"unchanged"` (the service already runs this source)
/// succeed, so CI/scripts get a non-zero exit code otherwise (see issue #68).
fn deploy_status_is_success(status: &str) -> bool {
    status == ServiceStatus::Deployed.as_str() || status == "unchanged"
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

/// Truncate an embedded NDJSON line to 256 chars + length suffix for error
/// messages, so a hostile or buggy control plane cannot flood the terminal.
fn truncate_for_error(s: &str) -> String {
    const LIMIT: usize = 256;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    let mut out = String::with_capacity(LIMIT + 64);
    // Prefer the char boundary of the LIMIT-th char; for multibyte strings
    // with fewer than LIMIT chars, fall back to the last char boundary at or
    // before LIMIT bytes so truncation always shortens the output.
    let end = match s.char_indices().nth(LIMIT) {
        Some((idx, _)) => idx,
        None => s
            .char_indices()
            .take_while(|&(idx, _)| idx <= LIMIT)
            .map(|(idx, _)| idx)
            .last()
            .unwrap_or(LIMIT),
    };
    out.push_str(&s[..end]);
    out.push_str(&format!("…[{} bytes total]", s.len()));
    out
}

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
    let unchanged = r.status == "unchanged";
    let ok = r.status == ServiceStatus::Deployed.as_str() || unchanged;
    let rolled_back = r.status == "rolled_back";
    let icon = if ok {
        "\x1b[1;32m✓\x1b[0m"
    } else if rolled_back {
        "\x1b[1;33m↩\x1b[0m"
    } else {
        "\x1b[1;31m✗\x1b[0m"
    };
    let label = if unchanged {
        "Unchanged"
    } else if ok {
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

    step("service", &sanitize_terminal(&r.service_id), "");
    step("status", &r.status, "");
    if let Some(rev) = &r.rev {
        step("rev", &sanitize_terminal(rev.get(..12).unwrap_or(rev)), "");
    }

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
            timing_row("ready", t.ready_ms, "app accepting connections");
        } else {
            timing_row(
                "create",
                t.create_ms,
                "build minimal initramfs (BusyBox + modules)",
            );
            timing_row(
                "network",
                t.network_ms,
                "port publishing (passt, or TAP + socat)",
            );
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

/// `russel deploy . -- --dir /data` used to forward the tokens to
/// `podman run`, which reads like process argv but never was. Reject them
/// and point at the two things the operator could have meant.
fn reject_trailing_deploy_args(trailing: &[String]) -> Result<()> {
    if trailing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "unexpected trailing arguments: {}\n\
         process argv goes in Russelfile service.args \
         (e.g. args = [\"--dir\", \"/data\"])\n\
         podman flags go in Russelfile service.podman_args \
         (e.g. podman_args = [\"-v\", \"/data:/data:ro\"])",
        trailing.join(" ")
    )
}

pub async fn login(args: LoginArgs, resolved: &Resolved) -> Result<()> {
    let url = args
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(resolved.control_plane.as_str())
        .to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("control plane URL must start with http:// or https:// (got {url})");
    }
    let token = read_login_token(&args)?;
    config::validate_token(&token)?;
    let mut cfg = config::load().unwrap_or_default();
    cfg.control_plane = Some(url.clone());
    cfg.token = Some(token);
    let path = config::save(&cfg)?;
    ui::heading("login");
    ui::kv("saved", &path.display().to_string());
    ui::kv("origin", &ui::sanitize(&url));
    ui::kv("auth", "token stored (not printed)");
    println!();
    println!(
        "  {}",
        ui::dim("Later shells read this file. Fish does not need KEY=VALUE exports.")
    );
    println!();
    Ok(())
}

pub fn logout() -> Result<()> {
    let mut cfg = config::load()?;
    cfg.token = None;
    let path = config::save(&cfg)?;
    ui::heading("logout");
    ui::kv("config", &path.display().to_string());
    ui::kv("auth", "token removed");
    println!();
    Ok(())
}

pub async fn origin(resolved: &Resolved) -> Result<()> {
    let host = control_plane_host(&resolved.control_plane);
    let auth = match (resolved.token.as_ref(), resolved.token_source) {
        (Some(_), Some(src)) => format!("token ({})", src.as_str()),
        (Some(_), None) => "token".to_string(),
        _ => "none".to_string(),
    };
    ui::heading("origin");
    ui::kv("url", &ui::sanitize(&resolved.control_plane));
    ui::kv("source", resolved.control_plane_source.as_str());
    ui::kv("host", host);
    ui::kv("auth", &auth);
    ui::kv("config", &resolved.config_path.display().to_string());
    match probe_origin(&resolved.control_plane).await {
        Ok(n) => {
            let word = if n == 1 { "service" } else { "services" };
            ui::kv("reachable", &format!("{}  ({n} {word})", ui::green("yes")));
        }
        Err(e) => {
            ui::kv("reachable", &format!("{}  ({e})", ui::red("no")));
        }
    }
    println!();
    Ok(())
}

async fn probe_origin(control_plane: &str) -> Result<usize> {
    let r = http_client(control_plane)?
        .get(format!("{control_plane}/vms"))
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<VmsResponse>()
        .await?;
    Ok(r.vms.len())
}

#[cfg(unix)]
struct TtyEchoGuard {
    fd: i32,
    orig: libc::termios,
}

#[cfg(unix)]
impl Drop for TtyEchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

#[cfg(unix)]
fn hide_tty_echo() -> Option<TtyEchoGuard> {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    unsafe {
        let mut term = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(fd, &mut term) != 0 {
            return None;
        }
        let orig = term;
        term.c_lflag &= !(libc::ECHO as libc::tcflag_t);
        if libc::tcsetattr(fd, libc::TCSANOW, &term) != 0 {
            return None;
        }
        Some(TtyEchoGuard { fd, orig })
    }
}

#[cfg(not(unix))]
fn hide_tty_echo() -> Option<()> {
    None
}

fn read_login_token(args: &LoginArgs) -> Result<String> {
    if let Some(path) = &args.token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read token file {}", path.display()))?;
        return parse_token_input(&raw);
    }
    if let Ok(raw) = std::env::var("RUSSEL_API_TOKEN") {
        let t = raw.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    use std::io::{BufRead, IsTerminal, Read, Write, stdin};
    let stdin = stdin();
    if stdin.is_terminal() {
        eprint!("API token: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let read = {
            let _guard = hide_tty_echo();
            stdin
                .lock()
                .read_line(&mut line)
                .context("read token from stdin")
        };
        eprintln!();
        read?;
        return parse_token_input(&line);
    }
    let mut value = String::new();
    stdin
        .lock()
        .read_to_string(&mut value)
        .context("read token from stdin")?;
    parse_token_input(&value)
}

fn parse_token_input(raw: &str) -> Result<String> {
    let mut bare: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("RUSSEL_API_TOKEN=") {
            let v = v.trim().trim_matches('"').trim().to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
        if !line.contains('=') && bare.is_none() {
            bare = Some(line.to_string());
        }
    }
    let trimmed = raw.trim();
    if !trimmed.is_empty() && !trimmed.contains('\n') && !trimmed.contains('=') {
        return Ok(trimmed.to_string());
    }
    bare.ok_or_else(|| {
        anyhow!(
            "no token found. Paste the token, or a line `RUSSEL_API_TOKEN=...`, \
             or pass --token-file"
        )
    })
}

pub async fn status(args: StatusArgs, control_plane: &str) -> Result<()> {
    let url = match &args.service_id {
        Some(id) => service_vm_url(control_plane, id, "/status")?,
        None => format!("{control_plane}/status"),
    };
    let r = http_client(control_plane)?
        .get(&url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<StatusResponse>()
        .await?;
    print_status(&r);
    Ok(())
}

fn print_status(r: &StatusResponse) {
    ui::heading(&ui::sanitize(&r.service_id));
    ui::kv("status", &ui::status_style(&r.status));
    ui::kv("state", &ui::status_style(&r.vm_state));
    if let Some(runtime) = r.runtime {
        ui::kv("runtime", &runtime.to_string());
    }
    match (r.host_port, r.guest_port) {
        (Some(h), Some(g)) => ui::kv("ports", &format!("{h} → {g}")),
        (Some(h), None) => ui::kv("host port", &h.to_string()),
        (None, Some(g)) => ui::kv("guest port", &g.to_string()),
        (None, None) => {}
    }
    ui::kv("uptime", &ui::format_uptime(r.uptime_seconds));
    println!();
}

pub async fn logs(args: LogsArgs, control_plane: &str) -> Result<()> {
    let url = match &args.service_id {
        Some(id) => service_vm_url(control_plane, id, "/logs")?,
        None => format!("{control_plane}/logs"),
    };
    let r = http_client(control_plane)?
        .get(&url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<LogsResponse>()
        .await?;
    let title = args.service_id.as_deref().unwrap_or("logs");
    ui::heading(title);
    ui::kv("origin", control_plane);
    println!("  {}", ui::dim("─".repeat(40).as_str()));
    print!("{}", ui::sanitize(&r.output));
    if !r.output.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub async fn ps(control_plane: &str) -> Result<()> {
    let r = http_client(control_plane)?
        .get(format!("{control_plane}/vms"))
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<VmsResponse>()
        .await?;
    println!();
    if r.vms.is_empty() {
        println!("  {}", ui::dim("no services"));
        println!();
        return Ok(());
    }

    let summaries: Vec<(String, String, String)> = if !r.services.is_empty() {
        r.services
            .iter()
            .map(|s| {
                (
                    s.service_id.clone(),
                    s.runtime
                        .as_ref()
                        .map(|rt| rt.to_string())
                        .unwrap_or_else(|| "—".into()),
                    s.status.clone(),
                )
            })
            .collect()
    } else {
        r.vms
            .iter()
            .map(|id| (id.clone(), "—".into(), "—".into()))
            .collect()
    };

    let mut rows = Vec::new();
    for (id, runtime, list_status) in summaries {
        let extra = fetch_status_row(control_plane, &id).await;
        let (state, ports, uptime, status) = match extra {
            Ok(st) => {
                let ports = match (st.host_port, st.guest_port) {
                    (Some(h), Some(g)) => format!("{h}→{g}"),
                    (Some(h), None) => h.to_string(),
                    _ => "—".into(),
                };
                (
                    ui::status_style(&st.vm_state),
                    ports,
                    ui::format_uptime(st.uptime_seconds),
                    ui::status_style(&st.status),
                )
            }
            Err(_) => (
                ui::dim("—"),
                "—".into(),
                "—".into(),
                ui::status_style(&list_status),
            ),
        };
        rows.push(vec![
            ui::bold(&ui::sanitize(&id)),
            runtime,
            status,
            state,
            ports,
            uptime,
        ]);
    }
    ui::table(
        &["ID", "RUNTIME", "STATUS", "STATE", "PORTS", "UPTIME"],
        &rows,
    );
    println!();
    Ok(())
}

async fn fetch_status_row(control_plane: &str, id: &str) -> Result<StatusResponse> {
    let url = service_vm_url(control_plane, id, "/status")?;
    http_client(control_plane)?
        .get(url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<StatusResponse>()
        .await
        .map_err(Into::into)
}

pub async fn stop_vm(id: &str, control_plane: &str) -> Result<()> {
    let url = service_vm_url(control_plane, id, "/stop")?;
    let r = http_client(control_plane)?
        .post(url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?
        .json::<String>()
        .await?;
    ui::heading("stop");
    ui::kv("id", &ui::sanitize(id));
    ui::kv("result", &ui::sanitize(&r));
    println!();
    Ok(())
}

pub async fn update(args: UpdateArgs, control_plane: &str) -> Result<()> {
    let url = service_vm_url(control_plane, &args.id, "/update")?;
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
    if args.refresh {
        body.insert("refresh".into(), serde_json::json!(true));
    }

    let client = http_client(control_plane)?;
    let mut response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?;

    let response = stream_deploy_events(&mut response, "update").await?;
    let status = response.status.clone();
    print_deploy_response(response, wall.elapsed());
    if !deploy_status_is_success(&status) {
        anyhow::bail!("update finished with status {status}");
    }
    Ok(())
}

pub async fn rollback(args: RollbackArgs, control_plane: &str) -> Result<()> {
    let url = service_vm_url(control_plane, &args.id, "/rollback")?;
    let wall = Instant::now();
    println!();
    println!("  \x1b[1;36mrussel rollback\x1b[0m  {}", args.id);
    println!();

    let mut body = serde_json::Map::new();
    if let Some(version) = args.version {
        body.insert("version".into(), serde_json::json!(version));
    }
    let client = http_client(control_plane)?;
    let mut response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))
        .and_then(|r| ok_status(r, control_plane))?;

    let response = stream_deploy_events(&mut response, "rollback").await?;
    let status = response.status.clone();
    print_deploy_response(response, wall.elapsed());
    if !deploy_status_is_success(&status) {
        anyhow::bail!("rollback finished with status {status}");
    }
    Ok(())
}

/// Maximum accepted length (bytes) for a single NDJSON line from the control
/// plane before we refuse it as hostile/buggy output.
const MAX_NDJSON_LINE: usize = 8 * 1024 * 1024;

/// Append one chunk to the NDJSON buffer and emit every complete
/// (`\n`-terminated) line through `on_line`, newline stripped. Empty and
/// whitespace-only lines (keepalives) are skipped. A partial trailing line is
/// left in `buffer`. Errors when any line exceeds `max_line` bytes or is not
/// valid UTF-8.
fn consume_ndjson_chunk(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    max_line: usize,
    on_line: &mut dyn FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    buffer.extend_from_slice(chunk);
    while let Some(i) = buffer.iter().position(|&b| b == b'\n') {
        if i > max_line {
            anyhow::bail!("control plane NDJSON line exceeded {max_line} bytes");
        }
        let line_bytes = buffer.drain(..=i).collect::<Vec<u8>>();
        let line_bytes = &line_bytes[..line_bytes.len() - 1];
        if line_bytes.is_empty() || line_bytes.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let line =
            std::str::from_utf8(line_bytes).context("control plane sent non-UTF-8 NDJSON line")?;
        on_line(line)?;
    }
    if buffer.len() > max_line {
        anyhow::bail!("control plane NDJSON line exceeded {max_line} bytes without a newline");
    }
    Ok(())
}

/// Emit the trailing partial record (no newline) left in `buffer` after the
/// stream ends, if any. Mirror of the loop in [`consume_ndjson_chunk`]: the
/// final record is trimmed before UTF-8/JSON handling.
fn consume_ndjson_final(
    buffer: &mut [u8],
    max_line: usize,
    on_line: &mut dyn FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    if buffer.len() > max_line {
        anyhow::bail!("control plane NDJSON line exceeded {max_line} bytes without a newline");
    }
    let line = std::str::from_utf8(buffer)
        .context("control plane sent non-UTF-8 final NDJSON record")?
        .trim();
    if !line.is_empty() {
        on_line(line)?;
    }
    Ok(())
}

async fn stream_deploy_events(
    response: &mut reqwest::Response,
    operation: &str,
) -> Result<DeployResponse> {
    let mut buffer = Vec::new();
    let mut final_response = None;

    while let Some(chunk) = response.chunk().await? {
        consume_ndjson_chunk(&mut buffer, &chunk, MAX_NDJSON_LINE, &mut |line| {
            let event: DeployEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse event from control plane: {}",
                    truncate_for_error(line)
                )
            })?;
            handle_deploy_event(event, operation, &mut final_response)
        })?;
    }

    consume_ndjson_final(&mut buffer, MAX_NDJSON_LINE, &mut |line| {
        let event: DeployEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse final event from control plane: {}",
                truncate_for_error(line)
            )
        })?;
        handle_deploy_event(event, operation, &mut final_response)
    })?;

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

/// Reject a service id before it is interpolated into a control-plane path.
///
/// Same rules as the control plane ([`russel_core::ids::validate_service_id`]):
/// dots and slashes are rejected so `..` cannot be resolved by the URL parser
/// into another route.
fn require_service_id(id: &str) -> Result<()> {
    russel_core::ids::validate_service_id(id)
        .map_err(|e| anyhow::anyhow!("invalid service id `{id}`: {e}"))
}

/// Mirror ctrl `validate_secret_name` (`crates/ctrl/src/secrets.rs`).
///
/// Length 1..=64, `[A-Za-z0-9_-]`, must not start with `-` or `.`.
fn require_secret_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("secret name must be 1..=64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("secret name may only contain [A-Za-z0-9_-]");
    }
    if name.starts_with('-') || name.starts_with('.') {
        anyhow::bail!("secret name must not start with '-' or '.'");
    }
    Ok(())
}

/// Percent-encode one URL path segment.
///
/// ASCII alphanumeric, `-`, and `_` are unchanged. Callers reject unsafe ids
/// first; encoding is defense in depth for that remaining charset.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// `/vm/{id}{suffix}` after [`require_service_id`]. `suffix` is a trusted
/// constant (`""`, `/status`, `/stop`, `/logs`, `/update`), not user input.
fn service_vm_url(base: &str, id: &str, suffix: &str) -> Result<String> {
    require_service_id(id)?;
    let id = encode_path_segment(id);
    Ok(format!("{base}/vm/{id}{suffix}"))
}

/// `/secrets/{name}` after [`require_secret_name`].
fn secret_url(base: &str, name: &str) -> Result<String> {
    require_secret_name(name)?;
    let name = encode_path_segment(name);
    Ok(format!("{base}/secrets/{name}"))
}

fn destroy_url(base: &str, id: &str, keep_volumes: bool, delete_volumes: bool) -> Result<String> {
    let mut url = service_vm_url(base, id, "")?;
    if keep_volumes {
        url.push_str("?keep_volumes=true");
    } else if delete_volumes {
        url.push_str("?keep_volumes=false");
    }
    Ok(url)
}

pub async fn destroy_vm(args: &DestroyArgs, control_plane: &str) -> Result<()> {
    let url = destroy_url(
        control_plane,
        &args.id,
        args.keep_volumes,
        args.delete_volumes,
    )?;
    let resp = http_client(control_plane)?
        .delete(url)
        .send()
        .await
        .map_err(|e| map_control_plane_error(e, control_plane))?;
    // Do not treat bare 404 as success: unmatched routes and "not in memory"
    // can 404 while runtime resources still exist. Server returns 200 with an
    // "already gone" body when destroy was intentionally idempotent.
    fail_if_unauthorized(resp.status(), control_plane)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("destroy failed ({status}): {body}");
    }
    let r = resp.json::<String>().await?;
    ui::heading("destroy");
    ui::kv("id", &ui::sanitize(&args.id));
    ui::kv("result", &ui::sanitize(&r));
    println!();
    Ok(())
}

pub async fn secrets(action: SecretsCommand, control_plane: &str) -> Result<()> {
    match action {
        SecretsCommand::Set { name } => {
            use std::io::Read;
            let url = secret_url(control_plane, &name)?;
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
                .post(url)
                .json(&serde_json::json!({ "value": value }))
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))?;
            fail_if_unauthorized(resp.status(), control_plane)?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("secrets set failed ({status}): {body}");
            }
            ui::heading("secrets");
            ui::kv("set", &ui::sanitize(&name));
            println!();
        }
        SecretsCommand::List => {
            let resp = http_client(control_plane)?
                .get(format!("{control_plane}/secrets"))
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))
                .and_then(|r| ok_status(r, control_plane))?;
            let body: serde_json::Value = resp.json().await?;
            ui::heading("secrets");
            if let Some(arr) = body.get("secrets").and_then(|v| v.as_array()) {
                if arr.is_empty() {
                    println!("  {}", ui::dim("(none)"));
                } else {
                    for name in arr {
                        if let Some(s) = name.as_str() {
                            println!("  {}", ui::sanitize(s));
                        }
                    }
                }
            } else {
                println!("  {body}");
            }
            println!();
        }
        SecretsCommand::Delete { name } => {
            let url = secret_url(control_plane, &name)?;
            let resp = http_client(control_plane)?
                .delete(url)
                .send()
                .await
                .map_err(|e| map_control_plane_error(e, control_plane))?;
            fail_if_unauthorized(resp.status(), control_plane)?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("secrets delete failed ({status}): {body}");
            }
            ui::heading("secrets");
            ui::kv("deleted", &ui::sanitize(&name));
            println!();
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
    fn destroy_url_maps_keep_and_delete_volume_flags() {
        let base = "http://127.0.0.1:7878";
        assert_eq!(
            destroy_url(base, "svc", false, false).unwrap(),
            "http://127.0.0.1:7878/vm/svc"
        );
        assert_eq!(
            destroy_url(base, "svc", true, false).unwrap(),
            "http://127.0.0.1:7878/vm/svc?keep_volumes=true"
        );
        assert_eq!(
            destroy_url(base, "svc", false, true).unwrap(),
            "http://127.0.0.1:7878/vm/svc?keep_volumes=false"
        );
    }

    #[test]
    fn destroy_url_rejects_path_escape_service_ids() {
        let base = "http://127.0.0.1:7878";
        let traversal = destroy_url(base, "../secrets/token", false, false).unwrap_err();
        assert!(
            traversal.to_string().contains("invalid service id"),
            "got: {traversal}"
        );
        assert!(destroy_url(base, "foo/bar", false, false).is_err());
        assert!(require_service_id("../secrets/token").is_err());
        assert!(require_service_id("foo/bar").is_err());
        assert!(require_service_id("foo/../../secrets/token").is_err());
        assert!(require_service_id("").is_err());
        assert!(require_service_id(&"a".repeat(129)).is_err());
        assert!(require_service_id("has.dot").is_err());
        // Service ids are directory names and URL path segments; keep ASCII.
        assert!(require_service_id("café").is_err());
        assert!(require_service_id("svc.with.dot").is_err());
    }

    #[test]
    fn destroy_url_accepts_normal_service_id() {
        let url = destroy_url("http://127.0.0.1:7878", "my-svc_1", false, false).unwrap();
        assert_eq!(url, "http://127.0.0.1:7878/vm/my-svc_1");
        assert!(url.split('?').next().unwrap().ends_with("/vm/my-svc_1"));
        require_service_id("my-svc_1").unwrap();
        assert_eq!(encode_path_segment("my-svc_1"), "my-svc_1");
        // Unsafe bytes are encoded, but callers reject them before building a URL.
        assert_eq!(
            encode_path_segment("../secrets/token"),
            "%2E%2E%2Fsecrets%2Ftoken"
        );
    }

    #[test]
    fn require_secret_name_rejects_path_escape() {
        assert!(require_secret_name("../secrets/token").is_err());
        assert!(require_secret_name("foo/bar").is_err());
        assert!(require_secret_name("-leading").is_err());
        assert!(require_secret_name("").is_err());
        require_secret_name("good_name-1").unwrap();
        let url = secret_url("http://127.0.0.1:7878", "good_name-1").unwrap();
        assert_eq!(url, "http://127.0.0.1:7878/secrets/good_name-1");
    }

    #[test]
    fn destroy_keep_and_delete_volumes_conflict_in_clap() {
        let err = Cli::try_parse_from([
            "russel",
            "destroy",
            "svc",
            "--keep-volumes",
            "--delete-volumes",
        ])
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--keep-volumes") && msg.contains("--delete-volumes"),
            "unexpected err: {msg}"
        );
    }

    #[test]
    fn deploy_status_success_is_deployed_or_unchanged() {
        assert!(deploy_status_is_success("deployed"));
        assert!(!deploy_status_is_success("rolled_back"));
        assert!(deploy_status_is_success("unchanged"));
        assert!(!deploy_status_is_success("failed"));
        assert!(!deploy_status_is_success("building"));
        assert!(!deploy_status_is_success(""));
        assert!(!deploy_status_is_success("Deployed")); // case-sensitive
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
    fn deploy_rejects_removed_flags() {
        for args in [
            vec!["russel", "deploy", ".", "-p", "8080:3000"],
            vec!["russel", "deploy", ".", "--vm-id", "x"],
            vec!["russel", "deploy", ".", "--env", "A=1"],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn deploy_rejects_trailing_tokens_with_guidance() {
        // `--dir /data` is process argv and belongs in service.args.
        let cli = Cli::try_parse_from(["russel", "deploy", ".", "--", "--dir", "/data"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected deploy subcommand");
        };
        assert_eq!(args.trailing, vec!["--dir", "/data"]);
        let msg = reject_trailing_deploy_args(&args.trailing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("--dir /data"), "{msg}");
        assert!(msg.contains("service.args"), "{msg}");
        assert!(msg.contains("service.podman_args"), "{msg}");
    }

    #[test]
    fn version_flag_is_display_version() {
        let err = Cli::try_parse_from(["russel", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains("0.1.0"));
    }

    #[test]
    fn apply_deploy_alias_update_refresh_and_rollback_parse() {
        for name in ["apply", "deploy"] {
            let cli = Cli::try_parse_from(["russel", name, ".", "--force"]).unwrap();
            let Command::Apply(args) = cli.command else {
                panic!("expected Apply for {name}");
            };
            assert!(args.force);
        }
        let cli = Cli::try_parse_from(["russel", "update", "api", "--refresh"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Update(UpdateArgs { refresh: true, .. })
        ));
        let cli = Cli::try_parse_from(["russel", "rollback", "api", "--version", "3"]).unwrap();
        let Command::Rollback(args) = cli.command else {
            panic!("expected Rollback");
        };
        assert_eq!((args.id.as_str(), args.version), ("api", Some(3)));
    }

    #[test]
    fn ps_list_vms_aliases_parse() {
        for name in ["ps", "list", "vms"] {
            let cli = Cli::try_parse_from(["russel", name]).unwrap();
            assert!(matches!(cli.command, Command::Ps), "expected Ps for {name}");
        }
    }

    #[test]
    fn login_and_origin_parse() {
        let login = Cli::try_parse_from([
            "russel",
            "login",
            "http://127.0.0.1:7878",
            "--token-file",
            "/tmp/t",
        ])
        .unwrap();
        match login.command {
            Command::Login(args) => {
                assert_eq!(args.url.as_deref(), Some("http://127.0.0.1:7878"));
                assert_eq!(
                    args.token_file.as_deref(),
                    Some(std::path::Path::new("/tmp/t"))
                );
            }
            _ => panic!("expected login"),
        }
        let origin = Cli::try_parse_from(["russel", "origin"]).unwrap();
        assert!(matches!(origin.command, Command::Origin));
        let logout = Cli::try_parse_from(["russel", "logout"]).unwrap();
        assert!(matches!(logout.command, Command::Logout));
    }

    #[test]
    fn parse_token_from_env_file_line() {
        let t = parse_token_input(
            "RUSSEL_CONTROL_PLANE=http://127.0.0.1:7878\nRUSSEL_API_TOKEN=abc123\n",
        )
        .unwrap();
        assert_eq!(t, "abc123");
    }

    #[test]
    fn parse_token_bare_line() {
        assert_eq!(parse_token_input("  deadbeef  \n").unwrap(), "deadbeef");
    }

    #[test]
    fn init_is_a_cli_subcommand() {
        let cli = Cli::try_parse_from([
            "russel",
            "init",
            "./svc",
            "--name",
            "svc",
            "--type",
            "container",
            "--with-flake",
        ])
        .unwrap();
        match cli.command {
            Command::Init(args) => {
                assert_eq!(args.path, PathBuf::from("./svc"));
                assert_eq!(args.name.as_deref(), Some("svc"));
                assert_eq!(args.runtime, Some(RuntimeKind::Container));
                assert!(args.with_flake);
            }
            _ => panic!("expected init subcommand"),
        }
    }

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

    #[test]
    fn truncate_for_error_ascii_over_limit() {
        // Pure ASCII, byte length > 256: truncation must shorten the output.
        let long = "a".repeat(1000);
        let truncated = truncate_for_error(&long);
        assert!(truncated.len() < long.len(), "must truncate ASCII input");
        assert!(truncated.contains("…[1000 bytes total]"));
    }

    #[test]
    fn truncate_for_error_multibyte_fewer_chars_than_limit() {
        // 'é' is 2 bytes: 200 chars = 400 bytes > 256, but char count <= 256.
        // The old code fell back to the whole string; truncation must happen.
        let long = "é".repeat(200);
        assert!(long.len() > 256);
        assert!(long.chars().count() <= 256);
        let truncated = truncate_for_error(&long);
        assert!(
            truncated.len() < long.len(),
            "must truncate multibyte input with few chars"
        );
        assert!(truncated.contains("…[400 bytes total]"));
    }

    #[test]
    fn truncate_for_error_multibyte_many_chars() {
        // '日' is 3 bytes: 300 chars = 900 bytes > 256, char count > 256.
        let long = "日".repeat(300);
        let truncated = truncate_for_error(&long);
        assert!(
            truncated.len() < long.len(),
            "must truncate multibyte input with many chars"
        );
        assert!(truncated.contains("…[900 bytes total]"));
        // Must not slice into the middle of a char: the kept prefix ends on a
        // char boundary, so re-encoding round-trips cleanly.
        let kept = truncated.split('…').next().unwrap();
        assert!(kept.is_char_boundary(kept.len()));
    }

    /// Serialize mutations of `RUSSEL_INSECURE_CLEARTEXT` and `CLI_INSECURE`.
    /// Restores both on drop (panic-safe). Fixes #302 parallel test races.
    fn with_cleartext_env<T>(
        env_value: Option<&str>,
        cli_insecure: bool,
        f: impl FnOnce() -> T,
    ) -> T {
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
                        Some(v) => std::env::set_var("RUSSEL_INSECURE_CLEARTEXT", v),
                        None => std::env::remove_var("RUSSEL_INSECURE_CLEARTEXT"),
                    }
                }
            }
        }

        struct InsecureRestore(bool);
        impl Drop for InsecureRestore {
            fn drop(&mut self) {
                set_cli_insecure(self.0);
            }
        }

        let prev_env = std::env::var_os("RUSSEL_INSECURE_CLEARTEXT");
        let prev_flag = CLI_INSECURE.load(std::sync::atomic::Ordering::Relaxed);
        let _restore_env = EnvRestore(prev_env);
        let _restore_flag = InsecureRestore(prev_flag);

        set_cli_insecure(cli_insecure);
        // SAFETY: exclusive lock held for the duration of the mutation + body.
        unsafe {
            match env_value {
                Some(v) => std::env::set_var("RUSSEL_INSECURE_CLEARTEXT", v),
                None => std::env::remove_var("RUSSEL_INSECURE_CLEARTEXT"),
            }
        }
        f()
    }

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
    fn control_plane_host_strips_scheme_userinfo_and_port() {
        assert_eq!(control_plane_host("127.0.0.1:7878/vms"), "127.0.0.1");
        assert_eq!(control_plane_host("example.com/foo"), "example.com");
        assert_eq!(control_plane_host("[::1]:7878"), "::1");
        assert_eq!(control_plane_host("[2001:db8::1]:443/x"), "2001:db8::1");
        assert_eq!(control_plane_host("192.168.1.1"), "192.168.1.1");
        assert_eq!(
            control_plane_host("http://user:pass@127.0.0.1:7878"),
            "127.0.0.1"
        );
        assert_eq!(
            control_plane_host("https://user:pass@ctrl.example.com/v1"),
            "ctrl.example.com"
        );
    }

    #[test]
    fn cleartext_policy_https_ok() {
        // No panic / no error for https regardless of host.
        with_cleartext_env(None, false, || {
            ensure_cleartext_token_ok("https://example.com:7878").unwrap();
            ensure_cleartext_token_ok("https://192.168.1.1").unwrap();
        });
    }

    #[test]
    fn cleartext_policy_scheme_is_case_insensitive() {
        // Scheme compare is case-insensitive. Other schemes fail closed.
        // RFC 3986 scheme is `scheme:`; `://` is not required to classify.
        with_cleartext_env(None, false, || {
            let err = ensure_cleartext_token_ok("HTTP://203.0.113.5:7878").unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("refusing to send RUSSEL_API_TOKEN"),
                "got: {msg}"
            );
            assert!(msg.contains("203.0.113.5"), "got: {msg}");

            ensure_cleartext_token_ok("HTTPS://example.com").unwrap();
            ensure_cleartext_token_ok("Http://127.0.0.1:7878").unwrap();

            let err = ensure_cleartext_token_ok("http:/203.0.113.5").unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("refusing to send RUSSEL_API_TOKEN"),
                "got: {msg}"
            );
            assert!(msg.contains("203.0.113.5"), "got: {msg}");

            let err = ensure_cleartext_token_ok("http:203.0.113.5").unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("refusing to send RUSSEL_API_TOKEN"),
                "got: {msg}"
            );
            assert!(msg.contains("203.0.113.5"), "got: {msg}");

            for url in [
                "file:///tmp/sock",
                "file:/tmp/sock",
                "unix:///tmp/russel.sock",
            ] {
                let err = ensure_cleartext_token_ok(url).unwrap_err();
                assert!(
                    err.to_string().contains("non-HTTP(S) scheme"),
                    "{url}: {err}"
                );
            }
        });
    }

    #[test]
    fn cleartext_policy_loopback_http_ok() {
        // Loopback is warn-only (should not error).
        with_cleartext_env(None, false, || {
            ensure_cleartext_token_ok("http://127.0.0.1:7878").unwrap();
            ensure_cleartext_token_ok("http://localhost:7878").unwrap();
            ensure_cleartext_token_ok("http://[::1]:7878").unwrap();
        });
    }

    #[test]
    fn cleartext_policy_non_loopback_http_refuses() {
        with_cleartext_env(None, false, || {
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
        });
    }

    #[test]
    fn cleartext_policy_insecure_flag_allows_non_loopback() {
        with_cleartext_env(None, true, || {
            ensure_cleartext_token_ok("http://10.0.0.5:7878").unwrap();
        });
    }

    #[test]
    fn cleartext_policy_env_escape_allows_non_loopback() {
        with_cleartext_env(Some("1"), false, || {
            ensure_cleartext_token_ok("http://10.0.0.5:7878").unwrap();
        });
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
        assert_eq!(cli.control_plane.as_deref(), Some("http://10.0.0.1:7878"));
        assert!(matches!(cli.command, Command::Ps));
    }

    async fn connect_error_to_closed_ephemeral_loopback() -> reqwest::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let err = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_connect(), "expected a connect error, got: {err}");
        err
    }

    #[test]
    fn control_plane_port_parses_urls() {
        assert_eq!(control_plane_port("http://127.0.0.1:7878"), "7878");
        assert_eq!(control_plane_port("http://127.0.0.1:9999"), "9999");
        assert_eq!(
            control_plane_port("https://ctrl.example.com:8443/api"),
            "8443"
        );
        assert_eq!(control_plane_port("http://[::1]:7878"), "7878");
        assert_eq!(control_plane_port("http://[::1]:9999/vms"), "9999");
        // No explicit port (default bind, bare host, userinfo) → default.
        assert_eq!(control_plane_port("http://127.0.0.1"), "7878");
        assert_eq!(control_plane_port("http://localhost/vms"), "7878");
        assert_eq!(
            control_plane_port("http://user@ctrl.example.com/vms"),
            "7878"
        );
    }

    #[tokio::test]
    async fn map_control_plane_error_connect_loopback() {
        let err = connect_error_to_closed_ephemeral_loopback().await;
        let mapped = map_control_plane_error(err, "http://127.0.0.1:7878");
        let msg = mapped.to_string();
        assert!(
            msg.contains("nothing is listening on this machine"),
            "got: {msg}"
        );
        assert!(
            msg.contains("systemctl status russel-ctrl  # install.sh host"),
            "got: {msg}"
        );
        assert!(
            msg.contains("systemctl status russel       # NixOS"),
            "got: {msg}"
        );
        assert!(
            msg.contains(
                "ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:7878:127.0.0.1:7878 <user@host>"
            ),
            "got: {msg}"
        );
        assert!(!msg.contains("routing/VPN"), "got: {msg}");
        assert!(!msg.contains("HTTPS/TLS reverse proxy"), "got: {msg}");
    }

    #[tokio::test]
    async fn map_control_plane_error_connect_loopback_custom_port() {
        let err = connect_error_to_closed_ephemeral_loopback().await;
        let mapped = map_control_plane_error(err, "http://127.0.0.1:9999");
        let msg = mapped.to_string();
        assert!(
            msg.contains(
                "ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:9999:127.0.0.1:9999 <user@host>"
            ),
            "got: {msg}"
        );
        assert!(!msg.contains("127.0.0.1:7878"), "got: {msg}");
    }

    #[tokio::test]
    async fn map_control_plane_error_connect_remote() {
        let err = connect_error_to_closed_ephemeral_loopback().await;
        let mapped = map_control_plane_error(err, "https://ctrl.example.com:7878");
        let msg = mapped.to_string();
        assert!(msg.contains("host cannot be reached"), "got: {msg}");
        assert!(msg.contains("routing/VPN"), "got: {msg}");
        assert!(msg.contains("HTTPS/TLS reverse proxy"), "got: {msg}");
        assert!(
            !msg.contains("systemctl status russel-ctrl  # install.sh host"),
            "got: {msg}"
        );
        assert!(!msg.contains("ssh -f -N"), "got: {msg}");
    }

    #[tokio::test]
    async fn map_control_plane_error_preserves_non_connect_error() {
        let err = reqwest::Client::builder()
            .build()
            .unwrap()
            .get("not a URL")
            .send()
            .await
            .unwrap_err();
        assert!(
            !err.is_connect(),
            "expected a non-connect error, got: {err}"
        );

        let original = err.to_string();
        let mapped = map_control_plane_error(err, "http://127.0.0.1:7878");
        let msg = mapped.to_string();
        assert_eq!(msg, original);
        for guidance in [
            "nothing is listening on this machine",
            "systemctl status russel-ctrl",
            "ssh -f -N",
            "host cannot be reached",
            "routing/VPN",
            "HTTPS/TLS reverse proxy",
        ] {
            assert!(
                !msg.contains(guidance),
                "unexpected topology guidance: {msg}"
            );
        }
    }

    fn collect_lines(chunks: &[&[u8]], trailing: bool) -> Vec<String> {
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        for chunk in chunks {
            consume_ndjson_chunk(&mut buffer, chunk, MAX_NDJSON_LINE, &mut |line| {
                lines.push(line.to_string());
                Ok(())
            })
            .unwrap();
        }
        if trailing {
            consume_ndjson_final(&mut buffer, MAX_NDJSON_LINE, &mut |line| {
                lines.push(line.to_string());
                Ok(())
            })
            .unwrap();
        }
        lines
    }

    #[test]
    fn ndjson_complete_lines_in_one_chunk() {
        let lines = collect_lines(&[b"{\"a\":1}\n{\"b\":2}\n".as_slice()], false);
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn ndjson_line_split_across_chunks() {
        let lines = collect_lines(
            &[b"{\"ph".as_slice(), b"ase\":\"build\"}\n".as_slice()],
            false,
        );
        assert_eq!(lines, vec!["{\"phase\":\"build\"}"]);
    }

    #[test]
    fn ndjson_split_mid_crlf_boundary() {
        // Split inside the `\r\n` pair that terminates a line (and the JSON).
        // The splitter strips only `\n`; a trailing `\r` is preserved and left
        // for serde_json (which treats `\r` as JSON whitespace), matching the
        // pre-existing wire behavior.
        let full = "{\"x\":1}\r\n{\"y\":2}\n".as_bytes();
        let lines = collect_lines(&[&full[..8], &full[8..]], false);
        assert_eq!(lines, vec!["{\"x\":1}\r", "{\"y\":2}"]);
    }

    #[test]
    fn ndjson_trailing_partial_line_without_newline() {
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        consume_ndjson_chunk(
            &mut buffer,
            b"{\"x\":1}\n{\"z\":".as_slice(),
            MAX_NDJSON_LINE,
            &mut |l| {
                lines.push(l.to_string());
                Ok(())
            },
        )
        .unwrap();
        // Trailing partial record is not emitted until the stream ends.
        assert_eq!(lines, vec!["{\"x\":1}"]);
        consume_ndjson_final(&mut buffer, MAX_NDJSON_LINE, &mut |l| {
            lines.push(l.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(lines, vec!["{\"x\":1}", "{\"z\":"]);
    }

    #[test]
    fn ndjson_empty_keepalives_are_skipped() {
        let lines = collect_lines(&[b"\n\n   \n{\"ok\":true}\n\t\n".as_slice()], false);
        assert_eq!(lines, vec!["{\"ok\":true}"]);
    }

    #[test]
    fn ndjson_oversized_line_rejected() {
        // A small max reproduces the same guard as the 8 MiB production limit.
        let mut buffer = Vec::new();
        let err = consume_ndjson_chunk(
            &mut buffer,
            b"{\"long\":\"xxxxxxxxxxxxxxxx\"}\n".as_slice(),
            16,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("exceeded"), "got: {err}");
    }

    #[test]
    fn ndjson_oversized_partial_without_newline_rejected() {
        let mut buffer = Vec::new();
        let err = consume_ndjson_chunk(
            &mut buffer,
            b"xxxxxxxxxxxxxxxxx".as_slice(),
            16,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("without a newline"), "got: {err}");
    }

    #[test]
    fn ndjson_non_utf8_line_rejected() {
        let mut buffer = Vec::new();
        let err = consume_ndjson_chunk(
            &mut buffer,
            &[0xff, 0xfe, b'\n'],
            MAX_NDJSON_LINE,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-UTF-8"), "got: {err}");
    }
}
