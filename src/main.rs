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
        .with_env_filter(EnvFilter::new(default_rust_log()))
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

fn default_rust_log() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())
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

#[cfg(test)]
mod tests {
    use super::default_rust_log;

    #[test]
    fn default_rust_log_uses_info_when_unset() {
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
        assert_eq!(default_rust_log(), "info");
    }

    #[test]
    fn default_rust_log_respects_environment_override() {
        unsafe {
            std::env::set_var("RUST_LOG", "debug,devloop=trace");
        }
        assert_eq!(default_rust_log(), "debug,devloop=trace");
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
    }
}
