#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, getpgid, kill_process, test_kill_process};
use tempfile::TempDir;

#[test]
fn abrupt_supervisor_death_kills_the_complete_managed_process_tree() {
    assert_abrupt_supervisor_death_cleans_tree(TreeLaunch::ManagedProcess);
}

#[test]
fn abrupt_supervisor_death_kills_the_complete_hook_process_tree() {
    assert_abrupt_supervisor_death_cleans_tree(TreeLaunch::Hook);
}

fn assert_abrupt_supervisor_death_cleans_tree(launch: TreeLaunch) {
    let fixture = ProcessTreeFixture::new(launch);
    let mut devloop = DevloopChild::spawn(&fixture);
    let parent = fixture.wait_for_pid("parent.pid", Duration::from_secs(10));
    let child = fixture.wait_for_pid("child.pid", Duration::from_secs(10));
    let grandchild = fixture.wait_for_pid("grandchild.pid", Duration::from_secs(10));
    let guardian = assert_target_group(&devloop, parent, child, grandchild);
    assert_guardian_identity_and_signal_resilience(guardian);

    devloop.kill_supervisor();

    assert_process_gone(guardian);
    assert_process_gone(parent);
    assert_process_gone(child);
    assert_process_gone(grandchild);
}

fn assert_target_group(devloop: &DevloopChild, parent: i32, child: i32, grandchild: i32) -> i32 {
    let parent = Pid::from_raw(parent).expect("parent pid");
    let child = Pid::from_raw(child).expect("child pid");
    let grandchild = Pid::from_raw(grandchild).expect("grandchild pid");
    let devloop = Pid::from_raw(devloop.child.id() as i32).expect("devloop pid");

    assert_eq!(getpgid(Some(parent)).expect("parent process group"), parent);
    assert_eq!(getpgid(Some(child)).expect("child process group"), parent);
    assert_eq!(
        getpgid(Some(grandchild)).expect("grandchild process group"),
        parent
    );
    assert_ne!(
        getpgid(Some(devloop)).expect("devloop process group"),
        parent
    );
    process_parent(parent.as_raw_nonzero().get())
}

fn assert_guardian_identity_and_signal_resilience(raw_pid: i32) {
    let guardian = Pid::from_raw(raw_pid).expect("guardian pid");
    let process_name = process_field(raw_pid, "comm");
    let executable_name = std::path::Path::new(process_name.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(process_name.trim());
    assert_ne!(
        executable_name, "devloop",
        "guardian must have a distinct name"
    );

    kill_process(guardian, Signal::TERM).expect("send SIGTERM to guardian");
    assert!(
        test_kill_process(guardian).is_ok(),
        "guardian must survive name-oriented termination signals"
    );
}

fn process_parent(raw_pid: i32) -> i32 {
    process_field(raw_pid, "ppid")
        .trim()
        .parse()
        .expect("parse guardian pid")
}

fn process_field(raw_pid: i32, field: &str) -> String {
    let output = Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &raw_pid.to_string()])
        .output()
        .expect("inspect process");
    assert!(output.status.success(), "ps failed for process {raw_pid}");
    String::from_utf8(output.stdout).expect("ps output is UTF-8")
}

#[derive(Clone, Copy)]
enum TreeLaunch {
    ManagedProcess,
    Hook,
}

struct ProcessTreeFixture {
    dir: TempDir,
}

impl ProcessTreeFixture {
    fn new(launch: TreeLaunch) -> Self {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let fixture = Self { dir };
        let config = match launch {
            TreeLaunch::ManagedProcess => {
                r#"root = "."
state_file = "./.devloop/state.json"

[watch.config]
paths = ["devloop.toml"]
workflow = "noop"

[process.tree]
command = ["sh", "parent.sh"]
autostart = true
restart = "never"

[workflow.noop]
steps = [{ action = "log", message = "configuration changed" }]
"#
            }
            TreeLaunch::Hook => {
                r#"root = "."
state_file = "./.devloop/state.json"
startup_workflows = ["start"]

[watch.config]
paths = ["devloop.toml"]
workflow = "noop"

[hook.tree]
command = ["sh", "parent.sh"]

[workflow.start]
steps = [{ action = "run_hook", hook = "tree" }]

[workflow.noop]
steps = [{ action = "log", message = "configuration changed" }]
"#
            }
        };
        fixture.write("devloop.toml", config);
        fixture.write(
            "parent.sh",
            r#"#!/bin/sh
set -eu

printf '%s\n' "$$" > parent.pid
sh child.sh &
wait "$!"
"#,
        );
        fixture.write(
            "child.sh",
            r#"#!/bin/sh
set -eu

printf '%s\n' "$$" > child.pid
sh grandchild.sh &
wait "$!"
"#,
        );
        fixture.write(
            "grandchild.sh",
            r#"#!/bin/sh
set -eu

trap '' TERM
printf '%s\n' "$$" > grandchild.pid
while :; do
  sleep 60
done
"#,
        );
        fixture
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).expect("write fixture file");
    }

    fn wait_for_pid(&self, name: &str, timeout: Duration) -> i32 {
        let path = self.path().join(name);
        let deadline = Instant::now() + timeout;
        loop {
            match std::fs::read_to_string(&path) {
                Ok(raw_pid) => {
                    if let Ok(pid) = raw_pid.trim().parse() {
                        return pid;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to read {}: {error}", path.display()),
            }
            assert!(Instant::now() < deadline, "timed out waiting for {name}");
            thread::yield_now();
        }
    }
}

struct DevloopChild {
    child: Child,
}

impl DevloopChild {
    fn spawn(fixture: &ProcessTreeFixture) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_devloop"))
            .arg("run")
            .arg("--config")
            .arg(fixture.path().join("devloop.toml"))
            .current_dir(fixture.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn devloop");
        Self { child }
    }

    fn kill_supervisor(&mut self) {
        let pid = Pid::from_raw(self.child.id() as i32).expect("devloop pid");
        kill_process(pid, Signal::KILL).expect("kill devloop supervisor");
        let status = self.child.wait().expect("wait for killed devloop");
        assert!(!status.success(), "SIGKILL should not report success");
    }
}

impl Drop for DevloopChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_process_gone(raw_pid: i32) {
    let pid = Pid::from_raw(raw_pid).expect("managed process pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(test_kill_process(pid), Err(Errno::SRCH)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "managed descendant {raw_pid} survived devloop"
        );
        thread::yield_now();
    }
}
