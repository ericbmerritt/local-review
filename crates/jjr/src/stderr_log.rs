//! Redirect process stderr (fd 2) to `<repo_root>/.jj-review/jjr.log` for the
//! duration of a TUI session.
//!
//! Background: synchronous `log_warning` calls (from `store::load_*_comments`,
//! `reviewed::ReviewedState::load`, `WorkingCopyGuard::drop`) write to stderr
//! while ratatui's alternate screen is active. Stderr writes go to the same
//! TTY, land at the cursor position, and corrupt cells. ratatui's next
//! `terminal.draw()` diffs against its in-memory previous-frame buffer (which
//! does not know about the stderr write), so any cell ratatui considers
//! "unchanged" stays showing the warning text — random characters bleed
//! across rows of the diff.

#[cfg(not(unix))]
compile_error!("StderrLogGuard requires unix; non-unix port not implemented");

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use crate::error::{JjrError, Result};

const STATE_DIR: &str = ".jj-review";
const LOG_FILE_NAME: &str = "jjr.log";

pub fn log_path(repo_root: &Path) -> PathBuf {
    repo_root.join(STATE_DIR).join(LOG_FILE_NAME)
}

fn open_log_file(repo_root: &Path) -> Result<File> {
    let dir = repo_root.join(STATE_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| JjrError::Io { source })?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(repo_root))
        .map_err(|source| JjrError::Io { source })
}

fn last_io_err() -> JjrError {
    JjrError::Io {
        source: std::io::Error::last_os_error(),
    }
}

/// `install` does its own dup2 because it must also clean up the saved fd
/// on the error path; this helper covers the suspend/resume sites that don't.
#[expect(
    unsafe_code,
    reason = "libc::dup2 swaps fd 2; caller guarantees src is a valid owned fd"
)]
fn dup2_to_stderr(src: RawFd) -> Result<()> {
    // SAFETY: caller documents that `src` is a valid fd owned by a live
    // resource (StderrLogGuard's `saved` or `log` fd). `STDERR_FILENO` is
    // always a valid target. dup2 atomically replaces fd 2.
    let rc = unsafe { libc::dup2(src, libc::STDERR_FILENO) };
    if rc < 0 {
        return Err(last_io_err());
    }
    Ok(())
}

// Field declaration order is load-bearing: the manual `Drop for
// StderrLogGuard` runs first (dup2 saved → 2 + close saved), then implicit
// field drops fire in declaration order top-down. Listing `saved` (a plain
// `RawFd` with no Drop) before `log` keeps the close of the log fd ordered
// after our manual restore, so fd 2 never points at a closed file.
//
// `install` is single-threaded only: callers must serialize entries (the TUI
// has exactly one). Concurrent `install` from multiple threads would race on
// fd 2.
pub struct StderrLogGuard {
    saved: RawFd,
    /// Kept alive so the redirect target stays valid; field drop runs after
    /// the manual `Drop` impl restores fd 2.
    log: File,
}

impl StderrLogGuard {
    #[expect(
        unsafe_code,
        reason = "libc::dup/dup2/close manage fd 2 directly; std has no safe equivalent"
    )]
    pub fn install(repo_root: &Path) -> Result<Self> {
        let log = open_log_file(repo_root)?;

        // SAFETY: `libc::STDERR_FILENO` is the integer constant 2, which is
        // a valid open file descriptor in any standard process started by a
        // shell or test harness. `libc::dup` has no preconditions beyond fd
        // validity. The returned fd, if non-negative, is owned by this
        // guard until Drop closes it.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 {
            return Err(last_io_err());
        }

        // SAFETY: both fds are valid: `log.as_raw_fd()` is owned by `log`
        // which moves into `Self` on success and outlives the call;
        // `STDERR_FILENO` is process-wide-valid (just dup'd above). dup2
        // atomically replaces fd 2 with a duplicate of the source fd.
        let rc = unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) };
        if rc < 0 {
            let err = last_io_err();
            // SAFETY: `saved` was just returned by `dup` and is owned solely
            // by this scope (no other code has observed it). Closing it on
            // the error path prevents an fd leak.
            unsafe {
                libc::close(saved);
            }
            return Err(err);
        }

        Ok(Self { saved, log })
    }

    /// Restore the original stderr to fd 2. Use before spawning a child that
    /// needs interactive stderr access (claude's permission prompts). Must
    /// be paired with [`Self::resume`] before the guard is dropped.
    pub fn suspend(&self) -> Result<()> {
        dup2_to_stderr(self.saved)
    }

    pub fn resume(&self) -> Result<()> {
        dup2_to_stderr(self.log.as_raw_fd())
    }
}

impl Drop for StderrLogGuard {
    #[expect(
        unsafe_code,
        reason = "saved is owned by self; restoring fd 2 and closing the saved alias"
    )]
    fn drop(&mut self) {
        // SAFETY: `self.saved` was produced by `libc::dup` in `install` and
        // is owned exclusively by this guard — no other code holds a copy,
        // so close is sound.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, ExitStatus, Stdio};
    use std::sync::Mutex;

    use super::*;

    /// Serialize tests that mutate process-wide fd 2. cargo runs tests in
    /// parallel by default; without this lock, two concurrent installs can
    /// race on fd 2 and a marker written under one install can land in the
    /// other's log file. Holding the mutex for the full install→spawn→drop
    /// cycle keeps the redirect window single-threaded.
    static FD_MUTEX: Mutex<()> = Mutex::new(());

    fn spawn_marker_writer(marker: &str) -> ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(format!("printf '{marker}\\n' >&2"))
            .stderr(Stdio::inherit())
            .status()
            .unwrap()
    }

    #[test]
    fn log_path_resolves_to_dot_jj_review_jjr_log() {
        let root = Path::new("/tmp/example");
        assert_eq!(
            log_path(root),
            PathBuf::from("/tmp/example/.jj-review/jjr.log")
        );
    }

    /// End-to-end fd test using a child subprocess. The child's
    /// `Stdio::inherit()` stderr is the parent's fd 2, which the guard has
    /// pointed at the log file — so the marker lands in the log.
    #[test]
    fn writes_route_to_log_when_not_suspended() {
        let _lock = FD_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = tempfile::tempdir().unwrap();
        let guard = StderrLogGuard::install(dir.path()).unwrap();

        let status = spawn_marker_writer("NOT_SUSPENDED_MARKER");
        assert!(status.success());

        drop(guard);

        let contents = std::fs::read_to_string(log_path(dir.path())).unwrap();
        assert!(
            contents.contains("NOT_SUSPENDED_MARKER"),
            "expected marker in log, got: {contents:?}"
        );
    }

    /// Suspend window must route inherited-stderr writes back to the
    /// original terminal stderr, NOT to the log file. Pins the fix for the
    /// regression where claude's interactive prompts disappeared into
    /// jjr.log.
    #[test]
    fn suspend_routes_writes_to_inherited_stderr_not_log() {
        let _lock = FD_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = tempfile::tempdir().unwrap();
        let guard = StderrLogGuard::install(dir.path()).unwrap();

        guard.suspend().unwrap();
        let status = spawn_marker_writer("SUSPENDED_MARKER");
        assert!(status.success());
        guard.resume().unwrap();

        drop(guard);

        let contents = std::fs::read_to_string(log_path(dir.path())).unwrap();
        assert!(
            !contents.contains("SUSPENDED_MARKER"),
            "log unexpectedly captured SUSPENDED_MARKER: {contents:?}"
        );
    }
}
