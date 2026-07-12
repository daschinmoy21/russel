mod commands;

use anyhow::Result;
use clap::Parser;

use crate::commands::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Deploy(args) => commands::deploy(args, &cli.control_plane).await?,
        Command::Status(args) => commands::status(args, &cli.control_plane).await?,
        Command::Logs(args) => commands::logs(args, &cli.control_plane).await?,
        Command::Vms => commands::vms(&cli.control_plane).await?,
        Command::Stop(args) => commands::stop_vm(&args.id, &cli.control_plane).await?,
        Command::Destroy(args) => commands::destroy_vm(&args.id, &cli.control_plane).await?,
    }

    Ok(())
}
