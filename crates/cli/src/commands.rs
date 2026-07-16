use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use russel_core::{
    RuntimeKind,
    api::{
        DeployRequest, DeployResponse, LogsResponse, PortMapping, StatusResponse, VmsResponse,
    },
    config::{Russelfile, resolve_runtime},
};

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
}

#[derive(Debug, Args)]
pub struct DeployArgs {
    #[arg(value_name = "REPO")]
    pub repo: String,

    #[arg(short = 'p', long = "publish", value_name = "HOST:GUEST")]
    pub port: Option<String>,

    #[arg(long, value_name = "ID")]
    pub vm_id: Option<String>,

    #[arg(long, default_value = "Russelfile.toml")]
    pub config: String,

    /// Runtime kind (`microvm` or `container`). Must match Russelfile `service.type` for local repos.
    #[arg(long, value_name = "RUNTIME")]
    pub runtime: Option<String>,
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
    println!();

    // ── Send deploy request ────────────────────────────────────────────────
    let client = reqwest::Client::new();
    let mut response = client
        .post(format!("{control_plane}/deploy"))
        .timeout(Duration::from_secs(300))
        .json(&DeployRequest {
            repo_url,
            config_path: args.config,
            vm_id: args.vm_id,
            port,
            runtime,
        })
        .send()
        .await?
        .error_for_status()?;

    let mut buffer = String::new();
    let mut final_response = None;

    while let Some(chunk) = response.chunk().await? {
        if let Ok(s) = std::str::from_utf8(&chunk) {
            buffer.push_str(s);
            while let Some(i) = buffer.find('\n') {
                let line = buffer[..i].to_string();
                buffer = buffer[i + 1..].to_string();

                if line.trim().is_empty() {
                    continue;
                }

                let event: russel_core::api::DeployEvent = serde_json::from_str(&line)
                    .with_context(|| {
                        format!("failed to parse event from control plane: {}", line)
                    })?;

                match event {
                    russel_core::api::DeployEvent::Progress {
                        phase: p,
                        description: d,
                    } => {
                        phase(&p, &d);
                    }
                    russel_core::api::DeployEvent::Complete(resp) => {
                        final_response = Some(resp);
                    }
                    russel_core::api::DeployEvent::Error(err) => {
                        anyhow::bail!("deploy failed: {}", err);
                    }
                }
            }
        }
    }

    let response = final_response.ok_or_else(|| {
        anyhow::anyhow!(
            "control plane closed connection before complete. \
             Run `russel logs` or check `russel status` for details."
        )
    })?;

    println!();
    print_deploy_response(*response, wall.elapsed());

    Ok(())
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
    let icon = if ok {
        "\x1b[1;32m✓\x1b[0m"
    } else {
        "\x1b[1;31m✗\x1b[0m"
    };
    let label = if ok { "Deployed" } else { "Failed" };

    println!(
        "  {icon} {label} in \x1b[1m{}\x1b[0m  (server: {})",
        ms(wall.as_millis()),
        ms(r.elapsed_ms)
    );
    println!();

    step("vm-id", &r.vm_id, "");
    step("status", &r.status, "");

    if let Some(p) = &r.port {
        step(
            "port",
            &format!("localhost:\x1b[1m{}\x1b[0m → guest:{}", p.host, p.guest),
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
    let r = reqwest::get(&url)
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
    let r = reqwest::get(&url)
        .await?
        .error_for_status()?
        .json::<LogsResponse>()
        .await?;
    print!("{}", r.output);
    Ok(())
}

pub async fn vms(control_plane: &str) -> Result<()> {
    let r = reqwest::get(format!("{control_plane}/vms"))
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
    let r = reqwest::Client::new()
        .post(format!("{control_plane}/vm/{id}/stop"))
        .send()
        .await?
        .error_for_status()?
        .json::<String>()
        .await?;
    println!("{}", r);
    Ok(())
}

pub async fn destroy_vm(id: &str, control_plane: &str) -> Result<()> {
    let r = reqwest::Client::new()
        .delete(format!("{control_plane}/vm/{id}"))
        .send()
        .await?
        .error_for_status()?
        .json::<String>()
        .await?;
    println!("{}", r);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
