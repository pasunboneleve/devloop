use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
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
    pub event_server: EventServerConfig,
    #[serde(default)]
    pub browser_reload_server: BrowserReloadServerConfig,
    #[serde(default)]
    pub event: BTreeMap<String, EventSpec>,
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
            if let Some(observe) = &hook.observe
                && !self.workflow.contains_key(&observe.workflow)
            {
                return Err(anyhow!(
                    "hook '{name}' observes missing workflow '{}'",
                    observe.workflow
                ));
            }
        }
        self.event_server.validate()?;
        self.browser_reload_server.validate()?;
        for (name, event) in &self.event {
            event
                .validate()
                .with_context(|| format!("invalid event '{name}'"))?;
            if !self.workflow.contains_key(&event.workflow) {
                return Err(anyhow!(
                    "event '{name}' references missing workflow '{}'",
                    event.workflow
                ));
            }
        }
        for (name, workflow) in &self.workflow {
            workflow
                .validate(self, name)
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

    pub fn has_external_events(&self) -> bool {
        !self.event.is_empty()
    }

    pub fn has_browser_reload_notifications(&self) -> bool {
        self.workflow
            .keys()
            .any(|name| self.workflow_uses_notify_reload(name, &mut Vec::new()))
    }

    fn workflow_uses_notify_reload(&self, workflow_name: &str, stack: &mut Vec<String>) -> bool {
        let Some(workflow) = self.workflow.get(workflow_name) else {
            return false;
        };
        if stack.iter().any(|name| name == workflow_name) {
            return false;
        }

        stack.push(workflow_name.to_string());
        let found = workflow.steps.iter().any(|step| match step {
            WorkflowStep::NotifyReload => true,
            WorkflowStep::RunWorkflow { workflow } => {
                self.workflow_uses_notify_reload(workflow, stack)
            }
            _ => false,
        }) || workflow
            .triggers
            .iter()
            .any(|trigger| self.workflow_uses_notify_reload(trigger, stack));
        stack.pop();
        found
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
    pub readiness: Option<ProbeSpec>,
    pub liveness: Option<ProbeSpec>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub output: OutputConfig,
}

impl ProcessSpec {
    fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(anyhow!("process command must not be empty"));
        }
        if let Some(readiness) = &self.readiness {
            readiness.validate()?;
        }
        if let Some(liveness) = &self.liveness {
            liveness.validate()?;
        }
        self.output.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeSpec {
    Http {
        url: String,
        #[serde(default = "default_interval_ms")]
        interval_ms: u64,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    StateKey {
        key: String,
        #[serde(default = "default_interval_ms")]
        interval_ms: u64,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
}

impl ProbeSpec {
    pub fn interval(&self) -> u64 {
        match self {
            Self::Http { interval_ms, .. } | Self::StateKey { interval_ms, .. } => *interval_ms,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Http { url, .. } => {
                if url.trim().is_empty() {
                    return Err(anyhow!("http probe url must not be empty"));
                }
            }
            Self::StateKey { key, .. } => {
                if key.trim().is_empty() {
                    return Err(anyhow!("state_key probe key must not be empty"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_true")]
    pub inherit: bool,
    #[serde(default)]
    pub body_style: OutputBodyStyle,
    #[serde(default)]
    pub rules: Vec<OutputRule>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            inherit: true,
            body_style: OutputBodyStyle::Plain,
            rules: Vec::new(),
        }
    }
}

impl OutputConfig {
    fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            rule.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputRule {
    pub state_key: String,
    pub pattern: Option<String>,
    #[serde(default)]
    pub extract: OutputExtract,
    #[serde(default = "default_capture_group")]
    pub capture_group: usize,
}

impl OutputRule {
    fn validate(&self) -> Result<()> {
        if self.state_key.trim().is_empty() {
            return Err(anyhow!("output rule state_key must not be empty"));
        }
        if let Some(pattern) = &self.pattern {
            Regex::new(pattern)
                .with_context(|| format!("invalid output rule regex '{}'", pattern))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputBodyStyle {
    #[default]
    Plain,
    Dim,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputExtract {
    #[default]
    Regex,
    UrlToken,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookSpec {
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_hook_output_config")]
    pub output: HookOutputConfig,
    pub capture: Option<CaptureMode>,
    pub state_key: Option<String>,
    #[serde(default)]
    pub observe: Option<ObservedHookSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventServerConfig {
    #[serde(default = "default_event_server_bind")]
    pub bind: String,
}

impl Default for EventServerConfig {
    fn default() -> Self {
        Self {
            bind: default_event_server_bind(),
        }
    }
}

impl EventServerConfig {
    fn validate(&self) -> Result<()> {
        self.bind
            .parse::<SocketAddr>()
            .map(|_| ())
            .with_context(|| {
                format!(
                    "event_server bind '{}' is not a valid socket address",
                    self.bind
                )
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserReloadServerConfig {
    #[serde(default = "default_browser_reload_server_bind")]
    pub bind: String,
}

impl Default for BrowserReloadServerConfig {
    fn default() -> Self {
        Self {
            bind: default_browser_reload_server_bind(),
        }
    }
}

impl BrowserReloadServerConfig {
    fn validate(&self) -> Result<()> {
        self.bind
            .parse::<SocketAddr>()
            .map(|_| ())
            .with_context(|| {
                format!(
                    "browser_reload_server bind '{}' is not a valid socket address",
                    self.bind
                )
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventSpec {
    pub state_key: String,
    pub workflow: String,
    pub pattern: Option<String>,
}

impl EventSpec {
    fn validate(&self) -> Result<()> {
        if self.state_key.trim().is_empty() {
            return Err(anyhow!("event state_key must not be empty"));
        }
        if self.workflow.trim().is_empty() {
            return Err(anyhow!("event workflow must not be empty"));
        }
        if let Some(pattern) = &self.pattern {
            Regex::new(pattern).with_context(|| format!("invalid event regex '{}'", pattern))?;
        }
        Ok(())
    }
}

impl HookSpec {
    fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(anyhow!("hook command must not be empty"));
        }
        self.output.validate()?;
        if matches!(self.capture, Some(CaptureMode::Text)) && self.state_key.is_none() {
            return Err(anyhow!("text capture requires state_key"));
        }
        if let Some(observe) = &self.observe {
            observe.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservedHookSpec {
    pub workflow: String,
    #[serde(default = "default_observe_interval_ms")]
    pub interval_ms: u64,
}

impl ObservedHookSpec {
    fn validate(&self) -> Result<()> {
        if self.workflow.trim().is_empty() {
            return Err(anyhow!("observed hook workflow must not be empty"));
        }
        if self.interval_ms == 0 {
            return Err(anyhow!(
                "observed hook interval_ms must be greater than zero"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookOutputConfig {
    #[serde(default = "default_true")]
    pub inherit: bool,
    #[serde(default = "default_hook_body_style")]
    pub body_style: OutputBodyStyle,
}

impl Default for HookOutputConfig {
    fn default() -> Self {
        default_hook_output_config()
    }
}

impl HookOutputConfig {
    fn validate(&self) -> Result<()> {
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
    #[serde(default)]
    pub triggers: Vec<String>,
}

impl WorkflowSpec {
    fn validate(&self, config: &Config, workflow_name: &str) -> Result<()> {
        self.validate_inner(config, workflow_name, &mut Vec::new())
    }

    fn validate_inner(
        &self,
        config: &Config,
        workflow_name: &str,
        stack: &mut Vec<String>,
    ) -> Result<()> {
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
                WorkflowStep::RunWorkflow { workflow } => {
                    validate_nested_workflow(config, stack, workflow)?;
                }
                WorkflowStep::SleepMs { .. }
                | WorkflowStep::WriteState { .. }
                | WorkflowStep::Log { .. }
                | WorkflowStep::NotifyReload => {}
            }
        }
        for workflow in &self.triggers {
            validate_nested_workflow(config, stack, workflow)?;
        }
        if !self.triggers.is_empty() {
            validate_execution_tree_overlap(config, workflow_name)?;
        }
        Ok(())
    }
}

fn validate_execution_tree_overlap(config: &Config, workflow_name: &str) -> Result<()> {
    let (trigger_targets, inline_reachable) = execution_tree_overlap_sets(config, workflow_name);

    for workflow in trigger_targets {
        if inline_reachable.contains(&workflow) {
            return Err(anyhow!(
                "workflow '{workflow}' is reachable both as a trigger target and via run_workflow in the same execution tree"
            ));
        }
    }

    Ok(())
}

fn execution_tree_overlap_sets(
    config: &Config,
    workflow_name: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut stack = vec![(workflow_name.to_string(), false)];
    let mut seen_plain = HashSet::new();
    let mut seen_inline = HashSet::new();
    let mut trigger_targets = BTreeSet::new();
    let mut inline_reachable = BTreeSet::new();

    while let Some((workflow_name, used_inline)) = stack.pop() {
        let seen_set = if used_inline {
            &mut seen_inline
        } else {
            &mut seen_plain
        };
        if !seen_set.insert(workflow_name.clone()) {
            continue;
        }
        if used_inline {
            inline_reachable.insert(workflow_name.clone());
        }

        let Some(workflow) = config.workflow.get(&workflow_name) else {
            continue;
        };

        for step in &workflow.steps {
            if let WorkflowStep::RunWorkflow { workflow } = step {
                stack.push((workflow.clone(), true));
            }
        }
        for trigger in &workflow.triggers {
            if !used_inline {
                trigger_targets.insert(trigger.clone());
            }
            stack.push((trigger.clone(), used_inline));
        }
    }

    (trigger_targets, inline_reachable)
}

fn validate_nested_workflow(
    config: &Config,
    stack: &mut Vec<String>,
    workflow: &str,
) -> Result<()> {
    if stack.iter().any(|name| name == workflow) {
        let mut cycle = stack.clone();
        cycle.push(workflow.to_string());
        return Err(anyhow!(
            "workflow recursion detected: {}",
            cycle.join(" -> ")
        ));
    }
    let nested = config
        .workflow
        .get(workflow)
        .ok_or_else(|| anyhow!("workflow references missing workflow '{workflow}'"))?;
    stack.push(workflow.to_string());
    nested.validate_inner(config, workflow, stack)?;
    stack.pop();
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkflowStep {
    StartProcess {
        process: String,
    },
    StopProcess {
        process: String,
    },
    RestartProcess {
        process: String,
    },
    WaitForProcess {
        process: String,
    },
    RunHook {
        hook: String,
    },
    RunWorkflow {
        workflow: String,
    },
    SleepMs {
        duration_ms: u64,
    },
    WriteState {
        key: String,
        value: String,
    },
    Log {
        message: String,
        #[serde(default)]
        style: LogStyle,
    },
    NotifyReload,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogStyle {
    #[default]
    Plain,
    Boxed,
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
    500
}

fn default_timeout_ms() -> u64 {
    15_000
}

fn default_true() -> bool {
    true
}

fn default_capture_group() -> usize {
    1
}

fn default_hook_body_style() -> OutputBodyStyle {
    OutputBodyStyle::Dim
}

fn default_hook_output_config() -> HookOutputConfig {
    HookOutputConfig {
        inherit: true,
        body_style: OutputBodyStyle::Dim,
    }
}

fn default_observe_interval_ms() -> u64 {
    1_000
}

fn default_event_server_bind() -> String {
    "127.0.0.1:0".to_string()
}

fn default_browser_reload_server_bind() -> String {
    "127.0.0.1:0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: EventServerConfig::default(),
            browser_reload_server: BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        }
    }

    #[test]
    fn validate_rejects_recursive_workflows() {
        let mut config = base_config();
        config.workflow.insert(
            "outer".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "inner".into(),
                }],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "inner".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "outer".into(),
                }],
                triggers: vec![],
            },
        );

        let error = config.workflow["outer"]
            .validate(&config, "outer")
            .expect_err("recursive workflow should fail");
        assert!(error.to_string().contains("workflow recursion detected"));
    }

    #[test]
    fn validate_rejects_recursive_trigger_workflows() {
        let mut config = base_config();
        config.workflow.insert(
            "outer".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "outer".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["inner".into()],
            },
        );
        config.workflow.insert(
            "inner".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "inner".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["outer".into()],
            },
        );

        let error = config.workflow["outer"]
            .validate(&config, "outer")
            .expect_err("recursive trigger workflow should fail");
        assert!(error.to_string().contains("workflow recursion detected"));
    }

    #[test]
    fn validate_rejects_missing_nested_workflow() {
        let mut config = base_config();
        config.workflow.insert(
            "outer".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "missing".into(),
                }],
                triggers: vec![],
            },
        );

        let error = config.workflow["outer"]
            .validate(&config, "outer")
            .expect_err("missing nested workflow should fail");
        assert!(error.to_string().contains("missing workflow 'missing'"));
    }

    #[test]
    fn validate_rejects_missing_trigger_workflow() {
        let mut config = base_config();
        config.workflow.insert(
            "outer".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "outer".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["missing".into()],
            },
        );

        let error = config.workflow["outer"]
            .validate(&config, "outer")
            .expect_err("missing trigger workflow should fail");
        assert!(error.to_string().contains("missing workflow 'missing'"));
    }

    #[test]
    fn validate_rejects_overlap_between_inline_and_triggered_workflows() {
        let mut config = base_config();
        config.workflow.insert(
            "browser_reload".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "css".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "browser_reload".into(),
                }],
                triggers: vec!["browser_reload".into()],
            },
        );

        let error = config.workflow["css"]
            .validate(&config, "css")
            .expect_err("overlapping trigger and inline workflow should fail");
        assert!(
            error
                .to_string()
                .contains("reachable both as a trigger target and via run_workflow")
        );
    }

    #[test]
    fn validate_rejects_trigger_reachable_via_inline_from_sibling_trigger() {
        let mut config = base_config();
        config.workflow.insert(
            "c".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "b".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "c".into(),
                }],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "a".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "a".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["b".into(), "c".into()],
            },
        );

        let error = config.workflow["a"]
            .validate(&config, "a")
            .expect_err("sibling trigger overlap should fail");
        assert!(
            error.to_string().contains(
                "workflow 'c' is reachable both as a trigger target and via run_workflow"
            )
        );
    }

    #[test]
    fn validate_rejects_nested_trigger_reachable_via_inline_path() {
        let mut config = base_config();
        config.workflow.insert(
            "d".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "b".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "b".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["d".into()],
            },
        );
        config.workflow.insert(
            "c".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "d".into(),
                }],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "a".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "a".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["b".into(), "c".into()],
            },
        );

        let error = config.workflow["a"]
            .validate(&config, "a")
            .expect_err("nested trigger overlap should fail");
        assert!(
            error.to_string().contains(
                "workflow 'd' is reachable both as a trigger target and via run_workflow"
            )
        );
    }

    #[test]
    fn validate_allows_inline_workflow_with_independent_triggers() {
        let mut config = base_config();
        config.workflow.insert(
            "c".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "d".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "b".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "b".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["d".into()],
            },
        );
        config.workflow.insert(
            "a".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "b".into(),
                }],
                triggers: vec!["c".into()],
            },
        );

        config.workflow["a"]
            .validate(&config, "a")
            .expect("independent inline triggers should be allowed");
    }

    #[test]
    fn output_config_defaults_to_inherited_output() {
        let config: OutputConfig = toml::from_str("").expect("parse default output config");

        assert!(config.inherit);
        assert_eq!(config.body_style, OutputBodyStyle::Plain);
        assert!(config.rules.is_empty());
    }

    #[test]
    fn process_spec_defaults_to_inherited_output_when_output_block_is_omitted() {
        let process: ProcessSpec =
            toml::from_str("command = [\"tailwindcss\", \"--watch\"]").expect("parse process");

        assert!(process.output.inherit);
        assert_eq!(process.output.body_style, OutputBodyStyle::Plain);
        assert!(process.output.rules.is_empty());
    }

    #[test]
    fn output_config_parses_body_style() {
        let config: OutputConfig =
            toml::from_str("body_style = \"dim\"").expect("parse output config");

        assert_eq!(config.body_style, OutputBodyStyle::Dim);
    }

    #[test]
    fn hook_output_defaults_to_dimmed_inherited_output() {
        let config: HookOutputConfig = toml::from_str("").expect("parse hook output config");

        assert!(config.inherit);
        assert_eq!(config.body_style, OutputBodyStyle::Dim);
    }

    #[test]
    fn hook_spec_defaults_to_dimmed_inherited_output() {
        let hook: HookSpec = toml::from_str("command = [\"echo\", \"ok\"]").expect("parse hook");

        assert!(hook.output.inherit);
        assert_eq!(hook.output.body_style, OutputBodyStyle::Dim);
    }

    #[test]
    fn hook_output_parses_plain_body_style_override() {
        let config: HookOutputConfig =
            toml::from_str("body_style = \"plain\"").expect("parse hook output config");

        assert_eq!(config.body_style, OutputBodyStyle::Plain);
    }

    #[test]
    fn hook_observe_defaults_interval() {
        let observe: ObservedHookSpec =
            toml::from_str("workflow = \"publish\"").expect("parse observe config");

        assert_eq!(observe.workflow, "publish");
        assert_eq!(observe.interval_ms, 1_000);
    }

    #[test]
    fn validate_rejects_missing_observed_workflow() {
        let mut config = base_config();
        config.watch.insert(
            "content".into(),
            WatchGroup {
                paths: vec!["content/**/*.md".into()],
                workflow: Some("content".into()),
            },
        );
        config.workflow.insert(
            "content".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "content".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.hook.insert(
            "current_post_slug".into(),
            HookSpec {
                command: vec!["./scripts/current-post-slug.sh".into()],
                cwd: None,
                env: BTreeMap::new(),
                output: HookOutputConfig::default(),
                capture: Some(CaptureMode::Text),
                state_key: Some("current_post_slug".into()),
                observe: Some(ObservedHookSpec {
                    workflow: "publish".into(),
                    interval_ms: 1_000,
                }),
            },
        );

        let error = config.validate().expect_err("validation should fail");
        assert!(error.to_string().contains("observes missing workflow"));
    }

    #[test]
    fn event_server_defaults_to_local_ephemeral_bind() {
        let config: EventServerConfig = toml::from_str("").expect("parse event server config");

        assert_eq!(config.bind, "127.0.0.1:0");
    }

    #[test]
    fn browser_reload_server_defaults_to_local_ephemeral_bind() {
        let config: BrowserReloadServerConfig =
            toml::from_str("").expect("parse browser reload server config");

        assert_eq!(config.bind, "127.0.0.1:0");
    }

    #[test]
    fn config_detects_notify_reload_in_nested_workflow() {
        let mut config = base_config();
        config.workflow.insert(
            "child".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "parent".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "child".into(),
                }],
                triggers: vec![],
            },
        );

        assert!(config.has_browser_reload_notifications());
    }

    #[test]
    fn config_detects_notify_reload_in_triggered_workflow() {
        let mut config = base_config();
        config.workflow.insert(
            "reload".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::NotifyReload],
                triggers: vec![],
            },
        );
        config.workflow.insert(
            "css".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "css".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec!["reload".into()],
            },
        );

        assert!(config.has_browser_reload_notifications());
    }

    #[test]
    fn validate_rejects_missing_event_workflow() {
        let mut config = base_config();
        config.watch.insert(
            "content".into(),
            WatchGroup {
                paths: vec!["content/**/*.md".into()],
                workflow: Some("content".into()),
            },
        );
        config.workflow.insert(
            "content".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "content".into(),
                    style: LogStyle::Plain,
                }],
                triggers: vec![],
            },
        );
        config.event.insert(
            "browser_path".into(),
            EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: Some("^/".into()),
            },
        );

        let error = config.validate().expect_err("validation should fail");
        assert!(error.to_string().contains("references missing workflow"));
    }
}
