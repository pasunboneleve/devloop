use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) struct EnvVarGuard {
    _lock: MutexGuard<'static, ()>,
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: Option<&str>) -> Self {
        let lock = env_lock().lock().expect("lock test env mutex");
        let original = std::env::var_os(key);
        match value {
            Some(value) => set_test_env_var(key, value),
            None => remove_test_env_var(key),
        }
        Self {
            _lock: lock,
            key,
            original,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => set_test_env_var(self.key, value),
            None => remove_test_env_var(self.key),
        }
    }
}

pub(crate) struct RustLogGuard {
    _guard: EnvVarGuard,
}

impl RustLogGuard {
    pub(crate) fn set(value: Option<&str>) -> Self {
        Self {
            _guard: EnvVarGuard::set("RUST_LOG", value),
        }
    }
}

fn set_test_env_var(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: all test-time environment mutation goes through the shared
    // `env_lock`, so no concurrent unit test can race on this
    // process-global environment state.
    unsafe {
        std::env::set_var(key, value);
    }
}

fn remove_test_env_var(key: &str) {
    // SAFETY: all test-time environment mutation goes through the shared
    // `env_lock`, so removing the variable cannot race with another
    // unit test in this process.
    unsafe {
        std::env::remove_var(key);
    }
}
