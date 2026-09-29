mod commands;
mod config;
mod init;
mod ui;

use anyhow::Result;
use clap::Parser;

use crate::commands::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Latch --insecure for cleartext Bearer policy (also RUSSEL_INSECURE_CLEARTEXT).
    commands::set_cli_insecure(cli.insecure);
    let resolved = config::resolve(cli.control_plane.as_deref())?;

    match cli.command {
        Command::Init(args) => init::run(args)?,
        Command::Login(args) => commands::login(args, &resolved).await?,
        Command::Logout => commands::logout()?,
        Command::Origin => commands::origin(&resolved).await?,
        Command::Apply(args) => commands::deploy(args, &resolved.control_plane).await?,
        Command::Status(args) => commands::status(args, &resolved.control_plane).await?,
        Command::Logs(args) => commands::logs(args, &resolved.control_plane).await?,
        Command::Ps => commands::ps(&resolved.control_plane).await?,
        Command::Stop(args) => commands::stop_vm(&args.id, &resolved.control_plane).await?,
        Command::Destroy(args) => commands::destroy_vm(&args, &resolved.control_plane).await?,
        Command::Update(args) => commands::update(args, &resolved.control_plane).await?,
        Command::Rollback(args) => commands::rollback(args, &resolved.control_plane).await?,
        Command::Secrets { action } => commands::secrets(action, &resolved.control_plane).await?,
    }

    Ok(())
}
