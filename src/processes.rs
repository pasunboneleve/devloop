use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{
    Config, HookSpec, OutputExtract, OutputRule, ProbeSpec, ProcessSpec, RestartPolicy,
};
use crate::output::{dim_text, format_output_prefix, should_colorize_output};
use crate::state::SessionState;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, ManagedProcess>,
    client: reqwest::Client,
    shutting_down: bool,
}

struct ManagedProcess {
    child: Child,
    last_liveness_check: Option<std::time::Instant>,
}

impl<'a> ProcessManager<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            children: BTreeMap::new(),
            client: reqwest::Client::new(),
            shutting_down: false,
        }
    }

    pub async fn start_autostart(&mut self, state: &SessionState) -> Result<()> {
        for (name, process) in &self.config.process {
            if process.autostart {
                self.start(name, process, state).await?;
            }
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
        terminate_child(name, &mut child.child).await
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
            &spec.env,
            &self.config.root,
            state.path(),
            changed_files,
            workflow,
        )?;
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to run hook '{name}'"))?;
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

    pub async fn stop_all(&mut self) -> Result<()> {
        self.initiate_shutdown();
        let names: Vec<String> = self.children.keys().cloned().collect();
        for name in names {
            self.stop_named(&name).await?;
        }
        Ok(())
    }

    pub async fn maintain(&mut self, state: &SessionState) -> Result<()> {
        let names: Vec<String> = self.children.keys().cloned().collect();
        for name in names {
            let Some(spec) = self.config.process.get(&name) else {
                continue;
            };

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
                if should_restart(spec.restart, status.success(), self.shutting_down) {
                    self.start_named(&name, state).await?;
                }
                continue;
            }

            if let Some(liveness) = &spec.liveness {
                let should_check = {
                    let managed = self
                        .children
                        .get(&name)
                        .ok_or_else(|| anyhow!("missing managed process '{name}'"))?;
                    managed.last_liveness_check.is_none_or(|last| {
                        last.elapsed() >= Duration::from_millis(liveness.interval())
                    })
                };
                if should_check {
                    let result = check_probe(&self.client, &name, liveness, state).await;
                    if let Some(managed) = self.children.get_mut(&name) {
                        managed.last_liveness_check = Some(std::time::Instant::now());
                    }
                    if let Err(error) = result {
                        warn!("liveness probe failed for {}: {}", name, error);
                        if spec.restart != RestartPolicy::Never && !self.shutting_down {
                            self.restart_named(&name, state).await?;
                        }
                    }
                }
            }
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
            &spec.env,
            &self.config.root,
            state.path(),
            &[],
            "startup",
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
        let rules = compile_output_rules(&spec.output.rules)?;
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(forward_output_lines(
                stdout,
                process_name.clone(),
                source_label.clone(),
                inherit_output,
                rules.clone(),
                state.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_output_lines(
                stderr,
                process_name,
                source_label,
                inherit_output,
                rules,
                state.clone(),
            ));
        }
        self.children.insert(
            name.to_owned(),
            ManagedProcess {
                child,
                last_liveness_check: None,
            },
        );
        info!("started process {}", name);
        Ok(())
    }

    pub fn initiate_shutdown(&mut self) {
        self.shutting_down = true;
    }
}

#[derive(Clone)]
struct CompiledOutputRule {
    regex: Option<Regex>,
    state_key: String,
    extract: OutputExtract,
    capture_group: usize,
}

async fn forward_output_lines<T>(
    reader: T,
    process_name: String,
    source_label: String,
    inherit_output: bool,
    rules: Vec<CompiledOutputRule>,
    state: SessionState,
) where
    T: tokio::io::AsyncRead + Unpin,
{
    let colorize = should_colorize_output();
    let mut reader = reader;
    let mut stdout = tokio::io::stdout();
    let mut chunk = [0_u8; 4096];
    let mut line_buffer = Vec::new();
    let mut at_line_start = true;
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
            if inherit_output
                && !output_failed
                && let Err(error) = write_output_byte(
                    &mut stdout,
                    &process_name,
                    &source_label,
                    byte,
                    colorize,
                    &mut at_line_start,
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
}

#[cfg(test)]
fn format_output_line(source_label: &str, line: &str, colorize: bool) -> String {
    let prefix = format_output_prefix(source_label, colorize);
    let body = if colorize {
        dim_text(line)
    } else {
        line.to_owned()
    };
    format!("{prefix}{body}")
}

async fn write_output_byte(
    stdout: &mut tokio::io::Stdout,
    _process_name: &str,
    source_label: &str,
    byte: u8,
    colorize: bool,
    at_line_start: &mut bool,
) -> std::io::Result<()> {
    if *at_line_start {
        let prefix = format_output_prefix(source_label, colorize);
        stdout.write_all(prefix.as_bytes()).await?;
        *at_line_start = false;
    }

    let rendered = render_output_byte(byte, colorize);
    stdout.write_all(rendered.as_bytes()).await?;

    if matches!(byte, b'\n' | b'\r') {
        stdout.flush().await?;
        *at_line_start = true;
    }

    Ok(())
}

fn render_output_byte(byte: u8, colorize: bool) -> String {
    let text = String::from_utf8_lossy(&[byte]).into_owned();
    if !colorize || byte.is_ascii_control() {
        return text;
    }
    dim_text(&text)
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
    format!("{name} {executable}")
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
    env: &BTreeMap<String, String>,
    root: &Path,
    state_path: &Path,
    changed_files: &[String],
    workflow: &str,
) -> Result<Command> {
    let Some(program) = command.first() else {
        return Err(anyhow!("command must not be empty"));
    };
    let program = resolve_program(root, program);
    let mut cmd = Command::new(program);
    cmd.args(&command[1..]);
    cmd.current_dir(cwd);
    cmd.envs(env);
    cmd.env("DEVLOOP_ROOT", root);
    cmd.env("DEVLOOP_STATE", state_path);
    cmd.env("DEVLOOP_WORKFLOW", workflow);
    cmd.env(
        "DEVLOOP_CHANGED_FILES_JSON",
        serde_json::to_string(changed_files)?,
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

fn should_restart(policy: RestartPolicy, success: bool, shutting_down: bool) -> bool {
    if shutting_down {
        return false;
    }
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => !success,
        RestartPolicy::Always => true,
    }
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
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let rendered = format_output_line("tunnel cloudflared", "INF ready", false);

        assert_eq!(rendered, "[tunnel cloudflared] INF ready");
    }

    #[test]
    fn format_output_line_colors_label_and_dims_body() {
        let rendered = format_output_line("tunnel cloudflared", "INF ready", true);

        assert!(rendered.contains("[tunnel cloudflared]"));
        assert!(rendered.contains("\u{1b}[2mINF ready\u{1b}[0m"));
        assert!(rendered.starts_with("\u{1b}[1;"));
    }

    #[test]
    fn render_output_byte_does_not_dim_newlines() {
        assert_eq!(render_output_byte(b'\n', true), "\n");
    }

    #[test]
    fn render_output_byte_does_not_dim_carriage_returns() {
        assert_eq!(render_output_byte(b'\r', true), "\r");
    }

    #[tokio::test]
    async fn write_output_byte_marks_carriage_return_as_line_start() {
        let mut stdout = tokio::io::stdout();
        let mut at_line_start = false;

        write_output_byte(
            &mut stdout,
            "css_watch",
            "css_watch tailwindcss",
            b'\r',
            false,
            &mut at_line_start,
        )
        .await
        .expect("write carriage return");

        assert!(at_line_start);
    }

    #[test]
    fn process_output_source_label_uses_process_name_and_executable() {
        let label = process_output_source_label(
            "build_css",
            &["./scripts/build-css.sh".into(), "--watch".into()],
        );

        assert_eq!(label, "build_css build-css.sh");
    }

    #[test]
    fn executable_display_name_handles_plain_programs() {
        assert_eq!(executable_display_name("cloudflared"), "cloudflared");
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

    #[test]
    fn should_not_restart_while_shutting_down() {
        assert!(!should_restart(RestartPolicy::Always, true, true));
        assert!(!should_restart(RestartPolicy::OnFailure, false, true));
    }
}
