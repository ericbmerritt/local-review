//! Shared `#[cfg(test)]` helpers for crate-internal tests.

#![cfg(test)]

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serialize tests that mutate process-wide environment variables. Rust's
/// test runner runs tests in parallel by default, and `std::env::set_var` is
/// not thread-safe; concurrent mutations from different tests can race and
/// produce flaky results. Tests that touch `JJR_CONFIG_PATH`, `XDG_CONFIG_HOME`,
/// or `HOME` should hold this lock for the lifetime of their env mutations.
///
/// Mirrors the pattern in `stderr_log::tests::FD_MUTEX`.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the env-mutation mutex, recovering from poisoning. Hold the
/// returned guard for the duration of any [`EnvGuard`] lifetime.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    ENV_MUTEX.lock().unwrap_or_else(PoisonError::into_inner)
}

/// RAII guard that sets an environment variable on construction and restores
/// the previous value (or unsets it) on drop. Tests must hold the
/// [`env_lock`] mutex for the full lifetime of any `EnvGuard`.
pub(crate) struct EnvGuard {
    key: OsString,
    prev: Option<OsString>,
}

impl EnvGuard {
    pub(crate) fn set_path(key: &str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key: OsString::from(key),
            prev,
        }
    }

    pub(crate) fn set_str(key: &str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key: OsString::from(key),
            prev,
        }
    }

    pub(crate) fn unset(key: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self {
            key: OsString::from(key),
            prev,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

/// Write `contents` to `<dir>/jjr/config.toml`, creating the directory if
/// needed, and return the full path. Callers typically point
/// `JJR_CONFIG_PATH` (or `XDG_CONFIG_HOME`) at the result via [`EnvGuard`].
pub(crate) fn write_global_config_at(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
    let cfg_dir = dir.join("jjr");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let path = cfg_dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    path
}
