use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt, Stderr, Stdout};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{
    Config, HookOutputConfig, HookSpec, OutputBodyStyle, OutputExtract, OutputRule, ProbeSpec,
    ProcessSpec,
};
use crate::core::{ProcessEffect, ProcessSupervisor};
use crate::external_events::{ExternalEventEnvironment, apply_external_event_env};
use crate::output::{
    dim_start, format_output_prefix_with_style, should_colorize_output, style_reset,
};
use crate::state::SessionState;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, ManagedProcess>,
    client: reqwest::Client,
    shutting_down: bool,
    stdout: Arc<Mutex<Stdout>>,
    stderr: Arc<Mutex<Stderr>>,
    supervisor: ProcessSupervisor,
    clock_start: Instant,
    external_event_env: Option<ExternalEventEnvironment>,
}

struct ManagedProcess {
    child: Child,
}

struct CommandContext<'a> {
    env: &'a BTreeMap<String, String>,
    external_event_env: Option<&'a ExternalEventEnvironment>,
    root: &'a Path,
    state_path: &'a Path,
    changed_files: &'a [String],
    workflow: &'a str,
}

impl<'a> ProcessManager<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            children: BTreeMap::new(),
            client: reqwest::Client::new(),
            shutting_down: false,
            stdout: Arc::new(Mutex::new(tokio::io::stdout())),
            stderr: Arc::new(Mutex::new(tokio::io::stderr())),
            supervisor: ProcessSupervisor::new(config),
            clock_start: Instant::now(),
            external_event_env: None,
        }
    }

    pub fn set_external_event_env(&mut self, external_event_env: Option<ExternalEventEnvironment>) {
        self.external_event_env = external_event_env;
    }

    pub async fn start_autostart(&mut self, state: &SessionState) -> Result<()> {
        for effect in self.supervisor.autostart_effects(self.config) {
            self.apply_process_effect(effect, state).await?;
        }
        Ok(())
    }

    pub async fn start_named(&mut self, name: &str, state: &SessionState) -> Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let spec = self
            .config
            .process
            .get(name)
            .ok_or_else(|| anyhow!("unknown process '{name}'"))?;
        self.start(name, spec, state).await
    }

    pub async fn stop_named(&mut self, name: &str) -> Result<()> {
        let Some(mut child) = self.children.remove(name) else {
            return Ok(());
        };
        terminate_child(name, &mut child.child).await?;
        self.supervisor.on_process_stopped(name);
        Ok(())
    }

    pub async fn restart_named(&mut self, name: &str, state: &SessionState) -> Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        self.stop_named(name).await?;
        self.start_named(name, state).await
    }

    pub async fn wait_for_named(&self, name: &str, state: &SessionState) -> Result<()> {
        let spec = self
            .config
            .process
            .get(name)
            .ok_or_else(|| anyhow!("unknown process '{name}'"))?;
        if let Some(check) = &spec.readiness {
            wait_for_probe(&self.client, name, check, state).await?;
        }
        Ok(())
    }

    pub async fn run_hook(
        &self,
        name: &str,
        state: &SessionState,
        changed_files: &[String],
        workflow: &str,
    ) -> Result<()> {
        let spec = self
            .config
            .hook
            .get(name)
            .ok_or_else(|| anyhow!("unknown hook '{name}'"))?;
        let mut command = configure_command(
            &spec.command,
            resolve_cwd(&self.config.root, spec.cwd.as_deref()),
            CommandContext {
                env: &spec.env,
                external_event_env: self.external_event_env.as_ref(),
                root: &self.config.root,
                state_path: state.path(),
                changed_files,
                workflow,
            },
        )?;
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to run hook '{name}'"))?;
        let source_label = process_output_source_label(name, &spec.command);
        self.render_hook_output(&source_label, &spec.output, &output.stdout, &output.stderr)
            .await;
        if !output.status.success() {
            return Err(anyhow!(
                "hook '{name}' failed with status {}",
                output.status
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .with_context(|| format!("hook '{name}' produced non-utf8 stdout"))?;
        apply_hook_capture(spec, stdout.trim(), state)
    }

    pub async fn run_observed_hook(
        &self,
        name: &str,
        state: &SessionState,
        changed_files: &[String],
        workflow: &str,
    ) -> Result<bool> {
        let before = state.snapshot()?;
        self.run_hook(name, state, changed_files, workflow).await?;
        let after = state.snapshot()?;
        Ok(before != after)
    }

    pub async fn stop_all(&mut self, state: &SessionState) -> Result<()> {
        self.initiate_shutdown();
        for effect in self.supervisor.on_shutdown() {
            self.apply_process_effect(effect, state).await?;
        }
        Ok(())
    }

    pub async fn maintain(&mut self, state: &SessionState) -> Result<()> {
        let names: Vec<String> = self.children.keys().cloned().collect();
        let mut exits = Vec::new();
        for name in names {
            let exited = {
                let managed = self
                    .children
                    .get_mut(&name)
                    .ok_or_else(|| anyhow!("missing managed process '{name}'"))?;
                managed.child.try_wait()?
            };

            if let Some(status) = exited {
                warn!("process {} exited with {}", name, status);
                self.children.remove(&name);
                exits.push((name, status.success()));
            }
        }
        let now_ms = self.clock_start.elapsed().as_millis() as u64;
        for effect in self.supervisor.on_tick(self.config, now_ms, exits) {
            self.apply_process_effect(effect, state).await?;
        }
        Ok(())
    }

    async fn start(&mut self, name: &str, spec: &ProcessSpec, state: &SessionState) -> Result<()> {
        if self.children.contains_key(name) {
            return Ok(());
        }
        let mut command = configure_command(
            &spec.command,
            resolve_cwd(&self.config.root, spec.cwd.as_deref()),
            CommandContext {
                env: &spec.env,
                external_event_env: self.external_event_env.as_ref(),
                root: &self.config.root,
                state_path: state.path(),
                changed_files: &[],
                workflow: "startup",
            },
        )?;
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start process '{name}'"))?;
        clear_output_state_keys(&spec.output.rules, state)?;
        let process_name = name.to_owned();
        let source_label = process_output_source_label(name, &spec.command);
        let inherit_output = spec.output.inherit;
        let body_style = spec.output.body_style;
        let rules = compile_output_rules(&spec.output.rules)?;
        let stdout_sink = OutputSink::Stdout(self.stdout.clone());
        let stderr_sink = OutputSink::Stderr(self.stderr.clone());
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(forward_output_lines(
                stdout,
                ForwardOutputConfig {
                    output: stdout_sink,
                    source_label: source_label.clone(),
                    inherit_output,
                    body_style,
                },
                process_name.clone(),
                rules.clone(),
                state.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_output_lines(
                stderr,
                ForwardOutputConfig {
                    output: stderr_sink,
                    source_label,
                    inherit_output,
                    body_style,
                },
                process_name,
                rules,
                state.clone(),
            ));
        }
        self.children
            .insert(name.to_owned(), ManagedProcess { child });
        self.supervisor.on_process_started(name);
        info!("started process {}", name);
        Ok(())
    }

    pub fn initiate_shutdown(&mut self) {
        self.shutting_down = true;
    }

    async fn apply_process_effect(
        &mut self,
        effect: ProcessEffect,
        state: &SessionState,
    ) -> Result<()> {
        let mut pending = VecDeque::from([effect]);

        while let Some(effect) = pending.pop_front() {
            match effect {
                ProcessEffect::StartProcess { process } => {
                    self.start_named(&process, state).await?
                }
                ProcessEffect::RestartProcess { process } => {
                    self.restart_named(&process, state).await?
                }
                ProcessEffect::StopProcess { process } => self.stop_named(&process).await?,
                ProcessEffect::CheckLiveness { process } => {
                    let Some(spec) = self.config.process.get(&process) else {
                        continue;
                    };
                    let Some(liveness) = &spec.liveness else {
                        continue;
                    };
                    let now_ms = self.clock_start.elapsed().as_millis() as u64;
                    let healthy = match check_probe(&self.client, &process, liveness, state).await {
                        Ok(()) => true,
                        Err(error) => {
                            warn!("liveness probe failed for {}: {}", process, error);
                            false
                        }
                    };
                    for next in
                        self.supervisor
                            .on_liveness_result(self.config, &process, healthy, now_ms)
                    {
                        pending.push_back(next);
                    }
                }
            }
        }

        Ok(())
    }

    async fn render_hook_output(
        &self,
        source_label: &str,
        output: &HookOutputConfig,
        stdout: &[u8],
        stderr: &[u8],
    ) {
        if !output.inherit {
            return;
        }

        if let Err(error) =
            write_captured_output_to_writer(&self.stdout, source_label, stdout, output.body_style)
                .await
        {
            warn!(
                "failed to write hook stdout for {}: {}",
                source_label, error
            );
        }

        if let Err(error) =
            write_captured_output_to_writer(&self.stderr, source_label, stderr, output.body_style)
                .await
        {
            warn!(
                "failed to write hook stderr for {}: {}",
                source_label, error
            );
        }
    }
}

#[derive(Clone)]
struct CompiledOutputRule {
    regex: Option<Regex>,
    state_key: String,
    extract: OutputExtract,
    capture_group: usize,
}

struct ForwardOutputConfig {
    output: OutputSink,
    source_label: String,
    inherit_output: bool,
    body_style: OutputBodyStyle,
}

async fn forward_output_lines<T>(
    reader: T,
    config: ForwardOutputConfig,
    process_name: String,
    rules: Vec<CompiledOutputRule>,
    state: SessionState,
) where
    T: tokio::io::AsyncRead + Unpin,
{
    let colorize = should_colorize_output();
    let mut reader = reader;
    let mut chunk = [0_u8; 4096];
    let mut line_buffer = Vec::new();
    let mut render_state = OutputRenderState::default();
    let mut last_was_carriage_return = false;
    let mut output_failed = false;

    loop {
        let bytes_read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(error) => {
                warn!("failed to read output for {}: {}", process_name, error);
                break;
            }
        };

        for &byte in &chunk[..bytes_read] {
            if config.inherit_output
                && !output_failed
                && let Err(error) = write_output_byte(
                    &config.output,
                    &config.source_label,
                    byte,
                    colorize,
                    config.body_style,
                    &mut render_state,
                )
                .await
            {
                warn!("failed to write output for {}: {}", process_name, error);
                output_failed = true;
            }
            process_output_byte_for_rules(
                &process_name,
                byte,
                &mut line_buffer,
                &mut last_was_carriage_return,
                &rules,
                &state,
            );
        }
    }

    if !line_buffer.is_empty() {
        process_output_line(&process_name, &line_buffer, &rules, &state);
    }
    if config.inherit_output
        && !output_failed
        && let Err(error) = flush_rendered_output(&config.output, &mut render_state, false).await
    {
        warn!("failed to flush output for {}: {}", process_name, error);
    }
}

#[cfg(test)]
fn format_output_line(
    source_label: &str,
    line: &str,
    colorize: bool,
    body_style: OutputBodyStyle,
) -> String {
    use crate::output::style_output_text;

    let prefix = format_output_prefix_with_style(source_label, colorize, body_style);
    let body = style_output_text(line, body_style, colorize);
    format!("{prefix}{body}")
}

#[derive(Debug)]
struct OutputRenderState {
    at_line_start: bool,
    last_was_carriage_return: bool,
    ansi_escape_state: AnsiEscapeState,
    utf8_buffer: Vec<u8>,
    body_style: OutputBodyStyle,
    colorize: bool,
    dim_active: bool,
    rendered_line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiEscapeState {
    None,
    AfterEsc,
    InCsi,
}

impl OutputRenderState {
    fn new() -> Self {
        Self {
            at_line_start: true,
            last_was_carriage_return: false,
            ansi_escape_state: AnsiEscapeState::None,
            utf8_buffer: Vec::new(),
            body_style: OutputBodyStyle::Plain,
            colorize: false,
            dim_active: false,
            rendered_line: String::new(),
        }
    }
}

impl Default for OutputRenderState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
enum OutputSink {
    Stdout(Arc<Mutex<Stdout>>),
    Stderr(Arc<Mutex<Stderr>>),
}

async fn write_output_byte(
    output: &OutputSink,
    source_label: &str,
    byte: u8,
    colorize: bool,
    body_style: OutputBodyStyle,
    render_state: &mut OutputRenderState,
) -> std::io::Result<()> {
    match output {
        OutputSink::Stdout(writer) => {
            write_output_byte_to_writer(
                writer,
                source_label,
                byte,
                colorize,
                body_style,
                render_state,
            )
            .await
        }
        OutputSink::Stderr(writer) => {
            write_output_byte_to_writer(
                writer,
                source_label,
                byte,
                colorize,
                body_style,
                render_state,
            )
            .await
        }
    }
}

async fn flush_rendered_output(
    output: &OutputSink,
    render_state: &mut OutputRenderState,
    add_newline: bool,
) -> std::io::Result<()> {
    match output {
        OutputSink::Stdout(writer) => {
            flush_rendered_output_to_writer(writer, render_state, add_newline).await
        }
        OutputSink::Stderr(writer) => {
            flush_rendered_output_to_writer(writer, render_state, add_newline).await
        }
    }
}

async fn write_captured_output_to_writer<W>(
    writer: &Arc<Mutex<W>>,
    source_label: &str,
    bytes: &[u8],
    body_style: OutputBodyStyle,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + Send,
{
    let colorize = should_colorize_output();
    let mut render_state = OutputRenderState::default();

    for &byte in bytes {
        write_output_byte_to_writer(
            writer,
            source_label,
            byte,
            colorize,
            body_style,
            &mut render_state,
        )
        .await?;
    }

    flush_rendered_output_to_writer(writer, &mut render_state, false).await
}

async fn write_output_byte_to_writer<W>(
    writer: &Arc<Mutex<W>>,
    source_label: &str,
    byte: u8,
    colorize: bool,
    body_style: OutputBodyStyle,
    render_state: &mut OutputRenderState,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + Send,
{
    if byte == b'\r' {
        render_state.body_style = body_style;
        render_state.colorize = colorize;
        flush_rendered_output_to_writer(writer, render_state, true).await?;
        render_state.last_was_carriage_return = true;
        return Ok(());
    }

    if byte == b'\n' {
        render_state.body_style = body_style;
        render_state.colorize = colorize;
        if render_state.last_was_carriage_return {
            render_state.last_was_carriage_return = false;
            return Ok(());
        }
        flush_rendered_output_to_writer(writer, render_state, true).await?;
        return Ok(());
    }

    render_state.last_was_carriage_return = false;

    if render_state.at_line_start {
        let prefix = format_output_prefix_with_style(source_label, colorize, body_style);
        render_state.rendered_line.push_str(&prefix);
        if matches!(body_style, OutputBodyStyle::Dim) {
            render_state.rendered_line.push_str(dim_start(colorize));
            render_state.dim_active = colorize;
        }
        render_state.at_line_start = false;
    }

    let rendered = render_output_byte(byte, colorize, body_style, render_state);
    render_state.rendered_line.push_str(&rendered);
    Ok(())
}

async fn flush_rendered_output_to_writer<W>(
    writer: &Arc<Mutex<W>>,
    render_state: &mut OutputRenderState,
    add_newline: bool,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + Send,
{
    flush_pending_utf8(render_state);

    if render_state.rendered_line.is_empty() && !add_newline {
        return Ok(());
    }

    let mut writer = writer.lock().await;
    if !render_state.rendered_line.is_empty() {
        if render_state.dim_active {
            render_state
                .rendered_line
                .push_str(style_reset(render_state.colorize));
            render_state.dim_active = false;
        }
        writer
            .write_all(render_state.rendered_line.as_bytes())
            .await?;
        render_state.rendered_line.clear();
    }
    if add_newline {
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    render_state.at_line_start = true;
    Ok(())
}

fn render_output_byte(
    byte: u8,
    colorize: bool,
    body_style: OutputBodyStyle,
    render_state: &mut OutputRenderState,
) -> String {
    if byte == 0x1b {
        let mut text = take_utf8_buffer_lossy(render_state);
        render_state.ansi_escape_state = AnsiEscapeState::AfterEsc;
        text.push(byte as char);
        return text;
    }

    match render_state.ansi_escape_state {
        AnsiEscapeState::AfterEsc => {
            if byte == b'[' {
                render_state.ansi_escape_state = AnsiEscapeState::InCsi;
            } else {
                render_state.ansi_escape_state = AnsiEscapeState::None;
            }
            return (byte as char).to_string();
        }
        AnsiEscapeState::InCsi => {
            if matches!(byte, 0x40..=0x7e) {
                render_state.ansi_escape_state = AnsiEscapeState::None;
            }
            let mut text = (byte as char).to_string();
            if byte == b'm' && matches!(body_style, OutputBodyStyle::Dim) {
                text.push_str(dim_start(colorize));
            }
            return text;
        }
        AnsiEscapeState::None => {}
    }

    if byte.is_ascii_control() {
        let mut text = take_utf8_buffer_lossy(render_state);
        text.push(byte as char);
        return text;
    }

    let _ = colorize;
    let _ = body_style;

    render_state.utf8_buffer.push(byte);
    take_complete_utf8(render_state)
}

fn take_complete_utf8(render_state: &mut OutputRenderState) -> String {
    match std::str::from_utf8(&render_state.utf8_buffer) {
        Ok(text) => {
            let rendered = text.to_owned();
            render_state.utf8_buffer.clear();
            rendered
        }
        Err(error) if error.error_len().is_none() => String::new(),
        Err(_) => take_utf8_buffer_lossy(render_state),
    }
}

fn flush_pending_utf8(render_state: &mut OutputRenderState) {
    let pending = take_utf8_buffer_lossy(render_state);
    render_state.rendered_line.push_str(&pending);
}

fn take_utf8_buffer_lossy(render_state: &mut OutputRenderState) -> String {
    if render_state.utf8_buffer.is_empty() {
        return String::new();
    }

    let rendered = String::from_utf8_lossy(&render_state.utf8_buffer).into_owned();
    render_state.utf8_buffer.clear();
    rendered
}

fn process_output_byte_for_rules(
    process_name: &str,
    byte: u8,
    line_buffer: &mut Vec<u8>,
    last_was_carriage_return: &mut bool,
    rules: &[CompiledOutputRule],
    state: &SessionState,
) {
    if byte == b'\r' {
        process_output_line(process_name, line_buffer, rules, state);
        line_buffer.clear();
        *last_was_carriage_return = true;
        return;
    }

    if byte == b'\n' {
        if !*last_was_carriage_return {
            process_output_line(process_name, line_buffer, rules, state);
            line_buffer.clear();
        }
        *last_was_carriage_return = false;
        return;
    }

    *last_was_carriage_return = false;
    line_buffer.push(byte);
}

fn process_output_line(
    process_name: &str,
    bytes: &[u8],
    rules: &[CompiledOutputRule],
    state: &SessionState,
) {
    let line = String::from_utf8_lossy(bytes)
        .trim_end_matches(['\n', '\r'])
        .to_owned();

    for rule in rules {
        if let Some(value) = extract_output_value(rule, &line)
            && let Err(error) = state.set(&rule.state_key, value.into())
        {
            warn!(
                "failed to persist output state for {} key {}: {}",
                process_name, rule.state_key, error
            );
        }
    }
}

fn process_output_source_label(name: &str, command: &[String]) -> String {
    let executable = command
        .first()
        .map(|program| executable_display_name(program))
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{executable} {name}")
}

fn executable_display_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(program)
        .to_owned()
}

fn configure_command(
    command: &[String],
    cwd: PathBuf,
    context: CommandContext<'_>,
) -> Result<Command> {
    let Some(program) = command.first() else {
        return Err(anyhow!("command must not be empty"));
    };
    let program = resolve_program(context.root, program);
    let mut cmd = Command::new(program);
    cmd.args(&command[1..]);
    cmd.current_dir(cwd);
    let mut full_env = context.env.clone();
    apply_external_event_env(&mut full_env, context.external_event_env);
    cmd.envs(full_env);
    cmd.env("DEVLOOP_ROOT", context.root);
    cmd.env("DEVLOOP_STATE", context.state_path);
    cmd.env("DEVLOOP_WORKFLOW", context.workflow);
    cmd.env(
        "DEVLOOP_CHANGED_FILES_JSON",
        serde_json::to_string(context.changed_files)?,
    );
    Ok(cmd)
}

fn resolve_program(root: &Path, program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() == 1 {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn resolve_cwd(root: &Path, cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => root.join(path),
        None => root.to_path_buf(),
    }
}

fn apply_hook_capture(spec: &HookSpec, stdout: &str, state: &SessionState) -> Result<()> {
    match spec.capture {
        None | Some(crate::config::CaptureMode::Ignore) => Ok(()),
        Some(crate::config::CaptureMode::Text) => state.set(
            spec.state_key
                .as_ref()
                .expect("validated state_key for text capture"),
            stdout.to_owned().into(),
        ),
        Some(crate::config::CaptureMode::Json) => {
            let object = serde_json::from_str(stdout).context("hook stdout was not valid JSON")?;
            state.merge_json_object(object)
        }
    }
}

fn compile_output_rules(rules: &[OutputRule]) -> Result<Vec<CompiledOutputRule>> {
    rules
        .iter()
        .map(|rule| {
            Ok(CompiledOutputRule {
                regex: match &rule.pattern {
                    Some(pattern) => Some(Regex::new(pattern)?),
                    None => None,
                },
                state_key: rule.state_key.clone(),
                extract: rule.extract,
                capture_group: rule.capture_group,
            })
        })
        .collect()
}

fn extract_output_value(rule: &CompiledOutputRule, line: &str) -> Option<String> {
    match rule.extract {
        OutputExtract::Regex => {
            let regex = rule.regex.as_ref()?;
            let captures = regex.captures(line)?;
            captures
                .get(rule.capture_group)
                .map(|value| value.as_str().to_owned())
        }
        OutputExtract::UrlToken => line
            .split_whitespace()
            .find(|token| token.starts_with("https://") && token.contains("trycloudflare.com"))
            .map(|token| token.trim_matches('|').to_owned()),
    }
}

fn clear_output_state_keys(rules: &[OutputRule], state: &SessionState) -> Result<()> {
    if rules.is_empty() {
        return Ok(());
    }
    for rule in rules {
        state.set(&rule.state_key, "".into())?;
    }
    Ok(())
}

async fn terminate_child(name: &str, child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        info!("process {} already exited", name);
        return Ok(());
    }
    child
        .kill()
        .await
        .with_context(|| format!("failed to stop process '{name}'"))?;
    info!("stopped process {}", name);
    Ok(())
}

async fn wait_for_probe(
    client: &reqwest::Client,
    name: &str,
    probe: &ProbeSpec,
    state: &SessionState,
) -> Result<()> {
    let started = std::time::Instant::now();
    let timeout = match probe {
        ProbeSpec::Http { timeout_ms, .. } | ProbeSpec::StateKey { timeout_ms, .. } => {
            Duration::from_millis(*timeout_ms)
        }
    };
    let interval = Duration::from_millis(probe.interval());
    loop {
        if check_probe(client, name, probe, state).await.is_ok() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(timeout_error(name, probe));
        }
        sleep(interval).await;
    }
}

async fn check_probe(
    client: &reqwest::Client,
    name: &str,
    probe: &ProbeSpec,
    state: &SessionState,
) -> Result<()> {
    match probe {
        ProbeSpec::Http { url, .. } => match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                info!("process {} is healthy at {}", name, url);
                Ok(())
            }
            Ok(response) => Err(anyhow!(
                "probe for '{}' at {} returned {}",
                name,
                url,
                response.status()
            )),
            Err(error) => Err(anyhow!("probe for '{}' at {} failed: {}", name, url, error)),
        },
        ProbeSpec::StateKey { key, .. } => {
            let value = state.get_string(key)?;
            if value.is_some_and(|value| !value.trim().is_empty()) {
                info!("process {} is ready via state key {}", name, key);
                Ok(())
            } else {
                Err(anyhow!("state key '{}' is empty", key))
            }
        }
    }
}

fn timeout_error(name: &str, probe: &ProbeSpec) -> anyhow::Error {
    match probe {
        ProbeSpec::Http { url, .. } => {
            anyhow!("timed out waiting for process '{}' probe {}", name, url)
        }
        ProbeSpec::StateKey { key, .. } => {
            anyhow!("timed out waiting for process '{}' state key {}", name, key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OutputExtract, ProbeSpec};
    use serde_json::Value;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncReadExt;
    use tokio::sync::Mutex;

    fn unique_state_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("devloop-process-state-{unique}.json"))
    }

    #[test]
    fn extract_url_token_finds_cloudflare_url() {
        let rule = CompiledOutputRule {
            regex: None,
            state_key: "tunnel_url".into(),
            extract: OutputExtract::UrlToken,
            capture_group: 1,
        };

        let value = extract_output_value(
            &rule,
            "INF | Your quick Tunnel has been created! Visit it: https://abc.trycloudflare.com |",
        );

        assert_eq!(value.as_deref(), Some("https://abc.trycloudflare.com"));
    }

    #[test]
    fn format_output_line_prefixes_source() {
        let rendered = format_output_line(
            "tunnel cloudflared",
            "INF ready",
            false,
            OutputBodyStyle::Plain,
        );

        assert_eq!(rendered, "[tunnel cloudflared] INF ready");
    }

    #[test]
    fn format_output_line_colors_label_and_dims_body() {
        let rendered = format_output_line(
            "tunnel cloudflared",
            "INF ready",
            true,
            OutputBodyStyle::Dim,
        );

        assert!(rendered.contains("[tunnel cloudflared]"));
        assert!(rendered.starts_with("\u{1b}[2;1;"));
        assert!(rendered.contains("\u{1b}[2mINF ready\u{1b}[0m"));
    }

    #[test]
    fn render_output_byte_does_not_dim_newlines() {
        assert_eq!(
            render_output_byte(
                b'\n',
                true,
                OutputBodyStyle::Plain,
                &mut OutputRenderState::new(),
            ),
            "\n"
        );
    }

    #[test]
    fn render_output_byte_does_not_dim_carriage_returns() {
        assert_eq!(
            render_output_byte(
                b'\r',
                true,
                OutputBodyStyle::Plain,
                &mut OutputRenderState::new(),
            ),
            "\r"
        );
    }

    #[test]
    fn render_output_byte_preserves_ansi_escape_sequences() {
        let mut render_state = OutputRenderState::new();
        let rendered = [
            render_output_byte(0x1b, true, OutputBodyStyle::Dim, &mut render_state),
            render_output_byte(b'[', true, OutputBodyStyle::Dim, &mut render_state),
            render_output_byte(b'3', true, OutputBodyStyle::Dim, &mut render_state),
            render_output_byte(b'4', true, OutputBodyStyle::Dim, &mut render_state),
            render_output_byte(b'm', true, OutputBodyStyle::Dim, &mut render_state),
            render_output_byte(b'D', true, OutputBodyStyle::Dim, &mut render_state),
        ]
        .concat();

        assert_eq!(rendered, "\u{1b}[34m\u{1b}[2mD");
    }

    #[test]
    fn render_output_byte_reapplies_dim_after_reset_sequence() {
        let mut render_state = OutputRenderState::new();
        let rendered = [0x1b_u8, b'[', b'0', b'm']
            .into_iter()
            .map(|byte| render_output_byte(byte, true, OutputBodyStyle::Dim, &mut render_state))
            .collect::<String>();

        assert_eq!(rendered, "\u{1b}[0m\u{1b}[2m");
    }

    #[test]
    fn render_output_byte_preserves_utf8_multibyte_characters() {
        let mut render_state = OutputRenderState::new();
        let rendered = [0xCE_u8, 0xBC_u8, b's']
            .into_iter()
            .map(|byte| render_output_byte(byte, false, OutputBodyStyle::Plain, &mut render_state))
            .collect::<String>();

        assert_eq!(rendered, "\u{3bc}s");
        assert!(render_state.utf8_buffer.is_empty());
    }

    #[tokio::test]
    async fn write_output_byte_renders_carriage_return_as_visible_newline() {
        let (writer, mut reader) = tokio::io::duplex(64);
        let mut render_state = OutputRenderState {
            at_line_start: false,
            last_was_carriage_return: false,
            ansi_escape_state: AnsiEscapeState::None,
            utf8_buffer: Vec::new(),
            body_style: OutputBodyStyle::Plain,
            colorize: false,
            dim_active: false,
            rendered_line: String::new(),
        };
        let writer = Arc::new(Mutex::new(writer));

        write_output_byte_to_writer(
            &writer,
            "css_watch tailwindcss",
            b'\r',
            false,
            OutputBodyStyle::Plain,
            &mut render_state,
        )
        .await
        .expect("write carriage return");

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered carriage return");

        assert_eq!(rendered, "\n");
        assert!(render_state.at_line_start);
        assert!(render_state.last_was_carriage_return);
    }

    #[tokio::test]
    async fn write_output_byte_suppresses_line_feed_after_carriage_return() {
        let (writer, mut reader) = tokio::io::duplex(64);
        let mut render_state = OutputRenderState {
            at_line_start: true,
            last_was_carriage_return: true,
            ansi_escape_state: AnsiEscapeState::None,
            utf8_buffer: Vec::new(),
            body_style: OutputBodyStyle::Plain,
            colorize: false,
            dim_active: false,
            rendered_line: String::new(),
        };
        let writer = Arc::new(Mutex::new(writer));

        write_output_byte_to_writer(
            &writer,
            "css_watch tailwindcss",
            b'\n',
            false,
            OutputBodyStyle::Plain,
            &mut render_state,
        )
        .await
        .expect("write line feed");

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered line feed");

        assert_eq!(rendered, "");
        assert!(render_state.at_line_start);
        assert!(!render_state.last_was_carriage_return);
    }

    #[tokio::test]
    async fn write_output_byte_flushes_complete_line_atomically() {
        let (writer, mut reader) = tokio::io::duplex(256);
        let mut render_state = OutputRenderState::new();
        let writer = Arc::new(Mutex::new(writer));

        for byte in b"alpha\n".iter().copied() {
            write_output_byte_to_writer(
                &writer,
                "echo python3",
                byte,
                false,
                OutputBodyStyle::Plain,
                &mut render_state,
            )
            .await
            .expect("write byte");
        }

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered output");

        assert_eq!(rendered, "[echo python3] alpha\n");
    }

    #[tokio::test]
    async fn write_output_byte_preserves_utf8_multibyte_characters() {
        let (writer, mut reader) = tokio::io::duplex(256);
        let mut render_state = OutputRenderState::new();
        let writer = Arc::new(Mutex::new(writer));

        for byte in "Done in 73μs\n".as_bytes().iter().copied() {
            write_output_byte_to_writer(
                &writer,
                "css_watch tailwindcss",
                byte,
                false,
                OutputBodyStyle::Plain,
                &mut render_state,
            )
            .await
            .expect("write byte");
        }

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered output");

        assert_eq!(rendered, "[css_watch tailwindcss] Done in 73μs\n");
    }

    #[tokio::test]
    async fn write_output_byte_can_dim_entire_line_once() {
        let (writer, mut reader) = tokio::io::duplex(256);
        let mut render_state = OutputRenderState::new();
        let writer = Arc::new(Mutex::new(writer));

        for byte in b"alpha\n".iter().copied() {
            write_output_byte_to_writer(
                &writer,
                "echo python3",
                byte,
                true,
                OutputBodyStyle::Dim,
                &mut render_state,
            )
            .await
            .expect("write byte");
        }

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered output");

        assert!(rendered.starts_with("\u{1b}[2;1;"));
        assert!(rendered.contains("[echo python3]"));
        assert!(rendered.ends_with("\u{1b}[2malpha\u{1b}[0m\n"));
    }

    #[tokio::test]
    async fn write_captured_output_dims_hook_body_by_default() {
        let (writer, mut reader) = tokio::io::duplex(256);
        let writer = Arc::new(Mutex::new(writer));

        write_captured_output_to_writer(
            &writer,
            "build-css.sh build_css",
            b"Done in 73ms\n",
            OutputBodyStyle::Dim,
        )
        .await
        .expect("write captured output");

        drop(writer);

        let mut rendered = String::new();
        reader
            .read_to_string(&mut rendered)
            .await
            .expect("read rendered output");

        assert!(rendered.contains("[build-css.sh build_css]"));
        if should_colorize_output() {
            assert!(rendered.contains("\u{1b}[2mDone in 73ms\u{1b}[0m"));
        } else {
            assert!(rendered.ends_with("Done in 73ms\n"));
        }
    }

    #[test]
    fn process_output_source_label_uses_executable_before_process_name() {
        let label = process_output_source_label(
            "build_css",
            &["./scripts/build-css.sh".into(), "--watch".into()],
        );

        assert_eq!(label, "build-css.sh build_css");
    }

    #[test]
    fn executable_display_name_handles_plain_programs() {
        assert_eq!(executable_display_name("cloudflared"), "cloudflared");
    }

    #[test]
    fn configure_command_inherits_parent_rust_log_by_default() {
        let original = std::env::var_os("RUST_LOG");
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
        }

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &BTreeMap::new(),
                external_event_env: None,
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect("configure command");

        let rust_log = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("RUST_LOG"));

        assert!(
            rust_log.is_none(),
            "RUST_LOG should not be overridden in child env"
        );

        restore_rust_log(original);
    }

    #[test]
    fn configure_command_keeps_explicit_rust_log_override() {
        let original = std::env::var_os("RUST_LOG");
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
        }

        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".into(), "info,gcp_rust_blog=debug".into());

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &env,
                external_event_env: None,
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect("configure command");

        let rust_log = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("RUST_LOG"))
            .and_then(|(_, value)| value)
            .expect("explicit RUST_LOG should be preserved");

        assert_eq!(rust_log, "info,gcp_rust_blog=debug");

        restore_rust_log(original);
    }

    #[test]
    fn configure_command_injects_external_event_environment() {
        let env = BTreeMap::new();
        let external_event_env = ExternalEventEnvironment {
            base_url: "http://127.0.0.1:12345".into(),
            token: "secret".into(),
            event_urls: BTreeMap::from([(
                "browser_path".into(),
                "http://127.0.0.1:12345/events/browser_path".into(),
            )]),
        };

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &env,
                external_event_env: Some(&external_event_env),
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect("configure command");

        let envs = command.as_std().get_envs().collect::<Vec<_>>();
        assert!(envs.iter().any(|(key, value)| {
            *key == std::ffi::OsStr::new("DEVLOOP_EVENTS_TOKEN")
                && *value == Some(std::ffi::OsStr::new("secret"))
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == std::ffi::OsStr::new("DEVLOOP_EVENT_BROWSER_PATH_URL")
                && *value
                    == Some(std::ffi::OsStr::new(
                        "http://127.0.0.1:12345/events/browser_path",
                    ))
        }));
    }

    fn restore_rust_log(original: Option<std::ffi::OsString>) {
        match original {
            Some(value) => unsafe {
                std::env::set_var("RUST_LOG", value);
            },
            None => unsafe {
                std::env::remove_var("RUST_LOG");
            },
        }
    }

    #[test]
    fn output_color_code_is_stable_for_same_process() {
        assert_eq!(
            crate::output::output_color_code("tunnel"),
            crate::output::output_color_code("tunnel")
        );
    }

    #[test]
    fn process_output_byte_for_rules_handles_carriage_return_and_line_feed() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        let rules = vec![CompiledOutputRule {
            regex: Some(Regex::new(r"(https://\S+)").expect("regex")),
            state_key: "url".into(),
            extract: OutputExtract::Regex,
            capture_group: 1,
        }];
        let mut line_buffer = Vec::new();
        let mut last_was_carriage_return = false;

        for byte in b"https://example.test\r\n".iter().copied() {
            process_output_byte_for_rules(
                "tunnel",
                byte,
                &mut line_buffer,
                &mut last_was_carriage_return,
                &rules,
                &state,
            );
        }

        assert_eq!(
            state.get_string("url").expect("get url").as_deref(),
            Some("https://example.test")
        );

        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn process_output_byte_for_rules_leaves_incomplete_line_buffered() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        let rules = vec![CompiledOutputRule {
            regex: Some(Regex::new(r"(https://\S+)").expect("regex")),
            state_key: "url".into(),
            extract: OutputExtract::Regex,
            capture_group: 1,
        }];
        let mut line_buffer = Vec::new();
        let mut last_was_carriage_return = false;

        for byte in b"https://example.test".iter().copied() {
            process_output_byte_for_rules(
                "tunnel",
                byte,
                &mut line_buffer,
                &mut last_was_carriage_return,
                &rules,
                &state,
            );
        }

        assert_eq!(state.get_string("url").expect("get url"), None);
        assert_eq!(
            String::from_utf8(line_buffer).expect("utf8"),
            "https://example.test"
        );

        let _ = std::fs::remove_file(state_path);
    }

    #[tokio::test]
    async fn state_key_probe_reads_shared_session_state() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        state
            .set(
                "tunnel_url",
                Value::String("https://abc.trycloudflare.com".into()),
            )
            .expect("set tunnel_url");

        check_probe(
            &reqwest::Client::new(),
            "tunnel",
            &ProbeSpec::StateKey {
                key: "tunnel_url".into(),
                interval_ms: 100,
                timeout_ms: 1000,
            },
            &state,
        )
        .await
        .expect("probe should succeed");

        std::fs::remove_file(state_path).expect("cleanup state file");
    }
}
