use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::{
    Config, HookSpec, OutputExtract, OutputRule, ProbeSpec, ProcessSpec, RestartPolicy,
};
use crate::state::SessionState;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, ManagedProcess>,
    client: reqwest::Client,
    event_tx: tokio::sync::mpsc::UnboundedSender<ProcessEvent>,
    observed_state: Arc<Mutex<BTreeMap<String, String>>>,
}

struct ManagedProcess {
    child: Child,
    last_liveness_check: Option<std::time::Instant>,
}

#[derive(Debug)]
pub enum ProcessEvent {
    StateUpdate { key: String, value: String },
}

impl<'a> ProcessManager<'a> {
    pub fn new(config: &'a Config) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ProcessEvent>) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let observed_state = Arc::new(Mutex::new(BTreeMap::new()));
        (
            Self {
                config,
                children: BTreeMap::new(),
                client: reqwest::Client::new(),
                event_tx,
                observed_state,
            },
            event_rx,
        )
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

    pub async fn wait_for_named(&self, name: &str, state: &mut SessionState) -> Result<()> {
        let spec = self
            .config
            .process
            .get(name)
            .ok_or_else(|| anyhow!("unknown process '{name}'"))?;
        if let Some(check) = &spec.readiness {
            wait_for_probe(&self.client, name, check, state, &self.observed_state).await?;
        }
        Ok(())
    }

    pub async fn run_hook(
        &self,
        name: &str,
        state: &mut SessionState,
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

    pub fn apply_event(&self, event: ProcessEvent, state: &mut SessionState) -> Result<()> {
        match event {
            ProcessEvent::StateUpdate { key, value } => {
                state.set(key, value.into())?;
                sync_current_post_url(state)
            }
        }
    }

    pub async fn maintain(&mut self, state: &mut SessionState) -> Result<()> {
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
                    let result =
                        check_probe(&self.client, &name, liveness, state, &self.observed_state)
                            .await;
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
        clear_output_state_keys(&spec.output.rules, state.path())?;
        clear_observed_state_keys(&self.observed_state, &spec.output.rules)?;
        let process_name = name.to_owned();
        let inherit_output = spec.output.inherit;
        let rules = compile_output_rules(&spec.output.rules)?;
        let observed_state = Arc::clone(&self.observed_state);
        let event_tx = self.event_tx.clone();
        let state_path = state.path().to_path_buf();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(forward_output_lines(
                stdout,
                process_name.clone(),
                inherit_output,
                rules.clone(),
                Arc::clone(&observed_state),
                event_tx.clone(),
                state_path.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_output_lines(
                stderr,
                process_name,
                inherit_output,
                rules,
                observed_state,
                event_tx,
                state_path,
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
    observed_state: Arc<Mutex<BTreeMap<String, String>>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<ProcessEvent>,
    state_path: PathBuf,
) where
    T: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if inherit_output {
            println!("{line}");
        }
        for rule in &rules {
            if let Some(value) = extract_output_value(rule, &line) {
                if let Ok(mut state) = observed_state.lock() {
                    state.insert(rule.state_key.clone(), value.clone());
                }
                match SessionState::load(state_path.clone()) {
                    Ok(mut state) => {
                        if let Err(error) = state.set(&rule.state_key, value.clone().into()) {
                            error!(
                                "failed to persist output state for {} key {}: {}",
                                process_name, rule.state_key, error
                            );
                        }
                    }
                    Err(error) => {
                        error!(
                            "failed to load state file for {} key {}: {}",
                            process_name, rule.state_key, error
                        );
                    }
                }
                if event_tx
                    .send(ProcessEvent::StateUpdate {
                        key: rule.state_key.clone(),
                        value,
                    })
                    .is_err()
                {
                    error!("failed to publish output state for {}", process_name);
                    return;
                }
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

fn apply_hook_capture(spec: &HookSpec, stdout: &str, state: &mut SessionState) -> Result<()> {
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
    }?;
    sync_current_post_url(state)
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

fn clear_output_state_keys(rules: &[OutputRule], state_path: &Path) -> Result<()> {
    if rules.is_empty() {
        return Ok(());
    }
    let mut state = SessionState::load(state_path.to_path_buf())?;
    for rule in rules {
        state.set(&rule.state_key, "".into())?;
    }
    Ok(())
}

fn sync_current_post_url(state: &mut SessionState) -> Result<()> {
    let tunnel_url = state.get_string("tunnel_url")?;
    let slug = state.get_string("current_post_slug")?;
    if let (Some(tunnel_url), Some(slug)) = (tunnel_url, slug)
        && !tunnel_url.trim().is_empty()
        && !slug.trim().is_empty()
    {
        state.set(
            "current_post_url",
            format!("{}/posts/{}", tunnel_url.trim_end_matches('/'), slug.trim()).into(),
        )?;
    }
    Ok(())
}

fn clear_observed_state_keys(
    observed_state: &Arc<Mutex<BTreeMap<String, String>>>,
    rules: &[OutputRule],
) -> Result<()> {
    let mut state = observed_state
        .lock()
        .map_err(|_| anyhow!("observed state mutex was poisoned"))?;
    for rule in rules {
        state.insert(rule.state_key.clone(), String::new());
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
    state: &mut SessionState,
    observed_state: &Arc<Mutex<BTreeMap<String, String>>>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let timeout = match probe {
        ProbeSpec::Http { timeout_ms, .. } | ProbeSpec::StateKey { timeout_ms, .. } => {
            Duration::from_millis(*timeout_ms)
        }
    };
    let interval = Duration::from_millis(probe.interval());
    loop {
        if check_probe(client, name, probe, state, observed_state)
            .await
            .is_ok()
        {
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
    state: &mut SessionState,
    observed_state: &Arc<Mutex<BTreeMap<String, String>>>,
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
            let observed = observed_state
                .lock()
                .map_err(|_| anyhow!("observed state mutex was poisoned"))?
                .get(key)
                .cloned();
            let value = observed.or_else(|| state.get_string(key).ok().flatten());
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                state.set(key, value.into())?;
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
