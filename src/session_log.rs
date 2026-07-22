use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, oneshot};

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const SESSION_LOG_QUEUE_CAPACITY: usize = 1024;
const SESSION_LOG_BATCH_RECORDS: usize = 64;

/// A durable, run-scoped record of Devloop output.
///
/// The log lives beside the session state so a client can ignore one owned
/// directory while preserving a separate record for each `devloop run`.
#[derive(Clone)]
pub(crate) struct SessionLog {
    path: PathBuf,
    error_state: Arc<Mutex<Option<StoredIoError>>>,
    writer: mpsc::Sender<SessionLogWrite>,
}

enum SessionLogWrite {
    Bytes(Vec<u8>),
    Flush(oneshot::Sender<io::Result<()>>),
}

#[derive(Clone)]
struct StoredIoError {
    kind: io::ErrorKind,
    message: String,
}

impl StoredIoError {
    fn from_error(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl SessionLog {
    pub(crate) fn create(state_file: &Path) -> Result<Self> {
        let state_dir = state_file.parent().ok_or_else(|| {
            anyhow!(
                "state file '{}' has no parent directory",
                state_file.display()
            )
        })?;
        let logs_dir = state_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).with_context(|| {
            format!(
                "failed to create session log directory {}",
                logs_dir.display()
            )
        })?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let process_id = std::process::id();

        for _ in 0..32 {
            let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = logs_dir.join(format!("session-{now}-{process_id}-{sequence}.log"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    let error_state = Arc::new(Mutex::new(None));
                    let (writer, records) = mpsc::channel(SESSION_LOG_QUEUE_CAPACITY);
                    spawn_session_log_writer(file, error_state.clone(), records);
                    return Ok(Self {
                        path,
                        error_state,
                        writer,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create session log {}", path.display())
                    });
                }
            }
        }

        Err(anyhow!(
            "failed to allocate a unique session log in {}",
            logs_dir.display()
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn write_labeled_output(&self, source_label: &str, bytes: &[u8]) -> io::Result<()> {
        let mut line = Vec::new();
        let mut last_was_carriage_return = false;

        for &byte in bytes {
            if byte == b'\r' {
                self.write_labeled_line(source_label, &line)?;
                line.clear();
                last_was_carriage_return = true;
                continue;
            }
            if byte == b'\n' {
                if !last_was_carriage_return {
                    self.write_labeled_line(source_label, &line)?;
                    line.clear();
                }
                last_was_carriage_return = false;
                continue;
            }
            last_was_carriage_return = false;
            line.push(byte);
        }

        if !line.is_empty() {
            self.write_labeled_line(source_label, &line)?;
        }
        Ok(())
    }

    pub(crate) fn write_labeled_line(&self, source_label: &str, bytes: &[u8]) -> io::Result<()> {
        let record = labeled_record(source_label, bytes);
        self.enqueue_ordered(record)
    }

    pub(crate) fn queue_raw(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.enqueue_ordered(bytes)
    }

    fn enqueue_ordered(&self, bytes: Vec<u8>) -> io::Result<()> {
        if let Some(error) = self.stored_error()? {
            return Err(error);
        }
        match self.writer.try_send(SessionLogWrite::Bytes(bytes)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(record)) => self.blocking_send(record),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(io::Error::other("session log writer stopped"))
            }
        }
    }

    pub(crate) async fn queue_labeled_line(
        &self,
        source_label: &str,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        if let Some(error) = self.stored_error()? {
            return Err(error);
        }
        self.writer
            .send(SessionLogWrite::Bytes(labeled_record(source_label, &bytes)))
            .await
            .map_err(|_| io::Error::other("session log writer stopped"))
    }

    pub(crate) async fn flush_queued(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.writer
            .send(SessionLogWrite::Flush(tx))
            .await
            .map_err(|_| io::Error::other("session log writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::other("session log writer stopped"))?
    }

    fn lock_error_state(&self) -> io::Result<std::sync::MutexGuard<'_, Option<StoredIoError>>> {
        self.error_state
            .lock()
            .map_err(|_| io::Error::other("session log error mutex was poisoned"))
    }

    fn blocking_send(&self, record: SessionLogWrite) -> io::Result<()> {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| {
                self.writer
                    .blocking_send(record)
                    .map_err(|_| io::Error::other("session log writer stopped"))
            })
        } else {
            self.writer
                .blocking_send(record)
                .map_err(|_| io::Error::other("session log writer stopped"))
        }
    }

    fn stored_error(&self) -> io::Result<Option<io::Error>> {
        Ok(self
            .lock_error_state()?
            .as_ref()
            .map(|error| error.to_error()))
    }

    #[cfg(test)]
    pub(crate) fn fail_for_test(&self, kind: io::ErrorKind, message: &str) {
        *self.lock_error_state().expect("lock log error state") = Some(StoredIoError {
            kind,
            message: message.into(),
        });
    }
}

fn spawn_session_log_writer(
    file: File,
    error_state: Arc<Mutex<Option<StoredIoError>>>,
    mut records: mpsc::Receiver<SessionLogWrite>,
) {
    std::thread::Builder::new()
        .name("devloop-session-log-writer".into())
        .spawn(move || {
            let mut file = Some(file);
            let mut pending = Vec::new();
            while let Some(record) = records.blocking_recv() {
                match record {
                    SessionLogWrite::Bytes(bytes) => {
                        pending.extend_from_slice(&bytes);
                        drain_available_records(
                            &mut records,
                            &mut file,
                            &error_state,
                            &mut pending,
                        );
                    }
                    SessionLogWrite::Flush(reply) => {
                        let result = flush_pending_records(&mut file, &error_state, &mut pending);
                        let _ = reply.send(result);
                    }
                }
            }
        })
        .expect("session log writer thread must start");
}

fn drain_available_records(
    records: &mut mpsc::Receiver<SessionLogWrite>,
    file: &mut Option<File>,
    error_state: &Arc<Mutex<Option<StoredIoError>>>,
    pending: &mut Vec<u8>,
) {
    for _ in 1..SESSION_LOG_BATCH_RECORDS {
        match records.try_recv() {
            Ok(SessionLogWrite::Bytes(bytes)) => pending.extend_from_slice(&bytes),
            Ok(SessionLogWrite::Flush(reply)) => {
                let result = flush_pending_records(file, error_state, pending);
                let _ = reply.send(result);
                return;
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    let _ = flush_pending_records(file, error_state, pending);
}

fn flush_pending_records(
    file: &mut Option<File>,
    error_state: &Arc<Mutex<Option<StoredIoError>>>,
    pending: &mut Vec<u8>,
) -> io::Result<()> {
    let had_error = error_state
        .lock()
        .map_err(|_| io::Error::other("session log error mutex was poisoned"))?
        .is_some();
    let result = write_batch_to_file(file, error_state, pending);
    if let Err(error) = &result
        && !had_error
    {
        eprintln!("devloop: failed to persist session log output: {error}");
    }
    pending.clear();
    result
}

fn labeled_record(source_label: &str, bytes: &[u8]) -> Vec<u8> {
    let mut record = Vec::new();
    let prefix = format!("[{source_label}] ");
    record.extend_from_slice(prefix.as_bytes());
    record.extend_from_slice(bytes);
    record.push(b'\n');
    record
}

fn write_batch_to_file(
    file: &mut Option<File>,
    error_state: &Arc<Mutex<Option<StoredIoError>>>,
    bytes: &[u8],
) -> io::Result<()> {
    if let Some(error) = error_state
        .lock()
        .map_err(|_| io::Error::other("session log error mutex was poisoned"))?
        .as_ref()
        .map(|error| error.to_error())
    {
        return Err(error);
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let Some(writer) = file.as_mut() else {
        return Err(io::Error::other("session log file is unavailable"));
    };
    if let Err(error) = writer.write_all(bytes).and_then(|_| writer.flush()) {
        let stored = StoredIoError::from_error(error);
        *error_state
            .lock()
            .map_err(|_| io::Error::other("session log error mutex was poisoned"))? =
            Some(stored.clone());
        *file = None;
        return Err(stored.to_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SessionLog;
    use std::fs;
    use std::io;
    use tempfile::tempdir;

    #[test]
    fn creates_a_unique_log_beside_session_state() {
        let dir = tempdir().expect("tempdir");
        let state_file = dir.path().join(".devloop").join("state.json");
        let first = SessionLog::create(&state_file).expect("create first log");
        let second = SessionLog::create(&state_file).expect("create second log");

        assert_ne!(first.path(), second.path());
        assert_eq!(
            first.path().parent(),
            Some(dir.path().join(".devloop/logs").as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn creates_session_log_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");

        let mode = fs::metadata(log.path())
            .expect("read log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn labels_each_persisted_output_line() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");

        log.write_labeled_output("echo server", b"first\r\nsecond\nthird")
            .expect("write output");
        log.flush_queued().await.expect("flush log");

        assert_eq!(
            fs::read_to_string(log.path()).expect("read log"),
            "[echo server] first\n[echo server] second\n[echo server] third\n"
        );
    }

    #[tokio::test]
    async fn flush_reports_prior_writer_failure() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");
        {
            log.fail_for_test(io::ErrorKind::BrokenPipe, "simulated write failure");
        }

        let error = log.flush_queued().await.expect_err("flush must fail");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("simulated write failure"));
    }

    #[tokio::test]
    async fn queue_reports_prior_writer_failure() {
        let dir = tempdir().expect("tempdir");
        let log = SessionLog::create(&dir.path().join(".devloop/state.json")).expect("create log");
        {
            log.fail_for_test(io::ErrorKind::BrokenPipe, "simulated write failure");
        }

        let error = log
            .queue_labeled_line("server", b"lost line".to_vec())
            .await
            .expect_err("queue must fail");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("simulated write failure"));
    }
}
