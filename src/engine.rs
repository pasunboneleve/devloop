use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::{Map, Value};
use tokio::signal;
use tokio::time::{Instant, sleep};
use tracing::{error, info};
use unicode_width::UnicodeWidthStr;

use crate::browser_reload::{BrowserReloadSender, BrowserReloadServer, notify_browser_reload};
use crate::config::{CompiledWatchGroup, Config, LogStyle};
use crate::core::{RuntimeEffect, RuntimeEvent, RuntimeMachine, WorkflowEffect, WorkflowMachine};
use crate::external_events::{ExternalEventMessage, ExternalEventServer};
use crate::processes::ProcessManager;
use crate::state::SessionState;

pub struct Engine {
    config: Config,
}

trait WorkflowEffectAdapter {
    async fn start_process(&mut self, process: &str) -> Result<()>;
    async fn stop_process(&mut self, process: &str) -> Result<()>;
    async fn restart_process(&mut self, process: &str) -> Result<()>;
    async fn wait_for_process(&mut self, process: &str) -> Result<()>;
    async fn run_hook(
        &mut self,
        hook: &str,
        changed_files: &[String],
        workflow_name: &str,
    ) -> Result<()>;
    async fn notify_reload(&mut self) -> Result<()>;
    async fn sleep_ms(&mut self, duration_ms: u64) -> Result<()>;
    async fn persist_state(&mut self, key: String, value: Value) -> Result<()>;
    async fn log_message(&mut self, style: LogStyle, message: String) -> Result<()>;
    fn snapshot_state(&self) -> Result<Map<String, Value>>;
}

trait RuntimeEffectAdapter {
    async fn persist_state(&mut self, key: String, value: Value) -> Result<()>;
    async fn start_external_event_server(&mut self) -> Result<()>;
    async fn start_browser_reload_server(&mut self) -> Result<()>;
    async fn start_autostart_processes(&mut self) -> Result<()>;
    async fn run_workflow(&mut self, workflow_name: &str, changed_files: &[String]) -> Result<()>;
    async fn start_watching(&mut self) -> Result<()>;
    async fn maintain_processes(&mut self) -> Result<()>;
    async fn poll_observed_hook(&mut self, hook: &str) -> Result<bool>;
    async fn log_info(&mut self, message: String) -> Result<()>;
    async fn stop_watching(&mut self) -> Result<()>;
    async fn stop_all_processes(&mut self) -> Result<()>;
}

struct LiveWorkflowAdapter<'a, 'b> {
    processes: &'a mut ProcessManager<'b>,
    state: &'a SessionState,
    browser_reload_sender: Option<BrowserReloadSender>,
}

struct LiveRuntimeAdapter<'a, 'b> {
    config: &'a Config,
    processes: &'a mut ProcessManager<'b>,
    state: &'a SessionState,
    watcher: &'a mut RecommendedWatcher,
    watcher_shutdown: Arc<AtomicBool>,
    external_event_tx: tokio::sync::mpsc::UnboundedSender<ExternalEventMessage>,
    external_event_server: Option<ExternalEventServer>,
    browser_reload_server: Option<BrowserReloadServer>,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let state = SessionState::load(
            self.config
                .state_file
                .clone()
                .ok_or_else(|| anyhow!("state file missing after config load"))?,
        )?;
        let mut processes = ProcessManager::new(&self.config);
        let watch_groups = self.config.compiled_watchers()?;
        let (tx, rx) = mpsc::channel();
        let (external_event_tx, mut external_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let tx_watcher = tx.clone();
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        let watcher_shutdown_callback = watcher_shutdown.clone();
        let mut watcher = RecommendedWatcher::new(
            move |result| {
                forward_watcher_event(&tx_watcher, &watcher_shutdown_callback, result);
            },
            NotifyConfig::default(),
        )?;
        let mut maintain_tick = tokio::time::interval(Duration::from_secs(1));
        let mut runtime = RuntimeMachine::new(&self.config);
        let runtime_start = Instant::now();
        runtime.handle_event(RuntimeEvent::Start {
            root_display: self.config.root.display().to_string(),
            startup_workflows: self.config.startup_workflows.clone(),
        });
        let mut adapter = LiveRuntimeAdapter {
            config: &self.config,
            processes: &mut processes,
            state: &state,
            watcher: &mut watcher,
            watcher_shutdown,
            external_event_tx,
            external_event_server: None,
            browser_reload_server: None,
        };
        execute_runtime_effects(&mut runtime, &mut adapter).await?;

        loop {
            tokio::select! {
                biased;
                result = signal::ctrl_c() => {
                    result?;
                    runtime.handle_event(RuntimeEvent::CtrlC);
                    if execute_runtime_effects(&mut runtime, &mut adapter).await? {
                        return Ok(());
                    }
                }
                _ = maintain_tick.tick() => {
                    runtime.handle_event(RuntimeEvent::MaintainTick {
                        now_ms: runtime_start.elapsed().as_millis() as u64,
                    });
                    if execute_runtime_effects(&mut runtime, &mut adapter).await? {
                        return Ok(());
                    }
                }
                batch = next_batch(&rx, self.config.debounce()) => {
                    let events = batch?;
                    let workflows = classify_events(&self.config.root, &watch_groups, &events);
                    if !workflows.is_empty() {
                        runtime.handle_event(RuntimeEvent::WatchChanges { workflows });
                        if execute_runtime_effects(&mut runtime, &mut adapter).await? {
                            return Ok(());
                        }
                    }
                }
                event = external_event_rx.recv() => {
                    match event {
                        Some(event) => {
                            runtime.handle_event(RuntimeEvent::WorkflowTrigger {
                                workflow_name: event.workflow_name,
                            });
                            if execute_runtime_effects(&mut runtime, &mut adapter).await? {
                                return Ok(());
                            }
                        }
                        None => return Err(anyhow!("external event channel disconnected")),
                    }
                }
            }
        }
    }
}

impl WorkflowEffectAdapter for LiveWorkflowAdapter<'_, '_> {
    async fn start_process(&mut self, process: &str) -> Result<()> {
        self.processes.start_named(process, self.state).await
    }

    async fn stop_process(&mut self, process: &str) -> Result<()> {
        self.processes.stop_named(process).await
    }

    async fn restart_process(&mut self, process: &str) -> Result<()> {
        self.processes.restart_named(process, self.state).await
    }

    async fn wait_for_process(&mut self, process: &str) -> Result<()> {
        self.processes.wait_for_named(process, self.state).await
    }

    async fn run_hook(
        &mut self,
        hook: &str,
        changed_files: &[String],
        workflow_name: &str,
    ) -> Result<()> {
        self.processes
            .run_hook(hook, self.state, changed_files, workflow_name)
            .await
    }

    async fn notify_reload(&mut self) -> Result<()> {
        if let Some(sender) = &self.browser_reload_sender {
            notify_browser_reload(sender);
        }
        Ok(())
    }

    async fn sleep_ms(&mut self, duration_ms: u64) -> Result<()> {
        sleep(Duration::from_millis(duration_ms)).await;
        Ok(())
    }

    async fn persist_state(&mut self, key: String, value: Value) -> Result<()> {
        self.state.set(key, value)
    }

    async fn log_message(&mut self, style: LogStyle, message: String) -> Result<()> {
        log_workflow_message(&style, &message);
        Ok(())
    }

    fn snapshot_state(&self) -> Result<Map<String, Value>> {
        self.state.snapshot()
    }
}

impl RuntimeEffectAdapter for LiveRuntimeAdapter<'_, '_> {
    async fn persist_state(&mut self, key: String, value: Value) -> Result<()> {
        self.state.set(key, value)
    }

    async fn start_external_event_server(&mut self) -> Result<()> {
        if self.external_event_server.is_some() {
            return Ok(());
        }
        if let Some(server) = ExternalEventServer::start(
            self.config,
            self.state.clone(),
            self.external_event_tx.clone(),
        )
        .await?
        {
            self.processes
                .set_external_event_env(Some(server.environment().clone()));
            self.external_event_server = Some(server);
        }
        Ok(())
    }

    async fn start_browser_reload_server(&mut self) -> Result<()> {
        if self.browser_reload_server.is_some() {
            return Ok(());
        }
        if let Some(server) = BrowserReloadServer::start(self.config).await? {
            self.processes
                .set_browser_reload_env(Some(server.environment().clone()));
            self.browser_reload_server = Some(server);
        }
        Ok(())
    }

    async fn start_autostart_processes(&mut self) -> Result<()> {
        self.processes.start_autostart(self.state).await
    }

    async fn run_workflow(&mut self, workflow_name: &str, changed_files: &[String]) -> Result<()> {
        info!("running workflow {}", workflow_name);
        self.config
            .workflow
            .get(workflow_name)
            .ok_or_else(|| anyhow!("runtime requested missing workflow '{workflow_name}'"))?;
        let mut adapter = LiveWorkflowAdapter {
            processes: self.processes,
            state: self.state,
            browser_reload_sender: self
                .browser_reload_server
                .as_ref()
                .map(BrowserReloadServer::sender),
        };
        run_workflow_machine(self.config, &mut adapter, workflow_name, changed_files).await
    }

    async fn start_watching(&mut self) -> Result<()> {
        self.watcher
            .watch(&self.config.root, RecursiveMode::Recursive)?;
        info!("watching {}", self.config.root.display());
        Ok(())
    }

    async fn maintain_processes(&mut self) -> Result<()> {
        self.processes.maintain(self.state).await
    }

    async fn poll_observed_hook(&mut self, hook: &str) -> Result<bool> {
        self.processes
            .run_observed_hook(hook, self.state, &[], "observe")
            .await
    }

    async fn log_info(&mut self, message: String) -> Result<()> {
        info!("{}", message);
        Ok(())
    }

    async fn stop_watching(&mut self) -> Result<()> {
        self.watcher_shutdown.store(true, Ordering::Relaxed);
        self.watcher.unwatch(&self.config.root)?;
        Ok(())
    }

    async fn stop_all_processes(&mut self) -> Result<()> {
        self.processes.stop_all(self.state).await
    }
}

async fn execute_runtime_effects<A: RuntimeEffectAdapter>(
    runtime: &mut RuntimeMachine,
    adapter: &mut A,
) -> Result<bool> {
    while let Some(effect) = runtime.next_effect() {
        match effect {
            RuntimeEffect::PersistState { key, value } => adapter.persist_state(key, value).await?,
            RuntimeEffect::StartExternalEventServer => {
                adapter.start_external_event_server().await?
            }
            RuntimeEffect::StartBrowserReloadServer => {
                adapter.start_browser_reload_server().await?
            }
            RuntimeEffect::StartAutostartProcesses => adapter.start_autostart_processes().await?,
            RuntimeEffect::RunWorkflow {
                workflow_name,
                changed_files,
            } => {
                if let Err(error) = adapter.run_workflow(&workflow_name, &changed_files).await {
                    error!(
                        workflow = %workflow_name,
                        error = %error,
                        "workflow failed; continuing runtime"
                    );
                }
            }
            RuntimeEffect::StartWatching => adapter.start_watching().await?,
            RuntimeEffect::MaintainProcesses => adapter.maintain_processes().await?,
            RuntimeEffect::PollObservedHook {
                hook,
                workflow_name,
            } => {
                if adapter.poll_observed_hook(&hook).await? {
                    runtime.handle_event(RuntimeEvent::WorkflowTrigger { workflow_name });
                }
            }
            RuntimeEffect::LogInfo { message } => adapter.log_info(message).await?,
            RuntimeEffect::StopWatching => adapter.stop_watching().await?,
            RuntimeEffect::StopAllProcesses => adapter.stop_all_processes().await?,
            RuntimeEffect::Exit => return Ok(true),
        }
    }

    Ok(false)
}

fn forward_watcher_event(
    tx: &mpsc::Sender<notify::Result<Event>>,
    shutting_down: &AtomicBool,
    result: notify::Result<Event>,
) {
    if let Err(error) = tx.send(result)
        && !shutting_down.load(Ordering::Relaxed)
    {
        error!(
            "failed to forward watcher event into runtime loop: {}",
            error
        );
    }
}

#[cfg(test)]
async fn run_workflow(
    config: &Config,
    processes: &mut ProcessManager<'_>,
    state: &SessionState,
    browser_reload_sender: Option<BrowserReloadSender>,
    workflow_name: &str,
    changed_files: &[String],
) -> Result<()> {
    info!("running workflow {}", workflow_name);
    config
        .workflow
        .get(workflow_name)
        .ok_or_else(|| anyhow!("runtime requested missing workflow '{workflow_name}'"))?;
    let mut adapter = LiveWorkflowAdapter {
        processes,
        state,
        browser_reload_sender,
    };
    run_workflow_machine(config, &mut adapter, workflow_name, changed_files).await
}

async fn run_workflow_machine<A: WorkflowEffectAdapter>(
    config: &Config,
    adapter: &mut A,
    workflow_name: &str,
    changed_files: &[String],
) -> Result<()> {
    let mut machine = WorkflowMachine::start(
        config,
        adapter.snapshot_state()?,
        workflow_name,
        changed_files,
    )?;
    while let Some(effect) = machine.next_effect(config)? {
        execute_workflow_effect(effect, adapter).await?;
        machine.replace_session(adapter.snapshot_state()?);
    }

    Ok(())
}

async fn execute_workflow_effect<A: WorkflowEffectAdapter>(
    effect: WorkflowEffect,
    adapter: &mut A,
) -> Result<()> {
    match effect {
        WorkflowEffect::StartProcess { process } => adapter.start_process(&process).await,
        WorkflowEffect::StopProcess { process } => adapter.stop_process(&process).await,
        WorkflowEffect::RestartProcess { process } => adapter.restart_process(&process).await,
        WorkflowEffect::WaitForProcess { process } => adapter.wait_for_process(&process).await,
        WorkflowEffect::RunHook {
            hook,
            workflow_name,
            changed_files,
        } => {
            adapter
                .run_hook(&hook, &changed_files, &workflow_name)
                .await
        }
        WorkflowEffect::NotifyReload => adapter.notify_reload().await,
        WorkflowEffect::SleepMs { duration_ms } => adapter.sleep_ms(duration_ms).await,
        WorkflowEffect::PersistState { key, value } => adapter.persist_state(key, value).await,
        WorkflowEffect::Log { message, style } => adapter.log_message(style, message).await,
    }
}

fn log_workflow_message(style: &LogStyle, message: &str) {
    match style {
        LogStyle::Plain => info!("{}", message),
        LogStyle::Boxed => log_boxed_banner(message),
    }
}

fn log_boxed_banner(message: &str) {
    for line in boxed_banner_lines(message) {
        info!("{}", line);
    }
}

fn boxed_banner_lines(message: &str) -> [String; 3] {
    let width = UnicodeWidthStr::width(message) + 4;
    let border = format!("+{}+", "-".repeat(width));
    let line = format!("|  {}  |", message);
    [border.clone(), line, border]
}

async fn next_batch(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce: Duration,
) -> Result<Vec<Event>> {
    let first = match rx.recv() {
        Ok(result) => result?,
        Err(_) => return Err(anyhow!("watcher event channel disconnected")),
    };
    let start = Instant::now();
    let mut events = vec![first];
    while start.elapsed() < debounce {
        match rx.try_recv() {
            Ok(result) => events.push(result?),
            Err(mpsc::TryRecvError::Empty) => sleep(Duration::from_millis(25)).await,
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    Ok(events)
}

fn classify_events(
    root: &Path,
    watch_groups: &[CompiledWatchGroup],
    events: &[Event],
) -> BTreeMap<String, Vec<String>> {
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for event in events {
        if !is_relevant_event(&event.kind) {
            continue;
        }
        for path in &event.paths {
            let Some(relative) = relativize_event_path(root, path) else {
                continue;
            };
            for group in watch_groups {
                if group.matches(relative) {
                    grouped
                        .entry(group.workflow.clone())
                        .or_default()
                        .insert(normalize_path(relative));
                }
            }
        }
    }
    grouped
        .into_iter()
        .map(|(workflow, files)| (workflow, files.into_iter().collect()))
        .collect()
}

fn relativize_event_path<'a>(root: &'a Path, path: &'a Path) -> Option<&'a Path> {
    path.strip_prefix(root)
        .ok()
        .or_else(|| strip_private_prefix_variant(root, path))
}

fn strip_private_prefix_variant<'a>(root: &'a Path, path: &'a Path) -> Option<&'a Path> {
    let private_root = Path::new("/private").join(root.strip_prefix("/").ok()?);
    path.strip_prefix(&private_root).ok()
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorkflowSpec, WorkflowStep};
    use notify::{Event, EventKind, event::ModifyKind};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_state_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("devloop-engine-state-{unique}.json"))
    }

    #[test]
    fn classify_changes_by_workflow() {
        let root = PathBuf::from("/tmp/example");
        let groups = vec![
            CompiledWatchGroup::for_test(&["src/**/*.rs"], "server").expect("watch group"),
            CompiledWatchGroup::for_test(&["content/**/*.md"], "content").expect("watch group"),
        ];
        let events = vec![
            Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![root.join("src/main.rs")],
                attrs: Default::default(),
            },
            Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![root.join("content/posts/example.md")],
                attrs: Default::default(),
            },
        ];
        let grouped = classify_events(&root, &groups, &events);
        assert_eq!(grouped["server"], vec!["src/main.rs"]);
        assert_eq!(grouped["content"], vec!["content/posts/example.md"]);
    }

    #[tokio::test]
    async fn next_batch_errors_when_watcher_channel_disconnects() {
        let (_tx, rx) = mpsc::channel();
        drop(_tx);

        let error = next_batch(&rx, Duration::from_millis(10))
            .await
            .expect_err("channel disconnect should error");

        assert!(
            error
                .to_string()
                .contains("watcher event channel disconnected")
        );
    }

    #[test]
    fn classify_changes_by_workflow_accepts_private_var_event_paths() {
        let root = PathBuf::from("/var/folders/example/tmp");
        let groups =
            vec![CompiledWatchGroup::for_test(&["watched.txt"], "content").expect("watch group")];
        let events = vec![Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![PathBuf::from(
                "/private/var/folders/example/tmp/watched.txt",
            )],
            attrs: Default::default(),
        }];

        let grouped = classify_events(&root, &groups, &events);

        assert_eq!(grouped["content"], vec!["watched.txt"]);
    }

    #[tokio::test]
    async fn write_state_step_renders_session_template() {
        let state_path = unique_state_path();
        let root = state_path.parent().expect("state parent").to_path_buf();
        let state = SessionState::load(state_path.clone()).expect("load state");
        state
            .set(
                "tunnel_url",
                Value::String("https://example.trycloudflare.com".into()),
            )
            .expect("set tunnel url");
        state
            .set("current_post_slug", Value::String("example-post".into()))
            .expect("set slug");

        let mut config = Config {
            root: root.clone(),
            debounce_ms: 100,
            state_file: Some(state_path.clone()),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        config.workflow.insert(
            "compose".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::WriteState {
                    key: "current_post_url".into(),
                    value: "{{tunnel_url}}/posts/{{current_post_slug}}".into(),
                }],
            },
        );

        let mut processes = ProcessManager::new(&config);
        run_workflow(&config, &mut processes, &state, None, "compose", &[])
            .await
            .expect("run workflow");

        assert_eq!(
            state
                .get_string("current_post_url")
                .expect("get current_post_url")
                .as_deref(),
            Some("https://example.trycloudflare.com/posts/example-post")
        );

        std::fs::remove_file(state_path).expect("cleanup state file");
    }

    #[tokio::test]
    async fn nested_workflow_runs_helper_steps() {
        let state_path = unique_state_path();
        let root = state_path.parent().expect("state parent").to_path_buf();
        let state = SessionState::load(state_path.clone()).expect("load state");
        state
            .set(
                "tunnel_url",
                Value::String("https://example.trycloudflare.com".into()),
            )
            .expect("set tunnel url");
        state
            .set("current_post_slug", Value::String("nested-post".into()))
            .expect("set slug");

        let mut config = Config {
            root: root.clone(),
            debounce_ms: 100,
            state_file: Some(state_path.clone()),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        config.workflow.insert(
            "publish_post_url".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::WriteState {
                    key: "current_post_url".into(),
                    value: "{{tunnel_url}}/posts/{{current_post_slug}}".into(),
                }],
            },
        );
        config.workflow.insert(
            "content".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::RunWorkflow {
                    workflow: "publish_post_url".into(),
                }],
            },
        );

        let mut processes = ProcessManager::new(&config);
        run_workflow(&config, &mut processes, &state, None, "content", &[])
            .await
            .expect("run workflow");

        assert_eq!(
            state
                .get_string("current_post_url")
                .expect("get current_post_url")
                .as_deref(),
            Some("https://example.trycloudflare.com/posts/nested-post")
        );
        assert_eq!(
            state
                .get_string("last_workflow")
                .expect("get last_workflow")
                .as_deref(),
            Some("content")
        );

        std::fs::remove_file(state_path).expect("cleanup state file");
    }

    #[tokio::test]
    async fn log_step_renders_session_template() {
        let root = PathBuf::from(".");
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        state
            .set(
                "current_post_url",
                Value::String("https://example.trycloudflare.com/posts/example-post".into()),
            )
            .expect("set current_post_url");

        let mut config = Config {
            root,
            debounce_ms: 100,
            state_file: Some(state_path.clone()),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        config.workflow.insert(
            "announce".into(),
            WorkflowSpec {
                steps: vec![WorkflowStep::Log {
                    message: "current post url: {{current_post_url}}".into(),
                    style: LogStyle::Plain,
                }],
            },
        );

        let mut processes = ProcessManager::new(&config);
        run_workflow(&config, &mut processes, &state, None, "announce", &[])
            .await
            .expect("run workflow");

        std::fs::remove_file(state_path).expect("cleanup state file");
    }

    #[test]
    fn boxed_banner_lines_wrap_message() {
        let lines = boxed_banner_lines("current post url: https://example.test/posts/x");
        assert_eq!(lines[0], lines[2]);
        assert_eq!(
            lines[1],
            "|  current post url: https://example.test/posts/x  |"
        );
    }

    #[test]
    fn boxed_banner_lines_use_display_width() {
        let lines = boxed_banner_lines("URL 測試");
        assert_eq!(lines[0], lines[2]);
        assert_eq!(UnicodeWidthStr::width(lines[0].as_str()), 14);
        assert_eq!(UnicodeWidthStr::width(lines[1].as_str()), 14);
    }

    struct MockWorkflowAdapter {
        state: Map<String, Value>,
        calls: Vec<String>,
        sleeps: VecDeque<u64>,
    }

    impl MockWorkflowAdapter {
        fn new(state: Map<String, Value>) -> Self {
            Self {
                state,
                calls: Vec::new(),
                sleeps: VecDeque::new(),
            }
        }
    }

    impl WorkflowEffectAdapter for MockWorkflowAdapter {
        async fn start_process(&mut self, process: &str) -> Result<()> {
            self.calls.push(format!("start:{process}"));
            Ok(())
        }

        async fn stop_process(&mut self, process: &str) -> Result<()> {
            self.calls.push(format!("stop:{process}"));
            Ok(())
        }

        async fn restart_process(&mut self, process: &str) -> Result<()> {
            self.calls.push(format!("restart:{process}"));
            Ok(())
        }

        async fn wait_for_process(&mut self, process: &str) -> Result<()> {
            self.calls.push(format!("wait:{process}"));
            Ok(())
        }

        async fn run_hook(
            &mut self,
            hook: &str,
            changed_files: &[String],
            workflow_name: &str,
        ) -> Result<()> {
            self.calls.push(format!(
                "hook:{hook}:{workflow_name}:{}",
                changed_files.join(",")
            ));
            Ok(())
        }

        async fn notify_reload(&mut self) -> Result<()> {
            self.calls.push("notify_reload".into());
            Ok(())
        }

        async fn sleep_ms(&mut self, duration_ms: u64) -> Result<()> {
            self.sleeps.push_back(duration_ms);
            Ok(())
        }

        async fn persist_state(&mut self, key: String, value: Value) -> Result<()> {
            self.calls.push(format!("persist:{key}"));
            self.state.insert(key, value);
            Ok(())
        }

        async fn log_message(&mut self, style: LogStyle, message: String) -> Result<()> {
            self.calls.push(format!("log:{style:?}:{message}"));
            Ok(())
        }

        fn snapshot_state(&self) -> Result<Map<String, Value>> {
            Ok(self.state.clone())
        }
    }

    #[tokio::test]
    async fn workflow_machine_can_run_against_mock_adapter() {
        let mut config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        config.workflow.insert(
            "startup".into(),
            WorkflowSpec {
                steps: vec![
                    WorkflowStep::WriteState {
                        key: "url".into(),
                        value: "https://example.test/{{slug}}".into(),
                    },
                    WorkflowStep::RunHook {
                        hook: "build_css".into(),
                    },
                    WorkflowStep::Log {
                        message: "ready {{url}}".into(),
                        style: LogStyle::Plain,
                    },
                    WorkflowStep::NotifyReload,
                ],
            },
        );

        let mut state = Map::new();
        state.insert("slug".into(), Value::String("post".into()));
        let mut adapter = MockWorkflowAdapter::new(state);

        run_workflow_machine(&config, &mut adapter, "startup", &["tailwind.css".into()])
            .await
            .expect("run workflow");

        assert_eq!(
            adapter.calls,
            vec![
                "persist:last_workflow",
                "persist:last_changed_files",
                "persist:url",
                "hook:build_css:startup:tailwind.css",
                "log:Plain:ready https://example.test/post",
                "notify_reload",
            ]
        );
    }

    struct MockRuntimeAdapter {
        calls: Vec<String>,
        changed_hooks: BTreeMap<String, bool>,
        workflow_errors: BTreeMap<String, String>,
        watching: bool,
    }

    impl MockRuntimeAdapter {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                changed_hooks: BTreeMap::new(),
                workflow_errors: BTreeMap::new(),
                watching: false,
            }
        }
    }

    impl RuntimeEffectAdapter for MockRuntimeAdapter {
        async fn persist_state(&mut self, key: String, _value: Value) -> Result<()> {
            self.calls.push(format!("persist:{key}"));
            Ok(())
        }

        async fn start_external_event_server(&mut self) -> Result<()> {
            self.calls.push("event_server".into());
            Ok(())
        }

        async fn start_browser_reload_server(&mut self) -> Result<()> {
            self.calls.push("browser_reload_server".into());
            Ok(())
        }

        async fn start_autostart_processes(&mut self) -> Result<()> {
            self.calls.push("autostart".into());
            Ok(())
        }

        async fn run_workflow(
            &mut self,
            workflow_name: &str,
            changed_files: &[String],
        ) -> Result<()> {
            if let Some(message) = self.workflow_errors.get(workflow_name) {
                return Err(anyhow!(message.clone()));
            }
            self.calls.push(format!(
                "workflow:{workflow_name}:{}",
                changed_files.join(",")
            ));
            Ok(())
        }

        async fn start_watching(&mut self) -> Result<()> {
            self.calls.push("watch".into());
            self.watching = true;
            Ok(())
        }

        async fn maintain_processes(&mut self) -> Result<()> {
            self.calls.push("maintain".into());
            Ok(())
        }

        async fn poll_observed_hook(&mut self, hook: &str) -> Result<bool> {
            self.calls.push(format!("poll:{hook}"));
            Ok(self.changed_hooks.get(hook).copied().unwrap_or(false))
        }

        async fn log_info(&mut self, message: String) -> Result<()> {
            self.calls.push(format!("log:{message}"));
            Ok(())
        }

        async fn stop_watching(&mut self) -> Result<()> {
            self.calls.push("stop_watch".into());
            self.watching = false;
            Ok(())
        }

        async fn stop_all_processes(&mut self) -> Result<()> {
            self.calls.push("stop_all".into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn runtime_machine_can_run_against_mock_adapter() {
        let mut config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
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
        config.event.insert(
            "browser_path".into(),
            crate::config::EventSpec {
                state_key: "current_browser_path".into(),
                workflow: "publish_post_url".into(),
                pattern: Some("^/posts/[a-z0-9-]+$".into()),
            },
        );
        let mut adapter = MockRuntimeAdapter::new();
        adapter
            .changed_hooks
            .insert("current_post_slug".into(), true);
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec!["startup".into()],
        });

        let mut workflows = BTreeMap::new();
        workflows.insert("rust".into(), vec!["src/main.rs".into()]);
        runtime.handle_event(RuntimeEvent::WatchChanges { workflows });
        runtime.handle_event(RuntimeEvent::MaintainTick { now_ms: 500 });

        let exit = execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute runtime effects");

        assert!(!exit);

        runtime.handle_event(RuntimeEvent::CtrlC);
        let exit = execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute runtime effects after ctrl-c");

        assert!(exit);
        assert_eq!(
            adapter.calls,
            vec![
                "persist:root",
                "event_server",
                "autostart",
                "workflow:startup:",
                "watch",
                "workflow:rust:src/main.rs",
                "maintain",
                "poll:current_post_slug",
                "workflow:publish_post_url:",
                "log:received ctrl-c, shutting down",
                "stop_watch",
                "stop_all",
            ]
        );
    }

    #[test]
    fn forward_watcher_event_ignores_send_failures_after_shutdown() {
        let (tx, rx) = mpsc::channel();
        let shutdown = AtomicBool::new(true);
        drop(rx);

        forward_watcher_event(
            &tx,
            &shutdown,
            Ok(Event {
                kind: EventKind::Any,
                paths: vec![PathBuf::from("content/layout.html")],
                attrs: Default::default(),
            }),
        );
    }

    #[tokio::test]
    async fn runtime_machine_does_not_run_observed_workflow_when_hook_state_is_unchanged() {
        let mut config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::from([(
                "browser_path".into(),
                crate::config::EventSpec {
                    state_key: "current_browser_path".into(),
                    workflow: "publish_post_url".into(),
                    pattern: Some("^/posts/[a-z0-9-]+$".into()),
                },
            )]),
            workflow: BTreeMap::new(),
        };
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
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec![],
        });
        let mut adapter = MockRuntimeAdapter::new();
        execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute startup effects");

        runtime.handle_event(RuntimeEvent::MaintainTick { now_ms: 500 });
        execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute maintenance effects");

        assert_eq!(
            adapter.calls,
            vec![
                "persist:root",
                "event_server",
                "autostart",
                "watch",
                "maintain",
                "poll:current_post_slug"
            ]
        );
    }

    #[tokio::test]
    async fn runtime_machine_runs_workflow_for_external_trigger() {
        let config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::from([(
                "browser_path".into(),
                crate::config::EventSpec {
                    state_key: "current_browser_path".into(),
                    workflow: "publish_post_url".into(),
                    pattern: Some("^/posts/[a-z0-9-]+$".into()),
                },
            )]),
            workflow: BTreeMap::new(),
        };
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec![],
        });
        let mut adapter = MockRuntimeAdapter::new();
        execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute startup effects");

        runtime.handle_event(RuntimeEvent::WorkflowTrigger {
            workflow_name: "publish_post_url".into(),
        });
        execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute external trigger");

        assert_eq!(
            adapter.calls,
            vec![
                "persist:root",
                "event_server",
                "autostart",
                "watch",
                "workflow:publish_post_url:",
            ]
        );
    }

    #[tokio::test]
    async fn runtime_machine_logs_and_continues_after_workflow_failure() {
        let config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec!["startup".into()],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        let mut runtime = RuntimeMachine::new(&config);
        runtime.handle_event(RuntimeEvent::Start {
            root_display: "/tmp/example".into(),
            startup_workflows: vec!["startup".into()],
        });
        let mut adapter = MockRuntimeAdapter::new();
        adapter.workflow_errors.insert(
            "startup".into(),
            "timed out waiting for process 'server' probe http://127.0.0.1:8080/".into(),
        );

        let exit = execute_runtime_effects(&mut runtime, &mut adapter)
            .await
            .expect("execute startup effects");

        assert!(!exit);
        assert_eq!(adapter.calls, vec!["persist:root", "autostart", "watch"]);
    }

    #[tokio::test]
    async fn missing_runtime_workflow_returns_error() {
        let config = Config {
            root: PathBuf::from("."),
            debounce_ms: 100,
            state_file: Some(PathBuf::from("./state.json")),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        };
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        let mut processes = ProcessManager::new(&config);

        let error = run_workflow(&config, &mut processes, &state, None, "missing", &[])
            .await
            .expect_err("missing workflow should error");

        assert!(
            error
                .to_string()
                .contains("runtime requested missing workflow 'missing'")
        );
        let _ = std::fs::remove_file(state_path);
    }
}
