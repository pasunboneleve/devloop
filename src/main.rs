mod config;
mod engine;
mod processes;
mod state;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::engine::Engine;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Run config-driven local development workflows",
    long_about = "devloop watches a client repository, supervises its processes, and executes ordered workflows defined in a TOML config file."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a devloop config file without starting any processes.
    Validate {
        /// Path to the devloop TOML config file. Defaults to ./devloop.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the configured watch, process, and workflow loop.
    Run {
        /// Path to the devloop TOML config file. Defaults to ./devloop.toml.
        #[arg(long)]
        config: Option<PathBuf>,
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
            let config = resolve_config_path(config)?;
            let config = Config::load(&config)?;
            config.validate()?;
        }
        Command::Run { config } => {
            let config = resolve_config_path(config)?;
            let config = Config::load(&config)?;
            config.validate()?;
            Engine::new(config).run().await?;
        }
    }
    Ok(())
}

fn resolve_config_path(config: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(config) = config {
        return Ok(config);
    }

    let default = std::env::current_dir()?.join("devloop.toml");
    if default.exists() {
        Ok(default)
    } else {
        Err(anyhow!(
            "no devloop config provided and no ./devloop.toml found in {}",
            std::env::current_dir()?.display()
        ))
    }
}
