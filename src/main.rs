mod config;
mod engine;
mod output;
mod processes;
mod state;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;
use crate::engine::Engine;
use crate::output::{format_output_prefix, normalize_source_label, should_colorize_output};

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
        .event_format(DevloopLogFormatter::default())
        .with_writer(std::io::stderr)
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

#[derive(Debug, Default)]
struct DevloopLogFormatter {
    timer: SystemTime,
}

impl<S, N> FormatEvent<S, N> for DevloopLogFormatter
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();
        let label = normalize_source_label(metadata.target());
        writer.write_str(&format_output_prefix(&label, should_colorize_output()))?;
        write!(writer, "{} ", metadata.level())?;
        self.timer.format_time(&mut writer)?;
        writer.write_char(' ')?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
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
    use crate::output::{format_output_prefix, normalize_source_label};
    use std::sync::{Mutex, OnceLock};

    fn rust_log_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn default_rust_log_uses_info_when_unset() {
        let _guard = rust_log_lock().lock().expect("lock RUST_LOG test mutex");
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
        assert_eq!(default_rust_log(), "info");
    }

    #[test]
    fn default_rust_log_respects_environment_override() {
        let _guard = rust_log_lock().lock().expect("lock RUST_LOG test mutex");
        unsafe {
            std::env::set_var("RUST_LOG", "debug,devloop=trace");
        }
        assert_eq!(default_rust_log(), "debug,devloop=trace");
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
    }

    #[test]
    fn tracing_prefix_reuses_shared_output_label_format() {
        assert_eq!(
            format_output_prefix(&normalize_source_label("devloop::processes"), false),
            "[devloop processes] "
        );
    }
}
