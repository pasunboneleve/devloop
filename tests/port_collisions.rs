#![cfg(unix)]

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

#[test]
#[ignore]
fn serve_ephemeral_port() {
    if std::env::var_os("DEVLOOP_TEST_SERVER_MODE").is_none() {
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral test server");
    println!(
        "listening {}",
        listener.local_addr().expect("read server address").port()
    );
    std::io::stdout().flush().expect("flush server address");
    loop {
        let _ = listener.accept().expect("accept test connection");
    }
}

#[test]
fn separate_sessions_can_run_on_distinct_ports() {
    let first = SessionFixture::ephemeral();
    let second = SessionFixture::ephemeral();

    let mut first_session = DevloopChild::spawn(&first);
    let mut second_session = DevloopChild::spawn(&second);

    let first_address = wait_for_reported_listener(&first, &mut first_session.child);
    let second_address = wait_for_reported_listener(&second, &mut second_session.child);
    assert_ne!(first_address, second_address);
    first_session.assert_running();
    second_session.assert_running();
}

#[test]
fn startup_fails_once_when_a_managed_address_is_occupied() {
    let incumbent = TcpListener::bind("127.0.0.1:0").expect("bind incumbent listener");
    assert_startup_collision(&incumbent);
}

#[test]
fn startup_detects_an_occupied_ipv6_loopback_address() {
    let Ok(incumbent) = TcpListener::bind("[::1]:0") else {
        return;
    };
    assert_startup_collision(&incumbent);
}

fn assert_startup_collision(incumbent: &TcpListener) {
    let address = incumbent.local_addr().expect("read incumbent address");
    let fixture = SessionFixture::new(address);

    let output = Command::new(env!("CARGO_BIN_EXE_devloop"))
        .arg("run")
        .arg("--config")
        .arg(fixture.config_path())
        .current_dir(fixture.path())
        .env("RUST_LOG", "off")
        .output()
        .expect("run colliding devloop session");

    assert!(!output.status.success(), "collision must exit non-zero");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let diagnostic = format!("process 'server' cannot start: address {address} is already in use");
    assert_eq!(
        stderr.matches(&diagnostic).count(),
        1,
        "collision must produce one concise diagnostic; stderr: {stderr}"
    );
    assert!(!stderr.contains("started process server"), "{stderr}");
    TcpStream::connect(address).expect("incumbent listener remains reachable");
}

fn wait_for_reported_listener(fixture: &SessionFixture, child: &mut Child) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(raw_state) = std::fs::read_to_string(fixture.state_path())
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&raw_state)
            && let Some(port) = state
                .get("server_port")
                .and_then(serde_json::Value::as_str)
                .and_then(|port| port.parse::<u16>().ok())
        {
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            if TcpStream::connect(address).is_ok() {
                return address;
            }
        }
        if let Some(status) = child.try_wait().expect("read devloop status") {
            panic!("devloop exited before its ephemeral port became ready: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for devloop's reported listener"
        );
        std::thread::yield_now();
    }
}

struct SessionFixture {
    dir: TempDir,
}

impl SessionFixture {
    fn new(address: SocketAddr) -> Self {
        let dir = tempfile::tempdir().expect("create session fixture");
        let fixture = Self { dir };
        let config = format!(
            r#"root = "."
state_file = "./.devloop/state.json"
startup_workflows = ["startup"]

[watch.config]
paths = ["devloop.toml"]
workflow = "startup"

[process.server]
command = ["devloop-test-command-that-must-not-run"]
autostart = false
readiness = {{ kind = "http", url = "http://{address}/", interval_ms = 20, timeout_ms = 5000 }}
restart = "always"

[workflow.startup]
steps = [
  {{ action = "start_process", process = "server" }},
  {{ action = "wait_for_process", process = "server" }},
]
"#,
        );
        std::fs::write(fixture.config_path(), config).expect("write session config");
        fixture
    }

    fn ephemeral() -> Self {
        let dir = tempfile::tempdir().expect("create session fixture");
        let fixture = Self { dir };
        let test_binary = std::env::current_exe().expect("resolve integration test executable");
        let config = format!(
            r#"root = "."
state_file = "./.devloop/state.json"
startup_workflows = ["startup"]

[watch.config]
paths = ["devloop.toml"]
workflow = "startup"

[process.server]
command = ["{test_binary}", "--exact", "serve_ephemeral_port", "--ignored", "--nocapture"]
autostart = false
readiness = {{ kind = "state_key", key = "server_port", interval_ms = 20, timeout_ms = 5000 }}
restart = "always"
env = {{ DEVLOOP_TEST_SERVER_MODE = "1" }}
output = {{ inherit = false, rules = [{{ state_key = "server_port", pattern = "^listening ([0-9]+)$", extract = "regex", capture_group = 1 }}] }}

[workflow.startup]
steps = [
  {{ action = "start_process", process = "server" }},
  {{ action = "wait_for_process", process = "server" }},
]
"#,
            test_binary = test_binary.display()
        );
        std::fs::write(fixture.config_path(), config).expect("write session config");
        fixture
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.path().join("devloop.toml")
    }

    fn state_path(&self) -> std::path::PathBuf {
        self.path().join(".devloop/state.json")
    }
}

struct DevloopChild {
    child: Child,
}

impl DevloopChild {
    fn spawn(fixture: &SessionFixture) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_devloop"))
            .arg("run")
            .arg("--config")
            .arg(fixture.config_path())
            .current_dir(fixture.path())
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn devloop session");
        Self { child }
    }

    fn assert_running(&mut self) {
        assert!(
            self.child
                .try_wait()
                .expect("read devloop status")
                .is_none(),
            "devloop session exited"
        );
    }
}

impl Drop for DevloopChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let pid = Pid::from_raw(self.child.id() as i32).expect("devloop pid");
            let _ = kill_process(pid, Signal::INT);
        }
        let _ = self.child.wait();
    }
}
