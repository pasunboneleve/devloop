use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

#[test]
fn run_persists_engine_and_hidden_hook_output_in_a_session_log() {
    let fixture = SessionLogFixture::new();
    let mut child = DevloopChild::spawn(&fixture);

    child.wait_for_stderr("startup durable log", Duration::from_secs(10));
    fixture.wait_for_single_session_log_containing("startup durable log", Duration::from_secs(10));

    let logs_dir = fixture.path().join(".devloop/logs");
    let entries = std::fs::read_dir(&logs_dir)
        .expect("read session logs")
        .collect::<Result<Vec<_>, _>>()
        .expect("read session log entries");
    assert_eq!(entries.len(), 1, "one log per devloop run");

    let content = std::fs::read_to_string(entries[0].path()).expect("read session log");
    assert!(content.contains("writing session log"));
    assert!(content.contains("startup durable log"));
    assert!(content.contains("[sh hidden] durable-hook"));
}

#[test]
fn session_log_path_report_ignores_rust_log_filter() {
    let fixture = SessionLogFixture::new();
    let mut child = DevloopChild::spawn_with_env(&fixture, &[("RUST_LOG", "warn")]);

    child.wait_for_stderr("writing session log:", Duration::from_secs(10));
    fixture.wait_for_single_session_log_containing(
        "[devloop] writing session log:",
        Duration::from_secs(10),
    );

    let content = fixture.read_single_session_log();
    assert!(content.contains("[devloop] writing session log:"));
}

#[test]
fn runtime_start_failure_is_persisted_in_the_session_log() {
    let fixture = SessionLogFixture::new();
    fixture.write_invalid_state_file();
    let mut child = DevloopChild::spawn(&fixture);

    child.wait_for_stderr("devloop run failed", Duration::from_secs(10));
    assert!(!child.wait_for_exit().success());

    let content = fixture.read_single_session_log();
    assert!(content.contains("devloop run failed"));
    assert!(content.contains("failed to parse state file"));
}

struct SessionLogFixture {
    dir: TempDir,
}

impl SessionLogFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let fixture = Self { dir };
        std::fs::write(
            fixture.path().join("devloop.toml"),
            r#"root = "."
startup_workflows = ["startup"]

[watch.config]
paths = ["devloop.toml"]
workflow = "startup"

[hook.hidden]
command = ["sh", "-c", "printf durable-hook"]
output = { inherit = false }

[workflow.startup]
steps = [
  { action = "run_hook", hook = "hidden" },
  { action = "log", message = "startup durable log" },
]
"#,
        )
        .expect("write fixture config");
        fixture
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn read_single_session_log(&self) -> String {
        let logs_dir = self.path().join(".devloop/logs");
        let entries = std::fs::read_dir(&logs_dir)
            .expect("read session logs")
            .collect::<Result<Vec<_>, _>>()
            .expect("read session log entries");
        assert_eq!(entries.len(), 1, "one log per devloop run");
        std::fs::read_to_string(entries[0].path()).expect("read session log")
    }

    fn wait_for_single_session_log_containing(&self, needle: &str, timeout: Duration) {
        let started = std::time::Instant::now();
        loop {
            if let Ok(content) = std::panic::catch_unwind(|| self.read_single_session_log())
                && content.contains(needle)
            {
                return;
            }
            assert!(
                started.elapsed() < timeout,
                "timed out waiting for session log containing '{needle}'"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn write_invalid_state_file(&self) {
        let devloop_dir = self.path().join(".devloop");
        std::fs::create_dir_all(&devloop_dir).expect("create .devloop directory");
        std::fs::write(devloop_dir.join("state.json"), "not-json").expect("write invalid state");
    }
}

struct DevloopChild {
    child: Child,
    stderr: Receiver<String>,
}

impl DevloopChild {
    fn spawn(fixture: &SessionLogFixture) -> Self {
        Self::spawn_with_env(fixture, &[])
    }

    fn spawn_with_env(fixture: &SessionLogFixture, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devloop"));
        command
            .arg("run")
            .arg("--config")
            .arg(fixture.path().join("devloop.toml"))
            .current_dir(fixture.path())
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("spawn devloop");
        let stderr = child.stderr.take().expect("take devloop stderr");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Self { child, stderr: rx }
    }

    fn wait_for_stderr(&mut self, needle: &str, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for stderr containing '{needle}'"
            );
            let line = self
                .stderr
                .recv_timeout(deadline - now)
                .unwrap_or_else(|_| panic!("timed out waiting for stderr containing '{needle}'"));
            if line.contains(needle) {
                return;
            }
        }
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait for devloop")
    }
}

impl Drop for DevloopChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
