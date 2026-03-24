use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{Config, HookSpec, HttpHealthcheck, ProcessSpec};
use crate::state::SessionState;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, Child>,
    client: reqwest::Client,
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
        terminate_child(name, &mut child).await
    }

    pub async fn restart_named(&mut self, name: &str, state: &SessionState) -> Result<()> {
        self.stop_named(name).await?;
        self.start_named(name, state).await
    }

    pub async fn wait_for_named(&self, name: &str) -> Result<()> {
        let spec = self
            .config
            .process
            .get(name)
            .ok_or_else(|| anyhow!("unknown process '{name}'"))?;
        if let Some(check) = &spec.healthcheck {
            wait_for_http(&self.client, name, check).await?;
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
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let child = command
            .spawn()
            .with_context(|| format!("failed to start process '{name}'"))?;
        self.children.insert(name.to_owned(), child);
        info!("started process {}", name);
        Ok(())
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
    }
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

async fn wait_for_http(
    client: &reqwest::Client,
    name: &str,
    check: &HttpHealthcheck,
) -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_millis(check.timeout_ms);
    let interval = Duration::from_millis(check.interval_ms);
    loop {
        match client.get(&check.url).send().await {
            Ok(response) if response.status().is_success() => {
                info!("process {} is healthy at {}", name, check.url);
                return Ok(());
            }
            Ok(response) => {
                warn!(
                    "waiting for process {} healthcheck {} returned {}",
                    name,
                    check.url,
                    response.status()
                );
            }
            Err(error) => {
                warn!(
                    "waiting for process {} healthcheck {} failed: {}",
                    name, check.url, error
                );
            }
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!(
                "timed out waiting for process '{name}' healthcheck {}",
                check.url
            ));
        }
        sleep(interval).await;
    }
}
