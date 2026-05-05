use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use russel_core::api::{DeployRequest, DeployResponse, LogsResponse, PortMapping, StatusResponse};

#[derive(Debug, Parser)]
#[command(name = "russel", about = "Deploy Nix-built services into microVMs")]
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
    Status,
    Logs,
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
}

pub async fn deploy(args: DeployArgs, control_plane: &str) -> Result<()> {
    let started = Instant::now();
    let repo_url = normalize_repo_arg(&args.repo)?;
    let port = args.port.as_deref().map(parse_port_mapping).transpose()?;

    println!("-> Initializing microVM runtime...");
    println!("-> Resolving deployment source...");
    if let Some(vm_id) = &args.vm_id {
        println!("-> Reserving VM id `{vm_id}`...");
    }
    if let Some(port) = &port {
        println!(
            "-> Publishing localhost:{} -> guest:{}...",
            port.host, port.guest
        );
    }
    println!("-> Loading Nix foundation layer...");
    println!("-> Mapping code payload...");

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{control_plane}/deploy"))
        .json(&DeployRequest {
            repo_url,
            config_path: args.config,
            vm_id: args.vm_id,
            port,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<DeployResponse>()
        .await?;

    print_deploy_response(response, started.elapsed());

    Ok(())
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
        .ok_or_else(|| anyhow!("port mapping must be HOST:GUEST, for example 3000:3000"))?;

    Ok(PortMapping {
        host: host
            .parse()
            .with_context(|| format!("invalid host port in {value}"))?,
        guest: guest
            .parse()
            .with_context(|| format!("invalid guest port in {value}"))?,
    })
}

fn print_deploy_response(response: DeployResponse, client_elapsed: Duration) {
    if response.status == "deployed" {
        println!("✓ Deployed in {}ms", response.elapsed_ms);
    } else {
        println!("✗ Deploy failed in {}ms", response.elapsed_ms);
    }

    println!("  vm_id: {}", response.vm_id);
    println!("  service: {}", response.service_id);
    println!("  status: {}", response.status);

    if let Some(port) = &response.port {
        println!("  port: localhost:{} -> guest:{}", port.host, port.guest);
    }
    if let Some(store_path) = response.store_path {
        println!("  store: {store_path}");
    }
    if let Some(path) = response.microvm_config_path {
        println!("  flake: {path}");
    }
    if let Some(path) = response.runner_path {
        println!("  runner: {path}");
    }

    println!("  note: {}", response.message);
    if response.status == "deployed" {
        if let Some(port) = &response.port {
            println!("  test: curl -I http://127.0.0.1:{}/", port.host);
        }
    }
    println!("  roundtrip: {}ms", client_elapsed.as_millis());
}

pub async fn status(control_plane: &str) -> Result<()> {
    let response = reqwest::get(format!("{control_plane}/status"))
        .await?
        .error_for_status()?
        .json::<StatusResponse>()
        .await?;

    println!("service_id={}", response.service_id);
    println!("status={}", response.status);
    println!("vm_state={}", response.vm_state);
    println!("uptime_seconds={}", response.uptime_seconds);

    Ok(())
}

pub async fn logs(control_plane: &str) -> Result<()> {
    let response = reqwest::get(format!("{control_plane}/logs"))
        .await?
        .error_for_status()?
        .json::<LogsResponse>()
        .await?;

    print!("{}", response.output);

    Ok(())
}
