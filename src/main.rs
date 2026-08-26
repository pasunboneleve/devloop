mod browser_reload;
mod config;
mod core;
mod engine;
mod env_expand;
mod external_events;
mod output;
mod processes;
mod session_log;
mod state;
#[cfg(test)]
mod test_support;

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use pulldown_cmark::{
    CodeBlockKind, Event as MarkdownEvent, HeadingLevel, Parser as MarkdownParser, Tag, TagEnd,
};
use tracing::{Event, Subscriber, error};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;
use crate::engine::Engine;
use crate::output::{format_output_prefix, normalize_internal_log_label, should_colorize_output};
use crate::session_log::SessionLog;

const SESSION_LOG_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Print built-in reference documentation.
    Docs {
        #[arg(value_enum)]
        topic: DocsTopic,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DocsTopic {
    Config,
    Behavior,
    Development,
    Security,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { config } => {
            init_logging(None);
            let config = resolve_config_path(config)?;
            let config = Config::load(&config)?;
            config.validate()?;
        }
        Command::Run { config } => {
            let guardian_executable = devloop::process_guardian::GuardianExecutable::open()?;
            let config = resolve_config_path(config)?;
            let config = Config::load(&config)?;
            config.validate()?;
            let state_file = config
                .state_file
                .as_deref()
                .ok_or_else(|| anyhow!("state file missing after config load"))?;
            let session_log = SessionLog::create(state_file)?;
            init_logging(Some(session_log.clone()));
            announce_session_log_path(&session_log).await?;
            if let Err(error) = Engine::new(config, session_log.clone(), guardian_executable)
                .run()
                .await
            {
                error!(error = %format!("{error:#}"), "devloop run failed");
                flush_session_log_before_exit(&session_log).await;
                return Err(error);
            }
            flush_session_log_before_exit(&session_log).await;
        }
        Command::Docs { topic } => {
            print!("{}", render_docs_text(topic));
        }
    }
    Ok(())
}

async fn flush_session_log_before_exit(session_log: &SessionLog) {
    match tokio::time::timeout(
        SESSION_LOG_SHUTDOWN_FLUSH_TIMEOUT,
        session_log.flush_queued(),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("devloop: failed to flush session log before exit: {error}"),
        Err(_) => eprintln!("devloop: timed out flushing session log before exit"),
    }
}

async fn announce_session_log_path(session_log: &SessionLog) -> Result<()> {
    let message = format!("writing session log: {}", session_log.path().display());
    session_log
        .write_labeled_line("devloop", message.as_bytes())
        .with_context(|| "failed to persist session log path before runtime start")?;
    session_log
        .flush_queued()
        .await
        .with_context(|| "failed to flush session log path before runtime start")?;
    let _ = writeln!(io::stderr(), "devloop: {message}");
    Ok(())
}

fn init_logging(session_log: Option<SessionLog>) {
    match session_log {
        Some(session_log) => tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(default_rust_log()))
            .event_format(DevloopLogFormatter::default())
            .with_writer(SessionLogMakeWriter { session_log })
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(default_rust_log()))
            .event_format(DevloopLogFormatter::default())
            .with_writer(std::io::stderr)
            .init(),
    }
}

#[derive(Clone)]
struct SessionLogMakeWriter {
    session_log: SessionLog,
}

impl<'a> MakeWriter<'a> for SessionLogMakeWriter {
    type Writer = SessionLogTeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SessionLogTeeWriter {
            session_log: self.session_log.clone(),
            buffer: Vec::new(),
        }
    }
}

struct SessionLogTeeWriter {
    session_log: SessionLog,
    buffer: Vec<u8>,
}

impl Write for SessionLogTeeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer_to_log_and_terminal()
    }
}

impl SessionLogTeeWriter {
    fn flush_buffer_to_log_and_terminal(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return io::stderr().flush();
        }
        let log_bytes = strip_ansi_csi_sequences(&self.buffer);
        let log_result = self.session_log.queue_raw(log_bytes);
        let terminal_result = io::stderr().write_all(&self.buffer);
        if let Err(error) = log_result {
            eprintln!("devloop: failed to persist tracing output: {error}");
        }
        terminal_result?;
        self.buffer.clear();
        io::stderr().flush()
    }
}

impl Drop for SessionLogTeeWriter {
    fn drop(&mut self) {
        if let Err(error) = self.flush_buffer_to_log_and_terminal() {
            eprintln!("devloop: failed to flush tracing output: {error}");
        }
    }
}

#[cfg(test)]
impl SessionLogTeeWriter {
    fn write_for_test(session_log: SessionLog, chunks: &[&[u8]]) -> io::Result<()> {
        let mut writer = Self {
            session_log,
            buffer: Vec::new(),
        };
        for chunk in chunks {
            writer.write_all(chunk)?;
        }
        writer.flush()
    }
}

fn strip_ansi_csi_sequences(bytes: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            continue;
        }
        stripped.push(bytes[index]);
        index += 1;
    }
    stripped
}

fn default_rust_log() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())
}

fn format_tracing_prefix(target: &str, colorize: bool) -> String {
    let label = normalize_internal_log_label(target);
    format_output_prefix(&label, colorize)
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
        writer.write_str(&format_tracing_prefix(
            metadata.target(),
            should_colorize_output(),
        ))?;
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

fn docs_text(topic: DocsTopic) -> &'static str {
    match topic {
        DocsTopic::Config => include_str!("../docs/configuration.md"),
        DocsTopic::Behavior => include_str!("../docs/behavior.md"),
        DocsTopic::Development => include_str!("../docs/development.md"),
        DocsTopic::Security => include_str!("../docs/security.md"),
    }
}

fn render_docs_text(topic: DocsTopic) -> String {
    render_markdown_for_terminal(docs_text(topic))
}

fn render_markdown_for_terminal(markdown: &str) -> String {
    #[derive(Default)]
    struct RenderState {
        output: String,
        heading_level: Option<HeadingLevel>,
        in_code_block: bool,
        in_link: bool,
        pending_link_destination: Option<String>,
        list_depth: usize,
        in_item: bool,
        needs_blank_line: bool,
        at_line_start: bool,
    }

    impl RenderState {
        fn ensure_blank_line(&mut self) {
            if self.output.is_empty() {
                return;
            }
            if !self.output.ends_with("\n\n") {
                if !self.output.ends_with('\n') {
                    self.output.push('\n');
                }
                self.output.push('\n');
            }
            self.at_line_start = true;
        }

        fn ensure_line_start(&mut self) {
            if !self.at_line_start {
                self.output.push('\n');
                self.at_line_start = true;
            }
        }

        fn push_text(&mut self, text: &str) {
            if text.is_empty() {
                return;
            }
            if self.at_line_start && self.in_item {
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                self.output.push_str(&indent);
                self.output.push_str("- ");
                self.at_line_start = false;
            }
            self.output.push_str(text);
            self.at_line_start = false;
        }
    }

    let mut state = RenderState::default();

    for event in MarkdownParser::new(markdown) {
        match event {
            MarkdownEvent::Start(Tag::Heading { level, .. }) => {
                state.ensure_blank_line();
                state.heading_level = Some(level);
            }
            MarkdownEvent::End(TagEnd::Heading(_)) => {
                state.output.push('\n');
                state.output.push('\n');
                state.at_line_start = true;
                state.heading_level = None;
            }
            MarkdownEvent::Start(Tag::Paragraph) => {
                if state.needs_blank_line {
                    state.ensure_blank_line();
                    state.needs_blank_line = false;
                }
            }
            MarkdownEvent::End(TagEnd::Paragraph) => {
                state.output.push('\n');
                state.output.push('\n');
                state.at_line_start = true;
            }
            MarkdownEvent::Start(Tag::List(_)) => {
                state.ensure_blank_line();
                state.list_depth += 1;
            }
            MarkdownEvent::End(TagEnd::List(_)) => {
                state.list_depth = state.list_depth.saturating_sub(1);
                state.output.push('\n');
                state.at_line_start = true;
            }
            MarkdownEvent::Start(Tag::Item) => {
                state.in_item = true;
            }
            MarkdownEvent::End(TagEnd::Item) => {
                state.output.push('\n');
                state.at_line_start = true;
                state.in_item = false;
            }
            MarkdownEvent::Start(Tag::CodeBlock(kind)) => {
                state.ensure_blank_line();
                if let CodeBlockKind::Fenced(info) = kind
                    && !info.is_empty()
                {
                    state.push_text(&format!("[{info}]"));
                    state.output.push('\n');
                    state.at_line_start = true;
                }
                state.in_code_block = true;
            }
            MarkdownEvent::End(TagEnd::CodeBlock) => {
                state.output.push('\n');
                state.at_line_start = true;
                state.in_code_block = false;
                state.needs_blank_line = true;
            }
            MarkdownEvent::Start(Tag::Link { dest_url, .. }) => {
                state.in_link = true;
                state.pending_link_destination = Some(dest_url.to_string());
            }
            MarkdownEvent::End(TagEnd::Link) => {
                if let Some(dest) = state.pending_link_destination.take() {
                    state.push_text(&format!(" ({dest})"));
                }
                state.in_link = false;
            }
            MarkdownEvent::Text(text) => {
                let rendered = if state.in_code_block {
                    text.lines()
                        .map(|line| format!("    {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else if let Some(level) = state.heading_level {
                    format_heading(&text, level)
                } else {
                    text.to_string()
                };
                state.push_text(&rendered);
            }
            MarkdownEvent::Code(text) => {
                state.push_text(&format!("`{text}`"));
            }
            MarkdownEvent::SoftBreak => {
                if state.in_code_block {
                    state.output.push('\n');
                    state.at_line_start = true;
                } else {
                    state.push_text(" ");
                }
            }
            MarkdownEvent::HardBreak => {
                state.ensure_line_start();
            }
            MarkdownEvent::Rule => {
                state.ensure_blank_line();
                state.push_text("----------------------------------------");
                state.output.push('\n');
                state.output.push('\n');
                state.at_line_start = true;
            }
            MarkdownEvent::Html(_)
            | MarkdownEvent::InlineHtml(_)
            | MarkdownEvent::InlineMath(_)
            | MarkdownEvent::DisplayMath(_)
            | MarkdownEvent::FootnoteReference(_)
            | MarkdownEvent::TaskListMarker(_)
            | MarkdownEvent::Start(_)
            | MarkdownEvent::End(_) => {}
        }
    }

    state.output.trim_end().to_owned() + "\n"
}

fn format_heading(text: &str, level: HeadingLevel) -> String {
    match level {
        HeadingLevel::H1 => text.to_uppercase(),
        HeadingLevel::H2 => format!("{text}\n{}", "=".repeat(text.len())),
        HeadingLevel::H3 => format!("{text}\n{}", "-".repeat(text.len())),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, DocsTopic, SessionLogTeeWriter, announce_session_log_path, default_rust_log,
        docs_text, format_tracing_prefix, render_docs_text, render_markdown_for_terminal,
        strip_ansi_csi_sequences,
    };
    use crate::output::{
        format_output_prefix, normalize_internal_log_label, normalize_source_label,
    };
    use crate::session_log::SessionLog;
    use crate::test_support::RustLogGuard;
    use clap::Parser;
    use tempfile::tempdir;

    #[test]
    fn default_rust_log_uses_info_when_unset() {
        let _guard = RustLogGuard::set(None);
        assert_eq!(default_rust_log(), "info");
    }

    #[test]
    fn default_rust_log_respects_environment_override() {
        let _guard = RustLogGuard::set(Some("debug,devloop=trace"));
        assert_eq!(default_rust_log(), "debug,devloop=trace");
    }

    #[test]
    fn tracing_prefix_reuses_shared_output_label_format() {
        assert_eq!(
            format_output_prefix(&normalize_source_label("devloop::processes"), false),
            "[devloop processes] "
        );
    }

    #[tokio::test]
    async fn session_log_tee_writer_appends_one_buffered_record() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");

        SessionLogTeeWriter::write_for_test(
            log.clone(),
            &[b"first ".as_slice(), b"second\n".as_slice()],
        )
        .expect("write tracing event");
        log.flush_queued()
            .await
            .expect("flush queued tracing event");

        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read log"),
            "first second\n"
        );
    }

    #[tokio::test]
    async fn announce_session_log_path_reports_persistence_failure() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");
        log.fail_for_test(
            std::io::ErrorKind::BrokenPipe,
            "simulated session log failure",
        );

        let error = announce_session_log_path(&log)
            .await
            .expect_err("announcement should report log failure");

        assert!(format!("{error:#}").contains("failed to persist session log path"));
    }

    #[tokio::test]
    async fn session_log_tee_writer_strips_ansi_sequences_from_file_copy() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");

        SessionLogTeeWriter::write_for_test(
            log.clone(),
            &[b"\x1b[31mred".as_slice(), b"\x1b[0m plain\n".as_slice()],
        )
        .expect("write tracing event");
        log.flush_queued()
            .await
            .expect("flush queued tracing event");

        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read log"),
            "red plain\n"
        );
    }

    #[test]
    fn strip_ansi_csi_sequences_removes_color_codes() {
        assert_eq!(strip_ansi_csi_sequences(b"a\x1b[2;31mb\x1b[0mc"), b"abc");
    }

    #[test]
    fn tracing_prefix_wraps_dependency_targets_under_devloop() {
        assert_eq!(
            format_output_prefix(
                &normalize_internal_log_label("hyper_util::client::legacy::connect::http"),
                false
            ),
            "[devloop hyper_util client legacy connect http] "
        );
    }

    #[test]
    fn tracing_prefix_colorizes_internal_targets() {
        let rendered = format_tracing_prefix("devloop::engine", true);

        assert!(rendered.contains("[devloop engine]"));
        assert!(rendered.starts_with("\u{1b}[1;"));
        assert!(rendered.ends_with(" "));
    }

    #[test]
    fn tracing_prefix_colorizes_dependency_targets_under_devloop() {
        let rendered = format_tracing_prefix("hyper_util::client::legacy::connect::http", true);

        assert!(rendered.contains("[devloop hyper_util client legacy connect http]"));
        assert!(rendered.starts_with("\u{1b}[1;"));
        assert!(rendered.ends_with(" "));
    }

    #[test]
    fn docs_text_uses_embedded_configuration_reference() {
        let rendered = docs_text(DocsTopic::Config);

        assert!(rendered.starts_with("# Configuration Reference"));
        assert!(rendered.contains("startup_workflows"));
        assert!(rendered.contains("## State and session logs"));
        assert!(rendered.contains(".devloop/logs/"));
    }

    #[test]
    fn docs_text_uses_embedded_session_log_behavior_reference() {
        let rendered = docs_text(DocsTopic::Behavior);

        assert!(rendered.starts_with("# Behavior Reference"));
        assert!(rendered.contains("### Session logs"));
        assert!(rendered.contains("output.inherit"));
    }

    #[test]
    fn docs_text_uses_embedded_development_reference() {
        let rendered = docs_text(DocsTopic::Development);

        assert!(rendered.starts_with("# Development Guide"));
        assert!(rendered.contains("DEVLOOP_RUN_WATCH_FLAKE_SMOKE"));
    }

    #[test]
    fn rendered_docs_drop_markdown_heading_markers() {
        let rendered = render_docs_text(DocsTopic::Config);

        assert!(rendered.starts_with("CONFIGURATION REFERENCE"));
        assert!(!rendered.contains("# Configuration Reference"));
    }

    #[test]
    fn markdown_renderer_formats_lists_and_code_blocks() {
        let rendered =
            render_markdown_for_terminal("# Title\n\n- one\n- two\n\n```bash\ncargo test\n```\n");

        assert!(rendered.contains("TITLE"));
        assert!(rendered.contains("- one"));
        assert!(rendered.contains("[bash]"));
        assert!(rendered.contains("    cargo test"));
    }

    #[test]
    fn markdown_renderer_formats_links_inline_code_rules_and_nested_lists() {
        let rendered = render_markdown_for_terminal(
            "## Section\n\nParagraph with [link](https://example.com) and `inline`.\n\n- parent\n  - child\n\n---\n",
        );

        assert!(rendered.contains("Section\n======="));
        assert!(rendered.contains("link (https://example.com)"));
        assert!(rendered.contains("`inline`"));
        assert!(rendered.contains("- parent"));
        assert!(rendered.contains("  - child"));
        assert!(rendered.contains("----------------------------------------"));
    }

    #[test]
    fn cli_parses_docs_subcommand() {
        let cli = Cli::try_parse_from(["devloop", "docs", "security"]).expect("parse cli");

        match cli.command {
            super::Command::Docs { topic } => assert!(matches!(topic, DocsTopic::Security)),
            _ => panic!("expected docs subcommand"),
        }
    }

    #[test]
    fn cli_parses_development_docs_subcommand() {
        let cli = Cli::try_parse_from(["devloop", "docs", "development"]).expect("parse cli");

        match cli.command {
            super::Command::Docs { topic } => assert!(matches!(topic, DocsTopic::Development)),
            _ => panic!("expected docs subcommand"),
        }
    }
}
