use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub root: PathBuf,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    pub state_file: Option<PathBuf>,
    #[serde(default)]
    pub startup_workflows: Vec<String>,
    #[serde(default)]
    pub watch: BTreeMap<String, WatchGroup>,
    #[serde(default)]
    pub process: BTreeMap<String, ProcessSpec>,
    #[serde(default)]
    pub hook: BTreeMap<String, HookSpec>,
    #[serde(default)]
    pub workflow: BTreeMap<String, WorkflowSpec>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let mut config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config at {}", path.display()))?;
        let base = path
            .parent()
            .ok_or_else(|| anyhow!("config path {} has no parent directory", path.display()))?;
        config.root = absolutize(base, &config.root);
        config.state_file = Some(match config.state_file {
            Some(path) => absolutize(base, &path),
            None => config.root.join(".devloop").join("state.json"),
        });
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.root.exists() {
            return Err(anyhow!(
                "config root '{}' does not exist",
                self.root.display()
            ));
        }
        if self.watch.is_empty() {
            return Err(anyhow!("config must define at least one watch group"));
        }
        for (name, group) in &self.watch {
            if group.paths.is_empty() {
                return Err(anyhow!(
                    "watch group '{name}' must define at least one path"
                ));
            }
            if let Some(workflow) = group.workflow_name(name)
                && !self.workflow.contains_key(workflow)
            {
                return Err(anyhow!(
                    "watch group '{name}' references missing workflow '{workflow}'"
                ));
            }
        }
        for (name, process) in &self.process {
            process
                .validate()
                .with_context(|| format!("invalid process '{name}'"))?;
        }
        for (name, hook) in &self.hook {
            hook.validate()
                .with_context(|| format!("invalid hook '{name}'"))?;
        }
        for (name, workflow) in &self.workflow {
            workflow
                .validate(self)
                .with_context(|| format!("invalid workflow '{name}'"))?;
        }
        for workflow in &self.startup_workflows {
            if !self.workflow.contains_key(workflow) {
                return Err(anyhow!(
                    "startup workflow references missing workflow '{workflow}'"
                ));
            }
        }
        Ok(())
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }

    pub fn compiled_watchers(&self) -> Result<Vec<CompiledWatchGroup>> {
        self.watch
            .iter()
            .map(|(name, group)| group.compile(name))
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchGroup {
    pub paths: Vec<String>,
    pub workflow: Option<String>,
}

impl WatchGroup {
    pub fn workflow_name<'a>(&'a self, fallback: &'a str) -> Option<&'a str> {
        self.workflow.as_deref().or(Some(fallback))
    }

    fn compile(&self, name: &str) -> Result<CompiledWatchGroup> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.paths {
            builder
                .add(Glob::new(pattern).with_context(|| {
                    format!("invalid glob '{pattern}' in watch group '{name}'")
                })?);
        }
        Ok(CompiledWatchGroup {
            workflow: self.workflow.clone().unwrap_or_else(|| name.to_owned()),
            matchers: builder.build()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CompiledWatchGroup {
    pub workflow: String,
    matchers: GlobSet,
}

impl CompiledWatchGroup {
    pub fn matches(&self, relative_path: &Path) -> bool {
        self.matchers.is_match(relative_path)
    }

    #[cfg(test)]
    pub fn for_test(patterns: &[&str], workflow: &str) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(Glob::new(pattern)?);
        }
        Ok(Self {
            workflow: workflow.to_owned(),
            matchers: builder.build()?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessSpec {
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub autostart: bool,
    pub healthcheck: Option<HttpHealthcheck>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl ProcessSpec {
    fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(anyhow!("process command must not be empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpHealthcheck {
    pub url: String,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookSpec {
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub capture: Option<CaptureMode>,
    pub state_key: Option<String>,
}

impl HookSpec {
    fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(anyhow!("hook command must not be empty"));
        }
        if matches!(self.capture, Some(CaptureMode::Text)) && self.state_key.is_none() {
            return Err(anyhow!("text capture requires state_key"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    Ignore,
    Text,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowSpec {
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowSpec {
    fn validate(&self, config: &Config) -> Result<()> {
        if self.steps.is_empty() {
            return Err(anyhow!("workflow must contain at least one step"));
        }
        for step in &self.steps {
            match step {
                WorkflowStep::StartProcess { process }
                | WorkflowStep::StopProcess { process }
                | WorkflowStep::RestartProcess { process }
                | WorkflowStep::WaitForProcess { process } => {
                    if !config.process.contains_key(process) {
                        return Err(anyhow!("workflow references missing process '{process}'"));
                    }
                }
                WorkflowStep::RunHook { hook } => {
                    if !config.hook.contains_key(hook) {
                        return Err(anyhow!("workflow references missing hook '{hook}'"));
                    }
                }
                WorkflowStep::SleepMs { .. } | WorkflowStep::WriteState { .. } => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkflowStep {
    StartProcess { process: String },
    StopProcess { process: String },
    RestartProcess { process: String },
    WaitForProcess { process: String },
    RunHook { hook: String },
    SleepMs { duration_ms: u64 },
    WriteState { key: String, value: String },
}

pub fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn default_debounce_ms() -> u64 {
    250
}

fn default_interval_ms() -> u64 {
    250
}

fn default_timeout_ms() -> u64 {
    15_000
}

fn default_true() -> bool {
    true
}
