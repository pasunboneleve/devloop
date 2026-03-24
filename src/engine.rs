use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::Value;
use tokio::signal;
use tokio::time::{Instant, sleep};
use tracing::{info, warn};

use crate::config::{CompiledWatchGroup, Config, WorkflowStep};
use crate::processes::ProcessManager;
use crate::state::SessionState;

pub struct Engine {
    config: Config,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let mut state = SessionState::load(
            self.config
                .state_file
                .clone()
                .ok_or_else(|| anyhow!("state file missing after config load"))?,
        )?;
        state.set(
            "root",
            Value::String(self.config.root.display().to_string()),
        )?;
        let mut processes = ProcessManager::new(&self.config);
        processes.start_autostart(&state).await?;
        for workflow_name in &self.config.startup_workflows {
            run_workflow(&self.config, &mut processes, &mut state, workflow_name, &[]).await?;
        }

        let watch_groups = self.config.compiled_watchers()?;
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |result| {
                let _ = tx.send(result);
            },
            NotifyConfig::default(),
        )?;
        watcher.watch(&self.config.root, RecursiveMode::Recursive)?;
        info!("watching {}", self.config.root.display());

        loop {
            tokio::select! {
                result = signal::ctrl_c() => {
                    result?;
                    info!("received ctrl-c, shutting down");
                    processes.stop_all().await?;
                    return Ok(());
                }
                batch = next_batch(&rx, self.config.debounce()) => {
                    let Some(events) = batch? else {
                        continue;
                    };
                    let changes = classify_events(&self.config.root, &watch_groups, &events);
                    for (workflow_name, changed_files) in changes {
                        run_workflow(
                            &self.config,
                            &mut processes,
                            &mut state,
                            &workflow_name,
                            &changed_files,
                        ).await?;
                    }
                }
            }
        }
    }
}

async fn run_workflow(
    config: &Config,
    processes: &mut ProcessManager<'_>,
    state: &mut SessionState,
    workflow_name: &str,
    changed_files: &[String],
) -> Result<()> {
    let Some(workflow) = config.workflow.get(workflow_name) else {
        warn!("skipping missing workflow {}", workflow_name);
        return Ok(());
    };
    info!("running workflow {}", workflow_name);
    state.set("last_workflow", workflow_name.to_owned().into())?;
    state.set(
        "last_changed_files",
        Value::Array(
            changed_files
                .iter()
                .map(|file| Value::String(file.clone()))
                .collect(),
        ),
    )?;
    for step in &workflow.steps {
        match step {
            WorkflowStep::StartProcess { process } => processes.start_named(process, state).await?,
            WorkflowStep::StopProcess { process } => processes.stop_named(process).await?,
            WorkflowStep::RestartProcess { process } => {
                processes.restart_named(process, state).await?
            }
            WorkflowStep::WaitForProcess { process } => processes.wait_for_named(process).await?,
            WorkflowStep::RunHook { hook } => {
                processes
                    .run_hook(hook, state, changed_files, workflow_name)
                    .await?
            }
            WorkflowStep::SleepMs { duration_ms } => {
                sleep(Duration::from_millis(*duration_ms)).await;
            }
            WorkflowStep::WriteState { key, value } => {
                state.set(key, value.clone().into())?;
            }
        }
    }
    Ok(())
}

async fn next_batch(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce: Duration,
) -> Result<Option<Vec<Event>>> {
    let first = match rx.recv() {
        Ok(result) => result?,
        Err(_) => return Ok(None),
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
    Ok(Some(events))
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
            let Ok(relative) = path.strip_prefix(root) else {
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
    use notify::{Event, EventKind, event::ModifyKind};
    use std::path::PathBuf;

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
}
