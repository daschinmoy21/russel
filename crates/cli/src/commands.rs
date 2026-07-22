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
fn http_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    if let Ok(token) = std::env::var("RUSSEL_API_TOKEN")
        && !token.is_empty()
        && let Ok(value) = format!("Bearer {}", token).parse()
    {
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP client")
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

    // ── Send deploy request ────────────────────────────────────────────────
    let client = http_client();
    let mut response = client
        .post(format!("{control_plane}/deploy"))
        .timeout(Duration::from_secs(300))
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
        .await?
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
    println!("  \x1b[2m{label:>10}\x1b[0m  \x1b[2m· {desc}\x1b[0m");
}

fn ms(v: u128) -> String {
    if v >= 1000 {
        format!("{:.1}s", v as f64 / 1000.0)
    } else {
        format!("{v}ms")
    }
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

    step("vm-id", &r.vm_id, "");
    step("status", &r.status, "");

    // ── Traefik route (primary ingress) ──────────────────────────────────
    if let Some(route_host) = &r.route_host {
        println!(
            "  \x1b[2m{:>10}\x1b[0m  \x1b[1mhttp://{}\x1b[0m  \x1b[2m(Traefik Host rule)\x1b[0m",
            "route", route_host
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
            &format!("\x1b[2m{ip}\x1b[0m"),
            &format!("  \x1b[2m(direct: curl {ip}:{gp})\x1b[0m"),
        );
    }
    if let Some(store) = &r.store_path {
        step("store", &format!("\x1b[2m{store}\x1b[0m"), "");
    }
    if let Some(flake) = &r.microvm_config_path {
        step("deploy.nix", &format!("\x1b[2m{flake}\x1b[0m"), "");
    }

    // ── Timing breakdown ───────────────────────────────────────────────────
    if let Some(t) = &r.timing {
        println!();
        println!("  \x1b[1;2mPhase timing & Docker Comparison:\x1b[0m");
        timing_row("resolve", t.resolve_ms, "repo + Russelfile");

        let docker_build_note = if t.build_ms < 3000 {
            " (Nix cache hit: fast incremental build - Docker equivalent takes 10s-30s)"
        } else {
            " (Nix package build - Docker equivalent takes 20s-60s)"
        };
        timing_row(
            "build",
            t.build_ms,
            &format!("nix build (package){}", docker_build_note),
        );

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

    println!();
    println!("  \x1b[2mnote: {}\x1b[0m", r.message);

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
    let path = PathBuf::from(repo);
    if path.exists() {
        return Ok(path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize local repo path {repo}"))?
            .display()
            .to_string());
    }
    Ok(repo.to_string())
}

/// Parse a single `KEY=VALUE` string into a (key, value) pair.
fn parse_env_kv(raw: &str) -> Result<(String, String)> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("env must be KEY=VALUE, got: {raw}"))?;
    if key.is_empty() {
        anyhow::bail!("env key must not be empty in: {raw}");
    }
    Ok((key.to_string(), value.to_string()))
}

/// Parse a `--env-file` path: each non-empty line is KEY=VALUE, `#` starts a comment.
fn parse_env_file(path: &str) -> Result<HashMap<String, String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read env file: {path}"))?;
    let mut map = HashMap::new();
    for (i, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, value) = parse_env_kv(trimmed)
            .with_context(|| format!("{}:{}: invalid env line", path, i + 1))?;
        map.insert(key, value);
    }
    Ok(map)
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
    let r = http_client()
        .get(&url)
        .send()
        .await?
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
    let r = http_client()
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<LogsResponse>()
        .await?;
    print!("{}", r.output);
    Ok(())
}

pub async fn vms(control_plane: &str) -> Result<()> {
    let r = http_client()
        .get(format!("{control_plane}/vms"))
        .send()
        .await?
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
    let r = http_client()
        .post(format!("{control_plane}/vm/{id}/stop"))
        .send()
        .await?
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
        body.insert("repo_url".into(), serde_json::json!(repo_url));
    }
    if let Some(config) = args.config {
        body.insert("config_path".into(), serde_json::json!(config));
    }

    let client = http_client();
    let mut response = client
        .post(format!("{control_plane}/vm/{}/update", args.id))
        .timeout(Duration::from_secs(300))
        .json(&body)
        .send()
        .await?
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
            let event: DeployEvent = serde_json::from_str(line)
                .with_context(|| format!("failed to parse event from control plane: {line}"))?;
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
                format!("failed to parse final event from control plane: {line}")
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
    let resp = http_client()
        .delete(format!("{control_plane}/vm/{id}"))
        .send()
        .await?;
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
            let resp = http_client()
                .post(format!("{control_plane}/secrets/{name}"))
                .json(&serde_json::json!({ "value": value }))
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("secrets set failed ({status}): {body}");
            }
            println!("secret {name} stored");
        }
        SecretsCommand::List => {
            let resp = http_client()
                .get(format!("{control_plane}/secrets"))
                .send()
                .await?
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
            let resp = http_client()
                .delete(format!("{control_plane}/secrets/{name}"))
                .send()
                .await?;
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
    fn normalize_repo_arg_nonexistent_returns_asis() {
        let name = "some-nonexistent-repo-name";
        assert_eq!(normalize_repo_arg(name).unwrap(), name);
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
        assert!(parse_env_kv("=value").is_err());
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
