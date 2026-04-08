use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn rust_log_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) struct RustLogGuard {
    _lock: MutexGuard<'static, ()>,
    original: Option<OsString>,
}

impl RustLogGuard {
    pub(crate) fn set(value: Option<&str>) -> Self {
        let lock = rust_log_lock().lock().expect("lock RUST_LOG test mutex");
        let original = std::env::var_os("RUST_LOG");
        match value {
            Some(value) => set_test_env_var("RUST_LOG", value),
            None => remove_test_env_var("RUST_LOG"),
        }
        Self {
            _lock: lock,
            original,
        }
    }
}

impl Drop for RustLogGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => set_test_env_var("RUST_LOG", value),
            None => remove_test_env_var("RUST_LOG"),
        }
    }
}

fn set_test_env_var(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: all test-time RUST_LOG mutation goes through the shared
    // `rust_log_lock`, so no concurrent unit test can race on this
    // process-global environment state.
    unsafe {
        std::env::set_var(key, value);
    }
}

fn remove_test_env_var(key: &str) {
    // SAFETY: all test-time RUST_LOG mutation goes through the shared
    // `rust_log_lock`, so removing the variable cannot race with another
    // unit test in this process.
    unsafe {
        std::env::remove_var(key);
    }
}
