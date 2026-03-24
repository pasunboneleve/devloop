use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{
    Config, HookSpec, OutputExtract, OutputRule, ProbeSpec, ProcessSpec, RestartPolicy,
};
use crate::state::SessionState;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, ManagedProcess>,
    client: reqwest::Client,
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
                if should_restart(spec.restart, status.success()) {
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
                        if spec.restart != RestartPolicy::Never {
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
        if spec.output.rules.is_empty() {
            command.stdout(Stdio::inherit());
            command.stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start process '{name}'"))?;
        clear_output_state_keys(&spec.output.rules, state)?;
        let process_name = name.to_owned();
        let inherit_output = spec.output.inherit;
        let rules = compile_output_rules(&spec.output.rules)?;
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(forward_output_lines(
                stdout,
                process_name.clone(),
                inherit_output,
                rules.clone(),
                state.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_output_lines(
                stderr,
                process_name,
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
    inherit_output: bool,
    rules: Vec<CompiledOutputRule>,
    state: SessionState,
) where
    T: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if inherit_output {
            println!("{line}");
        }
        for rule in &rules {
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

fn should_restart(policy: RestartPolicy, success: bool) -> bool {
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
