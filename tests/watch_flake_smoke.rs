use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

#[test]
fn repeated_literal_file_edits_keep_triggering_native_watch_workflow() {
    let fixture = WatchFixture::new();
    let mut child = DevloopChild::spawn(&fixture);

    child.wait_for_log_line("startup value: initial", Duration::from_secs(10));
    child.wait_for_log_line("watching ", Duration::from_secs(10));

    for write_index in 1..=10 {
        let value = format!("native-trial-{}", "x".repeat(write_index));
        fixture.write_value(&value);
        child.wait_for_log_line(&format!("changed value: {value}"), Duration::from_secs(10));
    }
}

struct WatchFixture {
    dir: TempDir,
}

impl WatchFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let fixture = Self { dir };
        fixture.write("watched.txt", "initial\n");
        fixture.write(
            "devloop.toml",
            r#"root = "."
debounce_ms = 300
state_file = "./.devloop/state.json"
startup_workflows = ["startup"]

[watch.content]
paths = ["watched.txt"]
workflow = "content"

[hook.current_value]
command = ["sed", "-n", "1p", "watched.txt"]
cwd = "."
capture = "text"
state_key = "current_value"
output = { inherit = false }

[workflow.startup]
steps = [
  { action = "run_hook", hook = "current_value" },
  { action = "log", message = "startup value: {{current_value}}" },
]

[workflow.content]
steps = [
  { action = "run_hook", hook = "current_value" },
  { action = "log", message = "changed value: {{current_value}}" },
]
"#,
        );
        fixture
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.dir.path().join("devloop.toml")
    }

    fn write_value(&self, value: &str) {
        self.write("watched.txt", &format!("{value}\n"));
    }

    fn write(&self, relative_path: &str, contents: &str) {
        let path = self.dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent directories");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }
}

struct DevloopChild {
    child: Child,
    lines: Receiver<String>,
    history: Arc<Mutex<Vec<String>>>,
}

impl DevloopChild {
    fn spawn(fixture: &WatchFixture) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devloop"));
        command
            .arg("run")
            .arg("--config")
            .arg(fixture.config_path())
            .current_dir(fixture.dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn devloop");
        let stderr = child.stderr.take().expect("take child stderr");
        let (tx, rx) = mpsc::channel();
        let history = Arc::new(Mutex::new(Vec::new()));
        let history_writer = Arc::clone(&history);
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        history_writer
                            .lock()
                            .expect("lock log history")
                            .push(line.clone());
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            child,
            lines: rx,
            history,
        }
    }

    fn wait_for_log_line(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| Duration::from_secs(0));
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|_| {
                let history = self.history.lock().expect("lock log history");
                panic!(
                    "timed out waiting for log line containing '{needle}'. recent logs: {history:?}"
                );
            });
            if line.contains(needle) {
                return;
            }
        }
    }
}

impl Drop for DevloopChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
