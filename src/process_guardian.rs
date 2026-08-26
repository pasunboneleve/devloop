use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};

pub const CONTROL_FD: i32 = 3;
pub const EXECUTABLE_FD_MINIMUM: i32 = 10;
const STARTED_MESSAGE: u8 = 1;
const START_FAILED_MESSAGE: u8 = 2;
const MAX_START_ERROR_BYTES: usize = 16 * 1024;

/// Holds the exact companion image used for every guardian in one devloop run.
///
/// Keeping the file open pins its inode across in-place upgrades. Each spawn
/// executes a duplicated descriptor, so later hooks and restarts cannot switch
/// to a different guardian protocol while the supervisor is still running.
#[derive(Clone)]
pub struct GuardianExecutable {
    image: Arc<File>,
}

impl GuardianExecutable {
    pub fn open() -> Result<Self> {
        let devloop = std::env::current_exe().context("failed to resolve devloop executable")?;
        let directory = executable_directory(&devloop)?;
        let path = directory.join(format!(
            "devloop-process-guardian{}",
            std::env::consts::EXE_SUFFIX
        ));
        Self::open_path(&path)
    }

    fn open_path(path: &Path) -> Result<Self> {
        let image = File::open(path)
            .with_context(|| format!("failed to open process guardian at {}", path.display()))?;
        Ok(Self {
            image: Arc::new(image),
        })
    }

    pub fn duplicate_for_exec(&self) -> Result<(PathBuf, OwnedFd)> {
        // SAFETY: fcntl duplicates the live companion descriptor without
        // borrowing memory. The returned descriptor is immediately owned.
        let raw_fd = unsafe {
            libc::fcntl(
                self.image.as_raw_fd(),
                libc::F_DUPFD_CLOEXEC,
                EXECUTABLE_FD_MINIMUM,
            )
        };
        if raw_fd == -1 {
            return Err(io::Error::last_os_error())
                .context("failed to duplicate process guardian executable");
        }
        // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor owned by this call.
        let descriptor = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        #[cfg(target_os = "linux")]
        let path = PathBuf::from(format!("/proc/self/fd/{raw_fd}"));
        #[cfg(not(target_os = "linux"))]
        let path = PathBuf::from(format!("/dev/fd/{raw_fd}"));
        Ok((path, descriptor))
    }
}

fn executable_directory(executable: &Path) -> Result<&Path> {
    let parent = executable
        .parent()
        .ok_or_else(|| anyhow!("devloop executable has no parent directory"))?;
    if parent.file_name().is_some_and(|name| name == "deps") {
        return parent
            .parent()
            .ok_or_else(|| anyhow!("Cargo test executable has no target profile directory"));
    }
    Ok(parent)
}

pub fn append_invocation(command: &mut tokio::process::Command, target: &std::process::Command) {
    command.arg(target.get_program()).args(target.get_args());
}

pub fn receive_process_group(control: &mut UnixStream) -> Result<Pid> {
    let mut message = [0_u8; 1];
    control.read_exact(&mut message)?;
    if message[0] == START_FAILED_MESSAGE {
        return Err(anyhow!(receive_start_error(control)?));
    }
    if message[0] != STARTED_MESSAGE {
        return Err(anyhow!(
            "guardian announced unknown startup message {}",
            message[0]
        ));
    }
    let mut encoded = [0_u8; size_of::<u32>()];
    control.read_exact(&mut encoded)?;
    let raw_pid = u32::from_ne_bytes(encoded);
    Ok(Pid::from_raw(raw_pid as i32).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("guardian announced invalid process group id {raw_pid}"),
        )
    })?)
}

/// Runs one target process group until the target exits or devloop disappears.
pub fn run_and_exit(command: Vec<OsString>) -> Result<()> {
    ignore_guardian_signals()?;
    let status = run(command)?;
    std::process::exit(exit_code(status));
}

fn run(command: Vec<OsString>) -> Result<ExitStatus> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("internal process guardian requires a target command"))?;
    let mut control = take_control_stream()?;
    set_close_on_exec(control.as_raw_fd())?;

    let mut target = Command::new(program);
    target.args(args);
    target.process_group(0);
    // SAFETY: the guardian ignores terminal-oriented signals so bulk shutdown
    // cannot remove the reaper. The target must restore ordinary dispositions
    // before exec rather than inheriting those guardian-only semantics.
    unsafe {
        target.pre_exec(restore_target_signals);
    }
    let mut child = match target.spawn() {
        Ok(child) => child,
        Err(source) => {
            let error =
                anyhow!(source).context(format!("failed to start guarded command {program:?}"));
            announce_start_error(&mut control, &format!("{error:#}"))?;
            return Err(error);
        }
    };
    let process_group = Pid::from_raw(child.id() as i32)
        .ok_or_else(|| anyhow!("guarded command has invalid process group id"))?;
    let monitor_control = match control.try_clone() {
        Ok(control) => control,
        Err(error) => {
            terminate_after_guardian_failure(&mut child, process_group);
            return Err(error).context("failed to clone guardian control socket");
        }
    };
    let monitor = match thread::Builder::new()
        .name("devloop-guardian".into())
        .spawn(move || monitor_control_channel(monitor_control, process_group))
    {
        Ok(monitor) => monitor,
        Err(error) => {
            terminate_after_guardian_failure(&mut child, process_group);
            return Err(error).context("failed to start guardian control monitor");
        }
    };

    let announcement = control
        .write_all(&[STARTED_MESSAGE])
        .and_then(|()| control.write_all(&child.id().to_ne_bytes()))
        .context("failed to announce guarded process group");
    if announcement.is_err() {
        let _ = control.shutdown(std::net::Shutdown::Both);
    }
    let status = child.wait().context("failed to wait for guarded command");
    let _ = control.shutdown(std::net::Shutdown::Both);
    let monitor_result = monitor
        .join()
        .map_err(|_| anyhow!("guardian control monitor panicked"))?;
    announcement?;
    monitor_result?;
    status
}

fn ignore_guardian_signals() -> Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        set_signal_disposition(signal, libc::SIG_IGN)?;
    }
    Ok(())
}

fn restore_target_signals() -> io::Result<()> {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: pre-exec signal restoration uses only libc::signal and a
        // constant disposition; it does not allocate or retain Rust memory.
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_signal_disposition(signal: i32, disposition: libc::sighandler_t) -> Result<()> {
    // SAFETY: signal installs a constant disposition for one supported signal;
    // no Rust callback or borrowed memory crosses the FFI boundary.
    if unsafe { libc::signal(signal, disposition) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to set guardian signal disposition for {signal}"));
    }
    Ok(())
}

fn announce_start_error(control: &mut UnixStream, message: &str) -> Result<()> {
    let bytes = message.as_bytes();
    let bytes = &bytes[..bytes.len().min(MAX_START_ERROR_BYTES)];
    control
        .write_all(&[START_FAILED_MESSAGE])
        .and_then(|()| control.write_all(&(bytes.len() as u32).to_ne_bytes()))
        .and_then(|()| control.write_all(bytes))
        .context("failed to announce guarded command startup failure")
}

fn receive_start_error(control: &mut UnixStream) -> io::Result<String> {
    let mut encoded_length = [0_u8; size_of::<u32>()];
    control.read_exact(&mut encoded_length)?;
    let length = u32::from_ne_bytes(encoded_length) as usize;
    if length > MAX_START_ERROR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("guardian startup error exceeds {MAX_START_ERROR_BYTES} bytes"),
        ));
    }
    let mut message = vec![0_u8; length];
    control.read_exact(&mut message)?;
    Ok(String::from_utf8_lossy(&message).into_owned())
}

fn take_control_stream() -> Result<UnixStream> {
    let mut socket_type = 0;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt only inspects CONTROL_FD and writes within the provided
    // c_int buffer. It validates the descriptor before ownership is assumed.
    let result = unsafe {
        libc::getsockopt(
            CONTROL_FD,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(socket_type).cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error()).context("guardian control descriptor is invalid");
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(anyhow!(
            "guardian control descriptor is not a stream socket"
        ));
    }
    // SAFETY: devloop maps one owned Unix stream socket to CONTROL_FD before
    // exec. The successful SO_TYPE check above establishes descriptor validity.
    Ok(unsafe { UnixStream::from_raw_fd(CONTROL_FD) })
}

fn terminate_after_guardian_failure(child: &mut std::process::Child, process_group: Pid) {
    let _ = signal_group(process_group, Signal::KILL);
    let _ = child.wait();
}

fn monitor_control_channel(mut control: UnixStream, process_group: Pid) -> Result<()> {
    let mut unexpected = [0_u8; 1];
    loop {
        match control.read(&mut unexpected) {
            Ok(0) => {
                signal_group(process_group, Signal::KILL)?;
                return Ok(());
            }
            Ok(_) => {
                signal_group(process_group, Signal::KILL)?;
                return Err(anyhow!("guardian control channel received unexpected data"));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                signal_group(process_group, Signal::KILL)?;
                return Ok(());
            }
        }
    }
}

fn signal_group(process_group: Pid, signal: Signal) -> Result<()> {
    match kill_process_group(process_group, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(anyhow!(
            "failed to send {signal:?} to guarded process group: {error}"
        )),
    }
}

fn set_close_on_exec(fd: i32) -> Result<()> {
    // SAFETY: fd is the live control socket owned by this process. fcntl does
    // not retain the pointer-free arguments and is safe before spawning target.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error())
                .context("failed to protect guardian control socket from target inheritance");
        }
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
        .clamp(0, 255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn exit_code_preserves_success() {
        let status = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .status()
            .expect("run status fixture");
        assert_eq!(exit_code(status), 0);
    }

    #[test]
    fn pinned_executable_keeps_original_inode_after_path_replacement() {
        let directory = tempfile::tempdir().expect("create executable fixture directory");
        let path = directory.path().join("guardian");
        let replacement = directory.path().join("replacement");
        std::fs::copy("/usr/bin/true", &path).expect("copy original executable");
        std::fs::copy("/usr/bin/false", &replacement).expect("copy replacement executable");
        let executable = GuardianExecutable::open_path(&path).expect("pin original executable");
        std::fs::rename(&replacement, &path).expect("replace executable path");
        let (descriptor_path, descriptor) = executable
            .duplicate_for_exec()
            .expect("duplicate pinned executable");
        let descriptor_fd = descriptor.as_raw_fd();
        let mut command = Command::new(descriptor_path);
        // SAFETY: the closure changes only the close-on-exec flag of the owned
        // fixture descriptor before exec.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(descriptor_fd, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(descriptor_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let status = command.status().expect("execute pinned original inode");
        drop(descriptor);

        assert!(status.success());
    }
}
