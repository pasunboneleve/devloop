use std::collections::{BTreeMap, VecDeque};

use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

use crate::config::{Config, LogStyle, RestartPolicy, WorkflowStep};
use crate::state::render_template_values;

#[derive(Debug, Clone)]
pub struct WorkflowMachine {
    session: Map<String, Value>,
    stack: Vec<WorkflowFrame>,
}

#[derive(Debug, Clone)]
struct WorkflowFrame {
    workflow_name: String,
    changed_files: Vec<String>,
    next_step: usize,
    pending_effects: VecDeque<WorkflowEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEffect {
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
        workflow_name: String,
        changed_files: Vec<String>,
    },
    SleepMs {
        duration_ms: u64,
    },
    PersistState {
        key: String,
        value: Value,
    },
    Log {
        message: String,
        style: LogStyle,
    },
}

#[derive(Debug, Clone)]
pub struct RuntimeMachine {
    phase: RuntimePhase,
    pending_effects: VecDeque<RuntimeEffect>,
    observed_hooks: BTreeMap<String, ObservedHookRuntimeState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimePhase {
    Initializing,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Start {
        root_display: String,
        startup_workflows: Vec<String>,
    },
    WatchChanges {
        workflows: BTreeMap<String, Vec<String>>,
    },
    MaintainTick {
        now_ms: u64,
    },
    ObservedHookChanged {
        workflow_name: String,
    },
    CtrlC,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    PersistState {
        key: String,
        value: Value,
    },
    StartAutostartProcesses,
    RunWorkflow {
        workflow_name: String,
        changed_files: Vec<String>,
    },
    StartWatching,
    MaintainProcesses,
    PollObservedHook {
        hook: String,
        workflow_name: String,
    },
    LogInfo {
        message: String,
    },
    StopAllProcesses,
    Exit,
}

#[derive(Debug, Clone)]
struct ObservedHookRuntimeState {
    workflow_name: String,
    interval_ms: u64,
    last_polled_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProcessSupervisor {
    processes: BTreeMap<String, ProcessRuntimeState>,
    shutting_down: bool,
}

#[derive(Debug, Clone)]
struct ProcessRuntimeState {
    running: bool,
    last_liveness_check_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEffect {
    StartProcess { process: String },
    RestartProcess { process: String },
    StopProcess { process: String },
    CheckLiveness { process: String },
}

impl WorkflowMachine {
    pub fn start(
        config: &Config,
        session: Map<String, Value>,
        workflow_name: &str,
        changed_files: &[String],
    ) -> Result<Self> {
        if !config.workflow.contains_key(workflow_name) {
            return Err(anyhow!("unknown workflow '{workflow_name}'"));
        }

        Ok(Self {
            session,
            stack: vec![WorkflowFrame::new(
                workflow_name.to_owned(),
                changed_files.to_vec(),
                true,
            )],
        })
    }

    pub fn replace_session(&mut self, session: Map<String, Value>) {
        self.session = session;
    }

    #[cfg(test)]
    pub fn session(&self) -> &Map<String, Value> {
        &self.session
    }

    pub fn next_effect(&mut self, config: &Config) -> Result<Option<WorkflowEffect>> {
        loop {
            if self.stack.is_empty() {
                return Ok(None);
            }

            if let Some(effect) = self
                .stack
                .last_mut()
                .expect("checked stack non-empty")
                .pending_effects
                .pop_front()
            {
                return Ok(Some(effect));
            }

            let (workflow_name, changed_files, next_step) = {
                let frame = self.stack.last().expect("checked stack non-empty");
                (
                    frame.workflow_name.clone(),
                    frame.changed_files.clone(),
                    frame.next_step,
                )
            };

            let workflow = config
                .workflow
                .get(&workflow_name)
                .ok_or_else(|| anyhow!("unknown workflow '{}'", workflow_name))?;

            if next_step >= workflow.steps.len() {
                self.stack.pop();
                continue;
            }

            let step = workflow.steps[next_step].clone();
            if let Some(frame) = self.stack.last_mut() {
                frame.next_step += 1;
            }

            match step {
                WorkflowStep::StartProcess { process } => {
                    return Ok(Some(WorkflowEffect::StartProcess { process }));
                }
                WorkflowStep::StopProcess { process } => {
                    return Ok(Some(WorkflowEffect::StopProcess { process }));
                }
                WorkflowStep::RestartProcess { process } => {
                    return Ok(Some(WorkflowEffect::RestartProcess { process }));
                }
                WorkflowStep::WaitForProcess { process } => {
                    return Ok(Some(WorkflowEffect::WaitForProcess { process }));
                }
                WorkflowStep::RunHook { hook } => {
                    return Ok(Some(WorkflowEffect::RunHook {
                        hook,
                        workflow_name,
                        changed_files,
                    }));
                }
                WorkflowStep::RunWorkflow { workflow } => {
                    if self
                        .stack
                        .iter()
                        .any(|frame| frame.workflow_name == workflow)
                    {
                        let mut cycle = self
                            .stack
                            .iter()
                            .map(|frame| frame.workflow_name.clone())
                            .collect::<Vec<_>>();
                        cycle.push(workflow);
                        return Err(anyhow!(
                            "workflow recursion detected at runtime: {}",
                            cycle.join(" -> ")
                        ));
                    }
                    self.stack
                        .push(WorkflowFrame::new(workflow, changed_files, false));
                }
                WorkflowStep::SleepMs { duration_ms } => {
                    return Ok(Some(WorkflowEffect::SleepMs { duration_ms }));
                }
                WorkflowStep::WriteState { key, value } => {
                    let rendered = render_template_values(&self.session, &value)?;
                    let json = Value::String(rendered.clone());
                    self.session.insert(key.clone(), json.clone());
                    return Ok(Some(WorkflowEffect::PersistState { key, value: json }));
                }
                WorkflowStep::Log { message, style } => {
                    let rendered = render_template_values(&self.session, &message)?;
                    return Ok(Some(WorkflowEffect::Log {
                        message: rendered,
                        style,
                    }));
                }
            }
        }
    }
}

impl RuntimeMachine {
    pub fn new(config: &Config) -> Self {
        let observed_hooks = config
            .hook
            .iter()
            .filter_map(|(name, spec)| {
                spec.observe.as_ref().map(|observe| {
                    (
                        name.clone(),
                        ObservedHookRuntimeState {
                            workflow_name: observe.workflow.clone(),
                            interval_ms: observe.interval_ms,
                            last_polled_ms: None,
                        },
                    )
                })
            })
            .collect();
        Self {
            phase: RuntimePhase::Initializing,
            pending_effects: VecDeque::new(),
            observed_hooks,
        }
    }

    pub fn handle_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Start {
                root_display,
                startup_workflows,
            } => {
                if self.phase != RuntimePhase::Initializing {
                    return;
                }
                self.pending_effects.push_back(RuntimeEffect::PersistState {
                    key: "root".into(),
                    value: Value::String(root_display),
                });
                self.pending_effects
                    .push_back(RuntimeEffect::StartAutostartProcesses);
                for workflow_name in startup_workflows {
                    self.pending_effects.push_back(RuntimeEffect::RunWorkflow {
                        workflow_name,
                        changed_files: Vec::new(),
                    });
                }
                self.pending_effects.push_back(RuntimeEffect::StartWatching);
                self.phase = RuntimePhase::Running;
            }
            RuntimeEvent::WatchChanges { workflows } => {
                if self.phase != RuntimePhase::Running {
                    return;
                }
                for (workflow_name, changed_files) in workflows {
                    self.pending_effects.push_back(RuntimeEffect::RunWorkflow {
                        workflow_name,
                        changed_files,
                    });
                }
            }
            RuntimeEvent::MaintainTick { now_ms } => {
                if self.phase == RuntimePhase::Running {
                    self.pending_effects
                        .push_back(RuntimeEffect::MaintainProcesses);
                    for (hook, observed) in &mut self.observed_hooks {
                        if observed
                            .last_polled_ms
                            .is_none_or(|last| now_ms.saturating_sub(last) >= observed.interval_ms)
                        {
                            observed.last_polled_ms = Some(now_ms);
                            self.pending_effects
                                .push_back(RuntimeEffect::PollObservedHook {
                                    hook: hook.clone(),
                                    workflow_name: observed.workflow_name.clone(),
                                });
                        }
                    }
                }
            }
            RuntimeEvent::ObservedHookChanged { workflow_name } => {
                if self.phase != RuntimePhase::Running {
                    return;
                }
                self.pending_effects.push_front(RuntimeEffect::RunWorkflow {
                    workflow_name,
                    changed_files: Vec::new(),
                });
            }
            RuntimeEvent::CtrlC => {
                if self.phase == RuntimePhase::Stopped {
                    return;
                }
                self.pending_effects.push_back(RuntimeEffect::LogInfo {
                    message: "received ctrl-c, shutting down".into(),
                });
                self.pending_effects
                    .push_back(RuntimeEffect::StopAllProcesses);
                self.pending_effects.push_back(RuntimeEffect::Exit);
                self.phase = RuntimePhase::Stopped;
            }
        }
    }

    pub fn next_effect(&mut self) -> Option<RuntimeEffect> {
        self.pending_effects.pop_front()
    }
}

impl ProcessSupervisor {
    pub fn new(config: &Config) -> Self {
        let processes = config
            .process
            .keys()
            .cloned()
            .map(|name| {
                (
                    name,
                    ProcessRuntimeState {
                        running: false,
                        last_liveness_check_ms: None,
                    },
                )
            })
            .collect();
        Self {
            processes,
            shutting_down: false,
        }
    }

    pub fn autostart_effects(&self, config: &Config) -> Vec<ProcessEffect> {
        config
            .process
            .iter()
            .filter(|(_, spec)| spec.autostart)
            .map(|(name, _)| ProcessEffect::StartProcess {
                process: name.clone(),
            })
            .collect()
    }

    pub fn on_process_started(&mut self, process: &str) {
        if let Some(state) = self.processes.get_mut(process) {
            state.running = true;
            state.last_liveness_check_ms = None;
        }
    }

    pub fn on_process_stopped(&mut self, process: &str) {
        if let Some(state) = self.processes.get_mut(process) {
            state.running = false;
            state.last_liveness_check_ms = None;
        }
    }

    pub fn on_shutdown(&mut self) -> Vec<ProcessEffect> {
        self.shutting_down = true;
        self.processes
            .iter()
            .filter(|(_, state)| state.running)
            .map(|(name, _)| ProcessEffect::StopProcess {
                process: name.clone(),
            })
            .collect()
    }

    pub fn on_tick(
        &mut self,
        config: &Config,
        now_ms: u64,
        exits: Vec<(String, bool)>,
    ) -> Vec<ProcessEffect> {
        let mut effects = Vec::new();

        for (process, success) in exits {
            self.on_process_stopped(&process);
            if let Some(spec) = config.process.get(&process)
                && should_restart(spec.restart, success, self.shutting_down)
            {
                effects.push(ProcessEffect::StartProcess { process });
            }
        }

        if self.shutting_down {
            return effects;
        }

        for (name, spec) in &config.process {
            let Some(liveness) = &spec.liveness else {
                continue;
            };
            let Some(state) = self.processes.get(name) else {
                continue;
            };
            if !state.running {
                continue;
            }
            if state
                .last_liveness_check_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= liveness.interval())
            {
                effects.push(ProcessEffect::CheckLiveness {
                    process: name.clone(),
                });
            }
        }

        effects
    }

    pub fn on_liveness_result(
        &mut self,
        config: &Config,
        process: &str,
        healthy: bool,
        now_ms: u64,
    ) -> Vec<ProcessEffect> {
        let Some(state) = self.processes.get_mut(process) else {
            return Vec::new();
        };
        state.last_liveness_check_ms = Some(now_ms);

        if healthy || self.shutting_down {
            return Vec::new();
        }

        let Some(spec) = config.process.get(process) else {
            return Vec::new();
        };
        if spec.restart == RestartPolicy::Never {
            return Vec::new();
        }

        vec![ProcessEffect::RestartProcess {
            process: process.to_owned(),
        }]
    }
}

impl WorkflowFrame {
    fn new(workflow_name: String, changed_files: Vec<String>, record_change_context: bool) -> Self {
        let mut pending_effects = VecDeque::new();
        if record_change_context {
            pending_effects.push_back(WorkflowEffect::PersistState {
                key: "last_workflow".into(),
                value: Value::String(workflow_name.clone()),
            });
            pending_effects.push_back(WorkflowEffect::PersistState {
                key: "last_changed_files".into(),
                value: Value::Array(changed_files.iter().cloned().map(Value::String).collect()),
            });
        }

        Self {
            workflow_name,
            changed_files,
            next_step: 0,
            pending_effects,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LogStyle, ProbeSpec, ProcessSpec, RestartPolicy, WorkflowSpec};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn base_config() -> Config {
        Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            workflow: BTreeMap::new(),
        }
    }

    #[test]
    fn machine_emits_change_context_then_steps() {
        let mut config = base_config();
        config.workflow.insert(
            "content".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RestartProcess {
                    process: "server".into(),
                }],
            },
        );

        let mut machine =
            WorkflowMachine::start(&config, Map::new(), "content", &["content/post.md".into()])
                .expect("start machine");

        assert_eq!(
            machine.next_effect(&config).expect("effect"),
            Some(WorkflowEffect::PersistState {
                key: "last_workflow".into(),
                value: Value::String("content".into())
            })
        );
        assert_eq!(
            machine.next_effect(&config).expect("effect"),
            Some(WorkflowEffect::PersistState {
                key: "last_changed_files".into(),
                value: Value::Array(vec![Value::String("content/post.md".into())])
            })
        );
        assert_eq!(
            machine.next_effect(&config).expect("effect"),
            Some(WorkflowEffect::RestartProcess {
                process: "server".into()
            })
        );
        assert_eq!(machine.next_effect(&config).expect("effect"), None);
    }

    #[test]
    fn machine_renders_write_state_from_session_snapshot() {
        let mut config = base_config();
        config.workflow.insert(
            "publish".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::WriteState {
                    key: "current_post_url".into(),
                    value: "{{tunnel_url}}/posts/{{current_post_slug}}".into(),
                }],
            },
        );

        let mut session = Map::new();
        session.insert(
            "tunnel_url".into(),
            Value::String("https://example.trycloudflare.com".into()),
        );
        session.insert(
            "current_post_slug".into(),
            Value::String("example-post".into()),
        );

        let mut machine =
            WorkflowMachine::start(&config, session, "publish", &[]).expect("start machine");
        let _ = machine.next_effect(&config).expect("effect");
        let _ = machine.next_effect(&config).expect("effect");

        assert_eq!(
            machine.next_effect(&config).expect("effect"),
            Some(WorkflowEffect::PersistState {
                key: "current_post_url".into(),
                value: Value::String("https://example.trycloudflare.com/posts/example-post".into())
            })
        );
        assert_eq!(
            machine
                .session()
                .get("current_post_url")
                .expect("current_post_url"),
            "https://example.trycloudflare.com/posts/example-post"
        );
    }

    #[test]
    fn machine_expands_nested_workflows_without_rewriting_top_level_context() {
        let mut config = base_config();
        config.workflow.insert(
            "helper".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "hello {{name}}".into(),
                    style: LogStyle::Plain,
                }],
            },
        );
        config.workflow.insert(
            "startup".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "helper".into(),
                }],
            },
        );

        let mut session = Map::new();
        session.insert("name".into(), Value::String("world".into()));
        let mut machine =
            WorkflowMachine::start(&config, session, "startup", &[]).expect("start machine");

        let _ = machine.next_effect(&config).expect("effect");
        let _ = machine.next_effect(&config).expect("effect");

        assert_eq!(
            machine.next_effect(&config).expect("effect"),
            Some(WorkflowEffect::Log {
                message: "hello world".into(),
                style: LogStyle::Plain
            })
        );
        assert_eq!(machine.next_effect(&config).expect("effect"), None);
    }

    #[test]
    fn machine_rejects_runtime_recursive_workflows() {
        let mut config = base_config();
        config.workflow.insert(
            "loop".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "loop".into(),
                }],
            },
        );

        let mut machine =
            WorkflowMachine::start(&config, Map::new(), "loop", &[]).expect("start machine");

        let _ = machine.next_effect(&config).expect("change context");
        let _ = machine.next_effect(&config).expect("change context");
        let error = machine
            .next_effect(&config)
            .expect_err("recursive workflow should fail");

        assert!(
            error
                .to_string()
                .contains("workflow recursion detected at runtime")
        );
    }

    #[test]
    fn runtime_machine_plans_startup_sequence() {
        let config = base_config();
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec!["startup".into(), "publish".into()],
        });

        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::PersistState {
                key: "root".into(),
                value: Value::String("/tmp/example".into())
            })
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::StartAutostartProcesses)
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::RunWorkflow {
                workflow_name: "startup".into(),
                changed_files: Vec::new()
            })
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::RunWorkflow {
                workflow_name: "publish".into(),
                changed_files: Vec::new()
            })
        );
        assert_eq!(runtime.next_effect(), Some(RuntimeEffect::StartWatching));
        assert_eq!(runtime.next_effect(), None);
    }

    #[test]
    fn runtime_machine_plans_watch_tick_and_shutdown() {
        let config = base_config();
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec![],
        });
        while runtime.next_effect().is_some() {}

        let mut workflows = BTreeMap::new();
        workflows.insert("rust".into(), vec!["src/main.rs".into()]);
        runtime.handle_event(RuntimeEvent::WatchChanges { workflows });
        runtime.handle_event(RuntimeEvent::MaintainTick { now_ms: 1_000 });
        runtime.handle_event(RuntimeEvent::CtrlC);

        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::RunWorkflow {
                workflow_name: "rust".into(),
                changed_files: vec!["src/main.rs".into()]
            })
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::MaintainProcesses)
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::LogInfo {
                message: "received ctrl-c, shutting down".into()
            })
        );
        assert_eq!(runtime.next_effect(), Some(RuntimeEffect::StopAllProcesses));
        assert_eq!(runtime.next_effect(), Some(RuntimeEffect::Exit));
        assert_eq!(runtime.next_effect(), None);
    }

    #[test]
    fn runtime_machine_polls_observed_hooks_and_runs_workflow_on_change() {
        let mut config = base_config();
        config.hook.insert(
            "current_post_slug".into(),
            crate::config::HookSpec {
                command: vec!["./scripts/current-post-slug.sh".into()],
                cwd: None,
                env: BTreeMap::new(),
                output: crate::config::HookOutputConfig::default(),
                capture: Some(crate::config::CaptureMode::Text),
                state_key: Some("current_post_slug".into()),
                observe: Some(crate::config::ObservedHookSpec {
                    workflow: "publish_post_url".into(),
                    interval_ms: 500,
                }),
            },
        );
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "announce".into(),
                    style: LogStyle::Plain,
                }],
            },
        );

        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec![],
        });
        while runtime.next_effect().is_some() {}

        runtime.handle_event(RuntimeEvent::MaintainTick { now_ms: 500 });
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::MaintainProcesses)
        );
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::PollObservedHook {
                hook: "current_post_slug".into(),
                workflow_name: "publish_post_url".into(),
            })
        );
        assert_eq!(runtime.next_effect(), None);

        runtime.handle_event(RuntimeEvent::ObservedHookChanged {
            workflow_name: "publish_post_url".into(),
        });
        assert_eq!(
            runtime.next_effect(),
            Some(RuntimeEffect::RunWorkflow {
                workflow_name: "publish_post_url".into(),
                changed_files: Vec::new(),
            })
        );
        assert_eq!(runtime.next_effect(), None);
    }

    #[test]
    fn process_supervisor_plans_autostart_restart_and_shutdown() {
        let mut config = base_config();
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["cargo".into(), "run".into()],
                cwd: None,
                autostart: true,
                readiness: None,
                liveness: None,
                restart: RestartPolicy::Always,
                env: BTreeMap::new(),
                output: Default::default(),
            },
        );

        let mut supervisor = ProcessSupervisor::new(&config);
        assert_eq!(
            supervisor.autostart_effects(&config),
            vec![ProcessEffect::StartProcess {
                process: "server".into()
            }]
        );

        supervisor.on_process_started("server");
        assert_eq!(
            supervisor.on_tick(&config, 1000, vec![("server".into(), false)]),
            vec![ProcessEffect::StartProcess {
                process: "server".into()
            }]
        );

        supervisor.on_process_started("server");
        assert_eq!(
            supervisor.on_shutdown(),
            vec![ProcessEffect::StopProcess {
                process: "server".into()
            }]
        );
    }

    #[test]
    fn process_supervisor_schedules_liveness_checks_and_restart_on_failure() {
        let mut config = base_config();
        config.process.insert(
            "tunnel".into(),
            ProcessSpec {
                command: vec!["cloudflared".into(), "tunnel".into()],
                cwd: None,
                autostart: false,
                readiness: None,
                liveness: Some(ProbeSpec::StateKey {
                    key: "tunnel_url".into(),
                    interval_ms: 250,
                    timeout_ms: 1000,
                }),
                restart: RestartPolicy::Always,
                env: BTreeMap::new(),
                output: Default::default(),
            },
        );

        let mut supervisor = ProcessSupervisor::new(&config);
        supervisor.on_process_started("tunnel");

        assert_eq!(
            supervisor.on_tick(&config, 250, Vec::new()),
            vec![ProcessEffect::CheckLiveness {
                process: "tunnel".into()
            }]
        );
        assert_eq!(
            supervisor.on_liveness_result(&config, "tunnel", false, 250),
            vec![ProcessEffect::RestartProcess {
                process: "tunnel".into()
            }]
        );
    }
}
