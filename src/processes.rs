use std::collections::{BTreeMap, VecDeque};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Stderr, Stdout};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use crate::browser_reload::{BrowserReloadEnvironment, apply_browser_reload_env};
use crate::config::{
    Config, HookOutputConfig, HookSpec, OutputBodyStyle, OutputExtract, OutputRule, ProbeSpec,
    ProcessSpec,
};
use crate::core::{ProcessEffect, ProcessSupervisor};
use crate::env_expand;
use crate::external_events::{ExternalEventEnvironment, apply_external_event_env};
use crate::output::{
    dim_start, format_output_prefix_with_style, should_colorize_output, style_reset,
};
use crate::session_log::SessionLog;
use crate::state::SessionState;
use devloop::process_guardian::GuardianExecutable;

pub struct ProcessManager<'a> {
    config: &'a Config,
    children: BTreeMap<String, ManagedProcess>,
    client: reqwest::Client,
    shutting_down: bool,
    stdout: Arc<Mutex<Stdout>>,
    stderr: Arc<Mutex<Stderr>>,
    supervisor: ProcessSupervisor,
    output_cleanup_tasks: JoinSet<Result<()>>,
    output_generations: BTreeMap<String, Arc<StdMutex<u64>>>,
    clock_start: Instant,
    external_event_env: Option<ExternalEventEnvironment>,
    browser_reload_env: Option<BrowserReloadEnvironment>,
    session_log: Option<SessionLog>,
    guardian_executable: GuardianExecutable,
}

struct ManagedProcess {
    guarded: GuardedProcess,
    output_tasks: Vec<OutputTask>,
}

/// Owns every external command behind one process-containment boundary.
///
/// The control socket remains open for exactly as long as devloop owns the
/// command. If devloop disappears, socket EOF makes the guardian kill the
/// complete process group without relying on supervisor shutdown code.
struct GuardedProcess {
    child: Child,
    process_group: Pid,
    _lifetime: UnixStream,
}

struct OutputTask {
    handle: JoinHandle<()>,
}

#[derive(Clone)]
struct OutputStateGeneration {
    current: Arc<StdMutex<u64>>,
    value: u64,
}

const OUTPUT_DRAIN_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_DRAIN_ABORT_TIMEOUT: Duration = Duration::from_secs(1);
const TERMINAL_OUTPUT_QUEUE_CAPACITY: usize = 256;
const SESSION_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const GUARDIAN_REAP_TIMEOUT: Duration = Duration::from_secs(2);

struct CommandContext<'a> {
    env: &'a BTreeMap<String, String>,
    external_event_env: Option<&'a ExternalEventEnvironment>,
    browser_reload_env: Option<&'a BrowserReloadEnvironment>,
    root: &'a Path,
    state_path: &'a Path,
    changed_files: &'a [String],
    workflow: &'a str,
}

impl<'a> ProcessManager<'a> {
    pub fn new(config: &'a Config, guardian_executable: GuardianExecutable) -> Self {
        Self {
            config,
            children: BTreeMap::new(),
            client: reqwest::Client::new(),
            shutting_down: false,
            stdout: Arc::new(Mutex::new(tokio::io::stdout())),
            stderr: Arc::new(Mutex::new(tokio::io::stderr())),
            supervisor: ProcessSupervisor::new(config),
            output_cleanup_tasks: JoinSet::new(),
            output_generations: BTreeMap::new(),
            clock_start: Instant::now(),
            external_event_env: None,
            browser_reload_env: None,
            session_log: None,
            guardian_executable,
        }
    }

    pub fn with_session_log(mut self, session_log: SessionLog) -> Self {
        self.session_log = Some(session_log);
        self
    }

    pub fn set_external_event_env(&mut self, external_event_env: Option<ExternalEventEnvironment>) {
        self.external_event_env = external_event_env;
    }

    pub fn set_browser_reload_env(&mut self, browser_reload_env: Option<BrowserReloadEnvironment>) {
        self.browser_reload_env = browser_reload_env;
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
        terminate_child(name, &mut child.guarded.child, child.guarded.process_group).await?;
        self.supervisor.on_process_stopped(name);
        if let Err(error) =
            wait_for_output_tasks(name, child.output_tasks, self.session_log.clone()).await
        {
            warn!("output cleanup failed while stopping {name}: {error:#}");
        }
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
        let command = configure_command(
            &spec.command,
            resolve_cwd(&self.config.root, spec.cwd.as_deref()),
            CommandContext {
                env: &spec.env,
                external_event_env: self.external_event_env.as_ref(),
                browser_reload_env: self.browser_reload_env.as_ref(),
                root: &self.config.root,
                state_path: state.path(),
                changed_files,
                workflow,
            },
        )?;
        let source_label = process_output_source_label(name, &spec.command);
        let mut guarded =
            spawn_guarded_process(name, command, Stdio::null(), &self.guardian_executable).await?;
        let stdout = guarded
            .child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture stdout for hook '{name}'"))?;
        let stderr = guarded
            .child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture stderr for hook '{name}'"))?;
        let stdout_task = collect_hook_output(
            stdout,
            self.session_log.clone(),
            source_label.clone(),
            name.to_owned(),
        );
        let stderr_task = collect_hook_output(
            stderr,
            self.session_log.clone(),
            source_label.clone(),
            name.to_owned(),
        );
        let child_status = async move {
            let status = guarded.child.wait().await.map_err(anyhow::Error::from)?;
            signal_process_group(name, guarded.process_group, Signal::KILL)?;
            Ok::<_, anyhow::Error>(status)
        };
        let (status, stdout, stderr) = tokio::try_join!(child_status, stdout_task, stderr_task)
            .with_context(|| format!("failed to run hook '{name}'"))?;
        if let Some(session_log) = &self.session_log
            && let Err(error) = flush_session_log_with_timeout(session_log).await
        {
            eprintln!("devloop: failed to flush persisted output for hook {name}: {error}");
        }

        self.render_hook_output(&source_label, &spec.output, &stdout, &stderr)
            .await;
        if !status.success() {
            return Err(anyhow!("hook '{name}' failed with status {}", status));
        }
        let stdout = String::from_utf8(stdout)
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
        let mut cleanup_errors = Vec::new();
        for effect in self.supervisor.on_shutdown() {
            match effect {
                ProcessEffect::StopProcess { process } => {
                    if let Err(error) = self.stop_named(&process).await {
                        cleanup_errors.push(error.context(format!(
                            "failed to stop process '{process}' during shutdown"
                        )));
                    }
                }
                effect => {
                    if let Err(error) = self.apply_process_effect(effect, state).await {
                        cleanup_errors
                            .push(error.context("failed to apply shutdown process effect"));
                    }
                }
            }
        }
        if let Err(error) = self.finish_output_cleanup_tasks().await {
            cleanup_errors.push(error.context("failed to finish output cleanup during shutdown"));
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(join_cleanup_errors(cleanup_errors))
        }
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
                managed.guarded.child.try_wait()?
            };

            if let Some(status) = exited {
                warn!("process {} exited with {}", name, status);
                let managed = self
                    .children
                    .remove(&name)
                    .ok_or_else(|| anyhow!("missing exited process '{name}'"))?;
                signal_process_group(&name, managed.guarded.process_group, Signal::KILL)?;
                self.spawn_output_cleanup(name.clone(), managed.output_tasks);
                exits.push((name, status.success()));
            }
        }
        self.reap_output_cleanup_tasks();
        let now_ms = self.clock_start.elapsed().as_millis() as u64;
        for effect in self.supervisor.on_tick(self.config, now_ms, exits) {
            self.apply_process_effect(effect, state).await?;
        }
        Ok(())
    }

    fn spawn_output_cleanup(&mut self, name: String, output_tasks: Vec<OutputTask>) {
        let session_log = self.session_log.clone();
        self.output_cleanup_tasks
            .spawn(async move { wait_for_output_tasks(&name, output_tasks, session_log).await });
    }

    fn reap_output_cleanup_tasks(&mut self) {
        while let Some(result) = self.output_cleanup_tasks.try_join_next() {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!("output cleanup failed: {error:#}"),
                Err(error) => warn!("output cleanup task panicked: {error}"),
            }
        }
    }

    fn next_output_state_generation(
        &mut self,
        name: &str,
        rules: &[OutputRule],
        state: &SessionState,
    ) -> Result<OutputStateGeneration> {
        let current = self
            .output_generations
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(StdMutex::new(0)))
            .clone();
        let mut generation = current
            .lock()
            .map_err(|_| anyhow!("output generation mutex for process '{name}' was poisoned"))?;
        *generation += 1;
        let value = *generation;
        clear_output_state_keys(rules, state)?;
        drop(generation);
        Ok(OutputStateGeneration { current, value })
    }

    async fn finish_output_cleanup_tasks(&mut self) -> Result<()> {
        let mut cleanup_errors = Vec::new();
        while let Some(result) = self.output_cleanup_tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => cleanup_errors.push(error),
                Err(error) => cleanup_errors.push(anyhow!("output cleanup task panicked: {error}")),
            }
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(join_cleanup_errors(cleanup_errors))
        }
    }

    async fn start(&mut self, name: &str, spec: &ProcessSpec, state: &SessionState) -> Result<()> {
        if self.children.contains_key(name) {
            return Ok(());
        }
        let command = configure_command(
            &spec.command,
            resolve_cwd(&self.config.root, spec.cwd.as_deref()),
            CommandContext {
                env: &spec.env,
                external_event_env: self.external_event_env.as_ref(),
                browser_reload_env: self.browser_reload_env.as_ref(),
                root: &self.config.root,
                state_path: state.path(),
                changed_files: &[],
                workflow: "startup",
            },
        )?;
        let mut guarded =
            spawn_guarded_process(name, command, Stdio::inherit(), &self.guardian_executable)
                .await?;
        let output_generation =
            self.next_output_state_generation(name, &spec.output.rules, state)?;
        let process_name = name.to_owned();
        let source_label = process_output_source_label(name, &spec.command);
        let inherit_output = spec.output.inherit;
        let body_style = spec.output.body_style;
        let rules = compile_output_rules(&spec.output.rules)?;
        let stdout_sink = OutputSink::Stdout(self.stdout.clone());
        let stderr_sink = OutputSink::Stderr(self.stderr.clone());
        let mut output_tasks = Vec::new();
        if let Some(stdout) = guarded.child.stdout.take() {
            output_tasks.push(OutputTask {
                handle: tokio::spawn(forward_output_lines(
                    stdout,
                    ForwardOutputConfig {
                        output: stdout_sink,
                        source_label: source_label.clone(),
                        inherit_output,
                        body_style,
                        session_log: self.session_log.clone(),
                    },
                    process_name.clone(),
                    rules.clone(),
                    state.clone(),
                    output_generation.clone(),
                )),
            });
        }
        if let Some(stderr) = guarded.child.stderr.take() {
            output_tasks.push(OutputTask {
                handle: tokio::spawn(forward_output_lines(
                    stderr,
                    ForwardOutputConfig {
                        output: stderr_sink,
                        source_label,
                        inherit_output,
                        body_style,
                        session_log: self.session_log.clone(),
                    },
                    process_name,
                    rules,
                    state.clone(),
                    output_generation,
                )),
            });
        }
        self.children.insert(
            name.to_owned(),
            ManagedProcess {
                guarded,
                output_tasks,
            },
        );
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
                    let healthy = match expand_probe_env(&process, liveness) {
                        Ok(liveness) => {
                            match check_probe(&self.client, &process, &liveness, state).await {
                                Ok(()) => true,
                                Err(error) => {
                                    warn!("liveness probe failed for {}: {}", process, error);
                                    false
                                }
                            }
                        }
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

/// Spawns a managed command behind devloop's internal Rust guardian.
///
/// The guardian stays outside the target process group, so ordinary TERM/KILL
/// signals reach only the target tree. It watches a private lifetime socket;
/// EOF after any devloop exit makes it kill the group and reap its direct target.
async fn spawn_guarded_process(
    name: &str,
    command: Command,
    stdin: Stdio,
    guardian_executable: &GuardianExecutable,
) -> Result<GuardedProcess> {
    let (mut lifetime, guardian_control) =
        UnixStream::pair().context("failed to create parent-death control socket")?;
    let guardian_control_fd = guardian_control.as_raw_fd();
    let target = command.as_std();
    let guardian_image = guardian_executable.prepare_invocation()?;
    let guardian_image_fd = guardian_image.inherited_image_fd();
    let mut guardian = Command::new(guardian_image.path());
    devloop::process_guardian::append_invocation(&mut guardian, target);
    if let Some(cwd) = target.get_current_dir() {
        guardian.current_dir(cwd);
    }
    for (key, value) in target.get_envs() {
        match value {
            Some(value) => {
                guardian.env(key, value);
            }
            None => {
                guardian.env_remove(key);
            }
        }
    }
    guardian.stdout(Stdio::piped());
    guardian.stderr(Stdio::piped());
    guardian.stdin(stdin);
    guardian.process_group(0);
    // SAFETY: this closure runs after fork and before exec. It only uses
    // async-signal-safe libc calls to expose the already-open control socket
    // at one fixed descriptor in the guardian process.
    unsafe {
        guardian.pre_exec(move || {
            if let Some(guardian_image_fd) = guardian_image_fd {
                let guardian_image_flags = libc::fcntl(guardian_image_fd, libc::F_GETFD);
                if guardian_image_flags == -1
                    || libc::fcntl(
                        guardian_image_fd,
                        libc::F_SETFD,
                        guardian_image_flags & !libc::FD_CLOEXEC,
                    ) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if guardian_control_fd == devloop::process_guardian::CONTROL_FD {
                let flags = libc::fcntl(devloop::process_guardian::CONTROL_FD, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(
                        devloop::process_guardian::CONTROL_FD,
                        libc::F_SETFD,
                        flags & !libc::FD_CLOEXEC,
                    ) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                if libc::dup2(guardian_control_fd, devloop::process_guardian::CONTROL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::close(guardian_control_fd) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = guardian
        .spawn()
        .with_context(|| format!("failed to start guarded process '{name}'"))?;
    drop(guardian_image);
    drop(guardian_control);
    let process_group = match devloop::process_guardian::receive_process_group(&mut lifetime) {
        Ok(process_group) => process_group,
        Err(error) => {
            drop(lifetime);
            child
                .wait()
                .await
                .with_context(|| format!("failed to reap guardian for process '{name}'"))?;
            return Err(error)
                .with_context(|| format!("guardian failed to start process '{name}'"));
        }
    };
    Ok(GuardedProcess {
        child,
        process_group,
        _lifetime: lifetime,
    })
}

async fn wait_for_output_tasks(
    name: &str,
    output_tasks: Vec<OutputTask>,
    session_log: Option<SessionLog>,
) -> Result<()> {
    let mut drain_tasks = JoinSet::new();
    for output_task in output_tasks {
        let name = name.to_owned();
        let session_log = session_log.clone();
        drain_tasks
            .spawn(async move { wait_for_output_task(name, output_task, session_log).await });
    }

    while let Some(result) = drain_tasks.join_next().await {
        result.with_context(|| format!("output drain task for process '{name}' panicked"))??;
    }
    if let Some(session_log) = session_log {
        flush_session_log_with_timeout(&session_log).await?;
    }
    Ok(())
}

async fn flush_session_log_with_timeout(session_log: &SessionLog) -> Result<()> {
    timeout(SESSION_LOG_FLUSH_TIMEOUT, session_log.flush_queued())
        .await
        .map_err(|_| anyhow!("timed out flushing session log"))?
        .map_err(anyhow::Error::from)
}

async fn wait_for_output_task(
    name: String,
    output_task: OutputTask,
    session_log: Option<SessionLog>,
) -> Result<()> {
    wait_for_output_task_with_deadline(name, output_task, session_log, OUTPUT_DRAIN_TOTAL_TIMEOUT)
        .await
}

async fn wait_for_output_task_with_deadline(
    name: String,
    mut output_task: OutputTask,
    session_log: Option<SessionLog>,
    drain_timeout: Duration,
) -> Result<()> {
    let drain_timeout_ms = drain_timeout.as_millis();
    let drain_timeout = sleep(drain_timeout);
    tokio::pin!(drain_timeout);

    tokio::select! {
        result = &mut output_task.handle => {
            result.with_context(|| {
                format!("output forwarding task for process '{name}' panicked")
            })?;
            Ok(())
        }
        () = &mut drain_timeout => {
            output_task.handle.abort();
            if timeout(OUTPUT_DRAIN_ABORT_TIMEOUT, &mut output_task.handle)
                .await
                .is_err()
            {
                warn!(
                    "timed out waiting for aborted output forwarding task for process '{name}'"
                );
            }
            report_abandoned_output_drain(
                &name,
                &format!(
                    "process output may be truncated: output for {} did not finish draining within {} ms; abandoned a forwarding task",
                    name,
                    drain_timeout_ms
                ),
                session_log.as_ref(),
            );
            Ok(())
        }
    }
}

fn report_abandoned_output_drain(name: &str, message: &str, session_log: Option<&SessionLog>) {
    if let Some(session_log) = session_log
        && let Err(error) = session_log.write_labeled_line("devloop", message.as_bytes())
    {
        eprintln!("devloop: failed to persist output truncation marker for {name}: {error}");
    }
    eprintln!("devloop: {message}");
}

fn join_cleanup_errors(errors: Vec<anyhow::Error>) -> anyhow::Error {
    let messages = errors
        .into_iter()
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow!("shutdown cleanup failed: {messages}")
}

async fn collect_hook_output<T>(
    reader: T,
    session_log: Option<SessionLog>,
    source_label: String,
    hook_name: String,
) -> Result<Vec<u8>>
where
    T: tokio::io::AsyncRead + Unpin,
{
    let mut reader = reader;
    let mut chunk = [0_u8; 4096];
    let mut output = Vec::new();
    let mut session_log_line = Vec::new();
    let mut session_log_last_was_carriage_return = false;
    let mut session_log = session_log;

    loop {
        let bytes_read = reader
            .read(&mut chunk)
            .await
            .with_context(|| format!("failed to read output for hook '{hook_name}'"))?;
        if bytes_read == 0 {
            break;
        }

        output.extend_from_slice(&chunk[..bytes_read]);
        for &byte in &chunk[..bytes_read] {
            if let Some(log) = &session_log
                && let Err(error) = persist_output_byte_blocking(
                    log,
                    &source_label,
                    byte,
                    &mut session_log_line,
                    &mut session_log_last_was_carriage_return,
                )
                .await
            {
                eprintln!("devloop: failed to persist output for hook {hook_name}: {error}");
                session_log = None;
            }
        }
    }

    if let Some(log) = &session_log
        && !session_log_line.is_empty()
        && let Err(error) = log
            .queue_labeled_line(&source_label, session_log_line)
            .await
    {
        eprintln!("devloop: failed to persist output for hook {hook_name}: {error}");
    }

    Ok(output)
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
    session_log: Option<SessionLog>,
}

async fn forward_output_lines<T>(
    reader: T,
    config: ForwardOutputConfig,
    process_name: String,
    rules: Vec<CompiledOutputRule>,
    state: SessionState,
    output_generation: OutputStateGeneration,
) where
    T: tokio::io::AsyncRead + Unpin,
{
    let colorize = should_colorize_output();
    let mut reader = reader;
    let mut chunk = [0_u8; 4096];
    let mut line_buffer = Vec::new();
    let mut render_state = OutputRenderState::default();
    let mut terminal_output = config
        .inherit_output
        .then(|| TerminalOutput::start(config.output.clone()));
    let mut last_was_carriage_return = false;
    let mut session_log_line = Vec::new();
    let mut session_log_last_was_carriage_return = false;
    let mut session_log = config.session_log;

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
            if let Some(log) = &session_log
                && let Err(error) = persist_output_byte_blocking(
                    log,
                    &config.source_label,
                    byte,
                    &mut session_log_line,
                    &mut session_log_last_was_carriage_return,
                )
                .await
            {
                eprintln!("devloop: failed to persist output for {process_name}: {error}");
                session_log = None;
            }
            if let Some(terminal_output) = &mut terminal_output
                && let Some(record) = prepare_output_byte(
                    &config.source_label,
                    byte,
                    colorize,
                    config.body_style,
                    &mut render_state,
                )
            {
                terminal_output.queue(record, &process_name).await;
            }
            process_output_byte_for_rules_guarded(
                &process_name,
                byte,
                &mut line_buffer,
                &mut last_was_carriage_return,
                &rules,
                &state,
                Some(&output_generation),
            );
        }
    }

    if !line_buffer.is_empty() {
        process_output_line_guarded(
            &process_name,
            &line_buffer,
            &rules,
            &state,
            Some(&output_generation),
        );
    }
    if let Some(log) = &session_log
        && !session_log_line.is_empty()
        && let Err(error) = log
            .queue_labeled_line(&config.source_label, session_log_line)
            .await
    {
        eprintln!("devloop: failed to persist output for {process_name}: {error}");
    }
    if let Some(mut terminal_output) = terminal_output {
        if let Some(record) = prepare_flush_rendered_output(&mut render_state, false) {
            terminal_output.queue(record, &process_name).await;
        }
        terminal_output.finish(&process_name).await;
    }
}

#[cfg(test)]
fn persist_output_byte(
    session_log: &SessionLog,
    source_label: &str,
    byte: u8,
    line: &mut Vec<u8>,
    last_was_carriage_return: &mut bool,
) -> std::io::Result<()> {
    if byte == b'\r' {
        session_log.write_labeled_line(source_label, line)?;
        line.clear();
        *last_was_carriage_return = true;
        return Ok(());
    }
    if byte == b'\n' {
        if !*last_was_carriage_return {
            session_log.write_labeled_line(source_label, line)?;
            line.clear();
        }
        *last_was_carriage_return = false;
        return Ok(());
    }
    *last_was_carriage_return = false;
    line.push(byte);
    Ok(())
}

async fn persist_output_byte_blocking(
    session_log: &SessionLog,
    source_label: &str,
    byte: u8,
    line: &mut Vec<u8>,
    last_was_carriage_return: &mut bool,
) -> std::io::Result<()> {
    if byte == b'\r' {
        session_log
            .queue_labeled_line(source_label, std::mem::take(line))
            .await?;
        *last_was_carriage_return = true;
        return Ok(());
    }
    if byte == b'\n' {
        if !*last_was_carriage_return {
            session_log
                .queue_labeled_line(source_label, std::mem::take(line))
                .await?;
        }
        *last_was_carriage_return = false;
        return Ok(());
    }
    *last_was_carriage_return = false;
    line.push(byte);
    Ok(())
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

struct TerminalOutput {
    records: Option<mpsc::Sender<Vec<u8>>>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl TerminalOutput {
    fn start(output: OutputSink) -> Self {
        let (records, mut received_records) =
            mpsc::channel::<Vec<u8>>(TERMINAL_OUTPUT_QUEUE_CAPACITY);
        let handle = tokio::spawn(async move {
            while let Some(record) = received_records.recv().await {
                write_output_record(&output, &record).await?;
            }
            Ok(())
        });
        Self {
            records: Some(records),
            handle: Some(handle),
        }
    }

    async fn queue(&mut self, record: Vec<u8>, process_name: &str) {
        let Some(records) = &self.records else {
            return;
        };
        match records.try_send(record) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(record)) => {
                self.queue_after_backpressure(record, process_name).await
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.records = None;
                warn!(
                    "terminal output writer for {process_name} stopped; suppressing inherited terminal output while persistent logging continues"
                );
            }
        }
    }

    async fn queue_after_backpressure(&mut self, record: Vec<u8>, process_name: &str) {
        tokio::task::yield_now().await;
        let Some(records) = &self.records else {
            return;
        };
        if records.send(record).await.is_err() {
            self.records = None;
            warn!(
                "terminal output writer for {process_name} stopped; suppressing inherited terminal output while persistent logging continues"
            );
        }
    }

    async fn finish(&mut self, process_name: &str) {
        self.records = None;
        let Some(handle) = self.handle.take() else {
            return;
        };
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!("failed to write output for {}: {}", process_name, error);
            }
            Err(error) => {
                warn!("terminal output task for {process_name} panicked: {error}");
            }
        }
    }
}

impl Drop for TerminalOutput {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

async fn write_output_record(output: &OutputSink, record: &[u8]) -> std::io::Result<()> {
    match output {
        OutputSink::Stdout(writer) => write_output_record_to_writer(writer, record).await,
        OutputSink::Stderr(writer) => write_output_record_to_writer(writer, record).await,
    }
}

async fn write_output_record_to_writer<W>(
    writer: &Arc<Mutex<W>>,
    record: &[u8],
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + Send,
{
    let mut writer = writer.lock().await;
    writer.write_all(record).await?;
    writer.flush().await
}

fn prepare_output_byte(
    source_label: &str,
    byte: u8,
    colorize: bool,
    body_style: OutputBodyStyle,
    render_state: &mut OutputRenderState,
) -> Option<Vec<u8>> {
    if byte == b'\r' {
        render_state.body_style = body_style;
        render_state.colorize = colorize;
        let record = prepare_flush_rendered_output(render_state, true);
        render_state.last_was_carriage_return = true;
        return record;
    }

    if byte == b'\n' {
        render_state.body_style = body_style;
        render_state.colorize = colorize;
        if render_state.last_was_carriage_return {
            render_state.last_was_carriage_return = false;
            return None;
        }
        return prepare_flush_rendered_output(render_state, true);
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
    None
}

fn prepare_flush_rendered_output(
    render_state: &mut OutputRenderState,
    add_newline: bool,
) -> Option<Vec<u8>> {
    flush_pending_utf8(render_state);

    if render_state.rendered_line.is_empty() && !add_newline {
        return None;
    }

    let mut record = Vec::new();
    if !render_state.rendered_line.is_empty() {
        if render_state.dim_active {
            render_state
                .rendered_line
                .push_str(style_reset(render_state.colorize));
            render_state.dim_active = false;
        }
        record.extend_from_slice(render_state.rendered_line.as_bytes());
        render_state.rendered_line.clear();
    }
    if add_newline {
        record.push(b'\n');
    }
    render_state.at_line_start = true;
    Some(record)
}

async fn flush_rendered_output_to_writer<W>(
    writer: &Arc<Mutex<W>>,
    render_state: &mut OutputRenderState,
    add_newline: bool,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + Send,
{
    let Some(record) = prepare_flush_rendered_output(render_state, add_newline) else {
        return Ok(());
    };
    write_output_record_to_writer(writer, &record).await
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
    if let Some(record) =
        prepare_output_byte(source_label, byte, colorize, body_style, render_state)
    {
        write_output_record_to_writer(writer, &record).await?;
    }
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

#[cfg(test)]
fn process_output_byte_for_rules(
    process_name: &str,
    byte: u8,
    line_buffer: &mut Vec<u8>,
    last_was_carriage_return: &mut bool,
    rules: &[CompiledOutputRule],
    state: &SessionState,
) {
    process_output_byte_for_rules_guarded(
        process_name,
        byte,
        line_buffer,
        last_was_carriage_return,
        rules,
        state,
        None,
    );
}

fn process_output_byte_for_rules_guarded(
    process_name: &str,
    byte: u8,
    line_buffer: &mut Vec<u8>,
    last_was_carriage_return: &mut bool,
    rules: &[CompiledOutputRule],
    state: &SessionState,
    output_generation: Option<&OutputStateGeneration>,
) {
    if byte == b'\r' {
        process_output_line_guarded(process_name, line_buffer, rules, state, output_generation);
        line_buffer.clear();
        *last_was_carriage_return = true;
        return;
    }

    if byte == b'\n' {
        if !*last_was_carriage_return {
            process_output_line_guarded(process_name, line_buffer, rules, state, output_generation);
            line_buffer.clear();
        }
        *last_was_carriage_return = false;
        return;
    }

    *last_was_carriage_return = false;
    line_buffer.push(byte);
}

fn process_output_line_guarded(
    process_name: &str,
    bytes: &[u8],
    rules: &[CompiledOutputRule],
    state: &SessionState,
    output_generation: Option<&OutputStateGeneration>,
) {
    let _generation_guard = match output_generation {
        Some(generation) => {
            let guard = match generation.current.lock() {
                Ok(guard) => guard,
                Err(error) => {
                    warn!("output generation mutex for {process_name} was poisoned: {error}");
                    return;
                }
            };
            if *guard != generation.value {
                return;
            }
            Some(guard)
        }
        None => None,
    };

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
    let command = env_expand::expand_vec(command, "command")?;
    let Some(program) = command.first() else {
        return Err(anyhow!("command must not be empty"));
    };
    let program = resolve_program(context.root, program);
    let mut cmd = Command::new(program);
    cmd.args(&command[1..]);
    cmd.current_dir(cwd);
    let mut full_env = env_expand::expand_map(context.env, "env")?;
    apply_external_event_env(&mut full_env, context.external_event_env);
    apply_browser_reload_env(&mut full_env, context.browser_reload_env);
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

async fn terminate_child(name: &str, child: &mut Child, process_group: Pid) -> Result<()> {
    terminate_child_with_timeouts(
        name,
        child,
        process_group,
        PROCESS_STOP_TIMEOUT,
        GUARDIAN_REAP_TIMEOUT,
    )
    .await
}

async fn terminate_child_with_timeouts(
    name: &str,
    child: &mut Child,
    process_group: Pid,
    stop_timeout: Duration,
    guardian_reap_timeout: Duration,
) -> Result<()> {
    if child.try_wait()?.is_some() {
        signal_process_group(name, process_group, Signal::KILL)?;
        info!("process {} already exited; cleaned up process group", name);
        return Ok(());
    }

    signal_process_group(name, process_group, Signal::TERM)?;
    match timeout(stop_timeout, child.wait()).await {
        Ok(result) => {
            result.with_context(|| format!("failed to wait for process '{name}' after SIGTERM"))?;
            signal_process_group(name, process_group, Signal::KILL)?;
        }
        Err(_) => {
            signal_process_group(name, process_group, Signal::KILL)?;
            match timeout(guardian_reap_timeout, child.wait()).await {
                Ok(result) => {
                    result.with_context(|| {
                        format!("failed to wait for process '{name}' after SIGKILL")
                    })?;
                }
                Err(_) => {
                    warn!(
                        "process {name} did not exit after target group SIGKILL; killing its guardian"
                    );
                    child
                        .start_kill()
                        .with_context(|| format!("failed to kill guardian for process '{name}'"))?;
                    child
                        .wait()
                        .await
                        .with_context(|| format!("failed to reap guardian for process '{name}'"))?;
                }
            }
        }
    }
    info!("stopped process {}", name);
    Ok(())
}

fn signal_process_group(name: &str, process_group: Pid, signal: Signal) -> Result<()> {
    match kill_process_group(process_group, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(anyhow!(
            "failed to send {:?} to process group for process '{}': {}",
            signal,
            name,
            error
        )),
    }
}

async fn wait_for_probe(
    client: &reqwest::Client,
    name: &str,
    probe: &ProbeSpec,
    state: &SessionState,
) -> Result<()> {
    let expanded_probe = expand_probe_env(name, probe)?;
    let started = std::time::Instant::now();
    let timeout = match &expanded_probe {
        ProbeSpec::Http { timeout_ms, .. } | ProbeSpec::StateKey { timeout_ms, .. } => {
            Duration::from_millis(*timeout_ms)
        }
    };
    let interval = Duration::from_millis(expanded_probe.interval());
    loop {
        if check_probe(client, name, &expanded_probe, state)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(timeout_error(name, &expanded_probe));
        }
        sleep(interval).await;
    }
}

fn expand_probe_env(process: &str, probe: &ProbeSpec) -> Result<ProbeSpec> {
    match probe {
        ProbeSpec::Http {
            url,
            interval_ms,
            timeout_ms,
        } => Ok(ProbeSpec::Http {
            url: env_expand::expand_value(url, &format!("process '{process}' http probe url"))?,
            interval_ms: *interval_ms,
            timeout_ms: *timeout_ms,
        }),
        ProbeSpec::StateKey {
            key,
            interval_ms,
            timeout_ms,
        } => Ok(ProbeSpec::StateKey {
            key: key.clone(),
            interval_ms: *interval_ms,
            timeout_ms: *timeout_ms,
        }),
    }
}

async fn check_probe(
    client: &reqwest::Client,
    name: &str,
    probe: &ProbeSpec,
    state: &SessionState,
) -> Result<()> {
    match probe {
        ProbeSpec::Http {
            url,
            interval_ms,
            timeout_ms,
        } => match client
            .get(url)
            .timeout(probe_attempt_timeout(*interval_ms, *timeout_ms))
            .send()
            .await
        {
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

fn probe_attempt_timeout(interval_ms: u64, timeout_ms: u64) -> Duration {
    let bounded = interval_ms.saturating_mul(2).max(1000).min(timeout_ms);
    Duration::from_millis(bounded)
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
    use crate::config::{OutputConfig, OutputExtract, OutputRule, ProbeSpec};
    use crate::test_support::{EnvVarGuard, RustLogGuard};
    use rustix::process::test_kill_process;
    use serde_json::Value;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    static TEST_STATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn unique_state_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let sequence = TEST_STATE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("devloop-process-state-{unique}-{sequence}.json"))
    }

    fn test_config(root: &Path) -> Config {
        Config {
            root: root.to_path_buf(),
            debounce_ms: 100,
            watcher: crate::config::WatcherConfig::default(),
            state_file: Some(unique_state_path()),
            startup_workflows: vec![],
            watch: BTreeMap::new(),
            process: BTreeMap::new(),
            hook: BTreeMap::new(),
            event_server: crate::config::EventServerConfig::default(),
            browser_reload_server: crate::config::BrowserReloadServerConfig::default(),
            event: BTreeMap::new(),
            workflow: BTreeMap::new(),
        }
    }

    #[cfg(unix)]
    async fn wait_for_path(path: &Path) {
        let started = Instant::now();
        loop {
            if path.exists() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "timed out waiting for {}",
                path.display()
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guarded_process_preserves_configured_stdin() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("IFS= read -r line; printf '%s' \"$line\"");
        let guardian = GuardianExecutable::open().expect("open test guardian");
        let mut guarded = spawn_guarded_process("stdin-reader", command, Stdio::piped(), &guardian)
            .await
            .expect("spawn guard");
        let mut stdin = guarded.child.stdin.take().expect("take stdin");
        let mut stdout = guarded.child.stdout.take().expect("take stdout");

        stdin
            .write_all(b"from-devloop\n")
            .await
            .expect("write stdin");
        drop(stdin);
        let status = guarded.child.wait().await.expect("wait for guard");
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .expect("read stdout");

        assert!(status.success());
        assert_eq!(output, "from-devloop");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guarded_process_surfaces_target_spawn_error() {
        let command = Command::new("devloop-test-command-that-does-not-exist");
        let guardian = GuardianExecutable::open().expect("open test guardian");
        let error = match spawn_guarded_process("missing", command, Stdio::null(), &guardian).await
        {
            Ok(_) => panic!("missing target unexpectedly started"),
            Err(error) => error,
        };
        let message = format!("{error:#}");

        assert!(
            message.contains("failed to start guarded command"),
            "{message}"
        );
        assert!(!message.contains("unexpected end of file"), "{message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_child_bounds_wait_for_an_unresponsive_guardian() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("trap '' TERM; printf ready; IFS= read -r _")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = command.spawn().expect("spawn guardian fixture");
        let mut ready = [0_u8; 5];
        child
            .stdout
            .as_mut()
            .expect("guardian fixture stdout")
            .read_exact(&mut ready)
            .await
            .expect("guardian fixture readiness");
        assert_eq!(&ready, b"ready");
        let nonexistent_group = Pid::from_raw(2_000_000_000).expect("nonexistent process group");

        terminate_child_with_timeouts(
            "unresponsive-guardian",
            &mut child,
            nonexistent_group,
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .await
        .expect("bound guardian wait");

        assert!(child.try_wait().expect("read guardian status").is_some());
    }

    #[cfg(unix)]
    async fn assert_process_gone(raw_pid: i32) {
        let pid = Pid::from_raw(raw_pid).expect("pid");
        let started = Instant::now();
        loop {
            if matches!(test_kill_process(pid), Err(Errno::SRCH)) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "process {raw_pid} still exists after managed process stop"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_named_terminates_descendant_processes() {
        let dir = tempdir().expect("tempdir");
        let pid_path = dir.path().join("descendant.pid");
        let script_path = dir.path().join("spawn-descendant.sh");
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
sh -c 'trap "" TERM; while :; do sleep 1; done' &
echo "$!" > "{}"
wait
"#,
                pid_path.display()
            ),
        )
        .expect("write script");

        let mut config = test_config(dir.path());
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["sh".into(), script_path.display().to_string()],
                cwd: Some(dir.path().to_path_buf()),
                autostart: false,
                readiness: None,
                liveness: None,
                restart: crate::config::RestartPolicy::Never,
                env: BTreeMap::new(),
                output: OutputConfig::default(),
            },
        );
        let state = SessionState::load(unique_state_path()).expect("state");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        );

        manager
            .start_named("server", &state)
            .await
            .expect("start process");
        wait_for_path(&pid_path).await;
        let descendant_pid = std::fs::read_to_string(&pid_path)
            .expect("read pid")
            .trim()
            .parse::<i32>()
            .expect("parse pid");

        manager.stop_named("server").await.expect("stop process");

        assert_process_gone(descendant_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_named_waits_for_final_process_output_to_reach_the_session_log() {
        let dir = tempdir().expect("tempdir");
        let started_path = dir.path().join("started");
        let script_path = dir.path().join("final-output.sh");
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
touch "{}"
trap 'printf final-output; exit 0' TERM
while :; do sleep 1; done
"#,
                started_path.display()
            ),
        )
        .expect("write script");

        let mut config = test_config(dir.path());
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["sh".into(), script_path.display().to_string()],
                cwd: Some(dir.path().to_path_buf()),
                autostart: false,
                readiness: None,
                liveness: None,
                restart: crate::config::RestartPolicy::Never,
                env: BTreeMap::new(),
                output: OutputConfig {
                    inherit: false,
                    ..OutputConfig::default()
                },
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log.clone());

        manager
            .start_named("server", &state)
            .await
            .expect("start process");
        wait_for_path(&started_path).await;
        manager.stop_named("server").await.expect("stop process");

        assert!(
            std::fs::read_to_string(log.path())
                .expect("read log")
                .contains("[sh server] final-output")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_named_continues_after_session_log_flush_failure() {
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("restartable.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
printf 'started\n'
exec sleep 600
"#,
        )
        .expect("write script");

        let mut config = test_config(dir.path());
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["sh".into(), script_path.display().to_string()],
                cwd: Some(dir.path().to_path_buf()),
                autostart: false,
                readiness: Some(ProbeSpec::StateKey {
                    key: "server_started".into(),
                    interval_ms: 10,
                    timeout_ms: 5000,
                }),
                liveness: None,
                restart: crate::config::RestartPolicy::Never,
                env: BTreeMap::new(),
                output: OutputConfig {
                    inherit: false,
                    rules: vec![OutputRule {
                        state_key: "server_started".into(),
                        pattern: Some("(started)".into()),
                        extract: OutputExtract::Regex,
                        capture_group: 1,
                    }],
                    ..OutputConfig::default()
                },
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log.clone());

        manager
            .start_named("server", &state)
            .await
            .expect("start process");
        manager
            .wait_for_named("server", &state)
            .await
            .expect("wait for first process readiness");
        log.fail_for_test(
            std::io::ErrorKind::BrokenPipe,
            "simulated session log failure",
        );

        manager
            .restart_named("server", &state)
            .await
            .expect("restart process");

        manager
            .wait_for_named("server", &state)
            .await
            .expect("wait for restarted process readiness");
        assert!(manager.children.contains_key("server"));
        manager.stop_named("server").await.expect("stop process");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn maintain_does_not_block_when_exited_wrapper_leaves_writing_descendant() {
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("writing-descendant.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
(while :; do printf x; done) &
exit 0
"#,
        )
        .expect("write script");

        let mut config = test_config(dir.path());
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["sh".into(), script_path.display().to_string()],
                cwd: Some(dir.path().to_path_buf()),
                autostart: false,
                readiness: None,
                liveness: None,
                restart: crate::config::RestartPolicy::Never,
                env: BTreeMap::new(),
                output: OutputConfig {
                    inherit: false,
                    ..OutputConfig::default()
                },
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file).expect("state");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        );

        manager
            .start_named("server", &state)
            .await
            .expect("start process");
        timeout(Duration::from_secs(5), async {
            while manager.children.contains_key("server") {
                manager.maintain(&state).await.expect("maintain process");
            }
        })
        .await
        .expect("maintain should not block on descendant output");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_all_waits_for_output_cleanup_scheduled_by_natural_exit() {
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("natural-output.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
printf natural-output
exit 0
"#,
        )
        .expect("write script");

        let mut config = test_config(dir.path());
        config.process.insert(
            "server".into(),
            ProcessSpec {
                command: vec!["sh".into(), script_path.display().to_string()],
                cwd: Some(dir.path().to_path_buf()),
                autostart: false,
                readiness: None,
                liveness: None,
                restart: crate::config::RestartPolicy::Never,
                env: BTreeMap::new(),
                output: OutputConfig {
                    inherit: false,
                    ..OutputConfig::default()
                },
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log.clone());

        manager
            .start_named("server", &state)
            .await
            .expect("start process");
        while manager.children.contains_key("server") {
            manager.maintain(&state).await.expect("maintain process");
        }
        manager.stop_all(&state).await.expect("stop all");

        assert!(
            std::fs::read_to_string(log.path())
                .expect("read log")
                .contains("[sh server] natural-output")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_all_treats_session_log_flush_failure_as_non_fatal() {
        let dir = tempdir().expect("tempdir");
        let first_started = dir.path().join("first-started");
        let second_started = dir.path().join("second-started");
        let first_script = dir.path().join("first.sh");
        let second_script = dir.path().join("second.sh");
        std::fs::write(
            &first_script,
            format!(
                r#"#!/bin/sh
touch "{}"
exec sleep 600
"#,
                first_started.display()
            ),
        )
        .expect("write first script");
        std::fs::write(
            &second_script,
            format!(
                r#"#!/bin/sh
touch "{}"
exec sleep 600
"#,
                second_started.display()
            ),
        )
        .expect("write second script");

        let mut config = test_config(dir.path());
        for (name, script) in [("first", first_script), ("second", second_script)] {
            config.process.insert(
                name.into(),
                ProcessSpec {
                    command: vec!["sh".into(), script.display().to_string()],
                    cwd: Some(dir.path().to_path_buf()),
                    autostart: false,
                    readiness: None,
                    liveness: None,
                    restart: crate::config::RestartPolicy::Never,
                    env: BTreeMap::new(),
                    output: OutputConfig {
                        inherit: false,
                        ..OutputConfig::default()
                    },
                },
            );
        }
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log.clone());

        manager
            .start_named("first", &state)
            .await
            .expect("start first process");
        manager
            .start_named("second", &state)
            .await
            .expect("start second process");
        wait_for_path(&first_started).await;
        wait_for_path(&second_started).await;
        log.fail_for_test(
            std::io::ErrorKind::BrokenPipe,
            "simulated session log failure",
        );

        manager
            .stop_all(&state)
            .await
            .expect("shutdown should continue despite session log failure");

        assert!(manager.children.is_empty());
    }

    #[tokio::test]
    async fn finish_output_cleanup_tasks_drains_all_failures() {
        let dir = tempdir().expect("tempdir");
        let config = test_config(dir.path());
        let mut manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        );
        manager
            .output_cleanup_tasks
            .spawn(async { Err(anyhow!("first cleanup failure")) });
        manager
            .output_cleanup_tasks
            .spawn(async { Err(anyhow!("second cleanup failure")) });

        let error = manager
            .finish_output_cleanup_tasks()
            .await
            .expect_err("cleanup should fail");

        assert!(manager.output_cleanup_tasks.is_empty());
        let error = format!("{error:#}");
        assert!(error.contains("first cleanup failure"));
        assert!(error.contains("second cleanup failure"));
    }

    #[tokio::test]
    async fn abandoned_output_drain_writes_truncation_marker_to_session_log() {
        let dir = tempdir().expect("tempdir");
        let state_file = dir.path().join(".devloop/state.json");
        let log = SessionLog::create(&state_file).expect("create log");
        let output_task = OutputTask {
            handle: tokio::spawn(async {
                std::future::pending::<()>().await;
            }),
        };

        wait_for_output_task_with_deadline(
            "server".into(),
            output_task,
            Some(log.clone()),
            Duration::from_millis(10),
        )
        .await
        .expect("wait for output task");
        log.flush_queued().await.expect("flush session log");

        assert!(
            std::fs::read_to_string(log.path())
                .expect("read log")
                .contains("[devloop] process output may be truncated")
        );
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
    fn probe_attempt_timeout_is_bounded_by_probe_timeout() {
        assert_eq!(
            probe_attempt_timeout(100, 5000),
            Duration::from_millis(1000)
        );
        assert_eq!(
            probe_attempt_timeout(750, 5000),
            Duration::from_millis(1500)
        );
        assert_eq!(
            probe_attempt_timeout(750, 1200),
            Duration::from_millis(1200)
        );
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

    #[tokio::test]
    async fn persisted_process_output_is_source_labeled_when_terminal_inheritance_is_disabled() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");
        let mut line = Vec::new();
        let mut last_was_carriage_return = false;

        for byte in b"first\r\nsecond" {
            persist_output_byte(
                &log,
                "server example",
                *byte,
                &mut line,
                &mut last_was_carriage_return,
            )
            .expect("persist output byte");
        }
        log.write_labeled_line("server example", &line)
            .expect("persist final output line");
        log.flush_queued().await.expect("flush session log");

        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read log"),
            "[server example] first\n[server example] second\n"
        );
    }

    #[tokio::test]
    async fn hook_output_is_persisted_when_terminal_inheritance_is_disabled() {
        let dir = tempdir().expect("tempdir");
        let mut config = test_config(dir.path());
        config.hook.insert(
            "capture".into(),
            HookSpec {
                command: vec!["sh".into(), "-c".into(), "printf durable-hook".into()],
                cwd: None,
                env: BTreeMap::new(),
                output: HookOutputConfig {
                    inherit: false,
                    body_style: OutputBodyStyle::Dim,
                },
                capture: None,
                state_key: None,
                observe: None,
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        let manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log.clone());

        manager
            .run_hook("capture", &state, &[], "test")
            .await
            .expect("run hook");

        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read log"),
            "[sh capture] durable-hook\n"
        );
    }

    #[tokio::test]
    async fn hook_capture_continues_after_session_log_flush_failure() {
        let dir = tempdir().expect("tempdir");
        let mut config = test_config(dir.path());
        config.hook.insert(
            "capture".into(),
            HookSpec {
                command: vec!["sh".into(), "-c".into(), "printf captured-value".into()],
                cwd: None,
                env: BTreeMap::new(),
                output: HookOutputConfig {
                    inherit: false,
                    body_style: OutputBodyStyle::Dim,
                },
                capture: Some(crate::config::CaptureMode::Text),
                state_key: Some("captured".into()),
                observe: None,
            },
        );
        let state_file = dir.path().join(".devloop/state.json");
        let state = SessionState::load(state_file.clone()).expect("state");
        let log = SessionLog::create(&state_file).expect("create log");
        log.fail_for_test(
            std::io::ErrorKind::BrokenPipe,
            "simulated session log failure",
        );
        let manager = ProcessManager::new(
            &config,
            GuardianExecutable::open().expect("open test guardian"),
        )
        .with_session_log(log);

        manager
            .run_hook("capture", &state, &[], "test")
            .await
            .expect("run hook");

        assert_eq!(
            state
                .get_string("captured")
                .expect("read captured state")
                .as_deref(),
            Some("captured-value")
        );
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
        let _guard = RustLogGuard::set(Some("debug"));

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &BTreeMap::new(),
                external_event_env: None,
                browser_reload_env: None,
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
    }

    #[test]
    fn configure_command_keeps_explicit_rust_log_override() {
        let _guard = RustLogGuard::set(Some("debug"));

        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".into(), "info,gcp_rust_blog=debug".into());

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &env,
                external_event_env: None,
                browser_reload_env: None,
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
    }

    #[test]
    fn configure_command_expands_parent_env_in_command_args_and_env_values() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));
        let mut env = BTreeMap::new();
        env.insert("PORT".into(), "$CONTAINER_PORT".into());

        let command = configure_command(
            &[
                "cloudflared".into(),
                "tunnel".into(),
                "--url".into(),
                "http://127.0.0.1:$CONTAINER_PORT".into(),
            ],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &env,
                external_event_env: None,
                browser_reload_env: None,
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect("configure command");

        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                std::ffi::OsStr::new("tunnel"),
                std::ffi::OsStr::new("--url"),
                std::ffi::OsStr::new("http://127.0.0.1:18080")
            ]
        );
        let port = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("PORT"))
            .and_then(|(_, value)| value)
            .expect("PORT should be set");
        assert_eq!(port, std::ffi::OsStr::new("18080"));
    }

    #[test]
    fn configure_command_reports_missing_env_references() {
        let _guard = EnvVarGuard::set("MISSING_DEVLOOP_TEST_VAR", None);

        let error = configure_command(
            &["echo".into(), "$MISSING_DEVLOOP_TEST_VAR".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &BTreeMap::new(),
                external_event_env: None,
                browser_reload_env: None,
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect_err("missing env should fail");

        assert!(error.to_string().contains("command[1]"));
        assert!(error.to_string().contains("MISSING_DEVLOOP_TEST_VAR"));
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
                browser_reload_env: None,
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

    #[test]
    fn configure_command_injects_browser_reload_environment() {
        let env = BTreeMap::new();
        let browser_reload_env = BrowserReloadEnvironment {
            events_url: "http://127.0.0.1:4455/browser-events".into(),
        };

        let command = configure_command(
            &["cargo".into(), "run".into()],
            PathBuf::from("/tmp"),
            CommandContext {
                env: &env,
                external_event_env: None,
                browser_reload_env: Some(&browser_reload_env),
                root: Path::new("/tmp"),
                state_path: Path::new("/tmp/state.json"),
                changed_files: &[],
                workflow: "startup",
            },
        )
        .expect("configure command");

        let envs = command.as_std().get_envs().collect::<Vec<_>>();
        assert!(envs.iter().any(|(key, value)| {
            *key == std::ffi::OsStr::new("DEVLOOP_BROWSER_EVENTS_URL")
                && *value == Some(std::ffi::OsStr::new("http://127.0.0.1:4455/browser-events"))
        }));
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
    async fn retired_process_output_does_not_update_output_rule_state() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        let rules = vec![CompiledOutputRule {
            regex: Some(Regex::new(r"(https://\S+)").expect("regex")),
            state_key: "url".into(),
            extract: OutputExtract::Regex,
            capture_group: 1,
        }];
        let (mut writer, reader) = tokio::io::duplex(128);
        writer
            .write_all(b"https://stale.example.test\n")
            .await
            .expect("write stale process output");
        drop(writer);

        forward_output_lines(
            reader,
            ForwardOutputConfig {
                output: OutputSink::Stdout(Arc::new(Mutex::new(tokio::io::stdout()))),
                source_label: "server".into(),
                inherit_output: false,
                body_style: OutputBodyStyle::Plain,
                session_log: None,
            },
            "server".into(),
            rules,
            state.clone(),
            OutputStateGeneration {
                current: Arc::new(StdMutex::new(1)),
                value: 0,
            },
        )
        .await;

        assert_eq!(state.get_string("url").expect("get url"), None);

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
    fn expands_http_probe_urls_from_parent_env() {
        let _guard = EnvVarGuard::set("CONTAINER_PORT", Some("18080"));

        let expanded = expand_probe_env(
            "server",
            &ProbeSpec::Http {
                url: "http://127.0.0.1:$CONTAINER_PORT/".into(),
                interval_ms: 100,
                timeout_ms: 1000,
            },
        )
        .expect("expand probe");

        match expanded {
            ProbeSpec::Http { url, .. } => assert_eq!(url, "http://127.0.0.1:18080/"),
            ProbeSpec::StateKey { .. } => panic!("expected http probe"),
        }
    }
}
