mod config;
mod engine;
mod processes;
mod state;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::engine::Engine;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Validate {
        #[arg(long)]
        config: PathBuf,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Validate { config } => {
            let config = Config::load(&config)?;
            config.validate()?;
        }
        Command::Run { config } => {
            let config = Config::load(&config)?;
            config.validate()?;
            Engine::new(config).run().await?;
        }
    }
    Ok(())
}
