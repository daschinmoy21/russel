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
        Command::Status => commands::status(&cli.control_plane).await?,
        Command::Logs => commands::logs(&cli.control_plane).await?,
    }

    Ok(())
}
