mod commands;
mod init;

use anyhow::Result;
use clap::Parser;

use crate::commands::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Latch --insecure for cleartext Bearer policy (also RUSSEL_INSECURE_CLEARTEXT).
    commands::set_cli_insecure(cli.insecure);

    match cli.command {
        Command::Init(args) => init::run(args)?,
        Command::Deploy(args) => commands::deploy(args, &cli.control_plane).await?,
        Command::Status(args) => commands::status(args, &cli.control_plane).await?,
        Command::Logs(args) => commands::logs(args, &cli.control_plane).await?,
        Command::Vms => commands::vms(&cli.control_plane).await?,
        Command::Stop(args) => commands::stop_vm(&args.id, &cli.control_plane).await?,
        Command::Destroy(args) => commands::destroy_vm(&args.id, &cli.control_plane).await?,
        Command::Update(args) => commands::update(args, &cli.control_plane).await?,
        Command::Secrets { action } => commands::secrets(action, &cli.control_plane).await?,
    }

    Ok(())
}
