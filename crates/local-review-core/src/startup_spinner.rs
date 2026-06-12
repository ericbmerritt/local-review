//! Animated startup status indicator for the pre-TUI loading phase.
//!
//! Both `ggr` and `jjr` perform slow work before the TUI initializes — `gh`
//! API calls for ggr, `jj` subprocesses for jjr on large repos. During this
//! window nothing is on screen, leaving the reviewer wondering whether the
//! tool is doing anything. `StartupSpinner` fills the gap with a single line
//! of stderr output that animates while the work runs and clears itself once
//! the TUI is ready to take the terminal.
//!
//! ## Usage
//!
//! ```ignore
//! let spinner = local_review_core::startup_spinner::StartupSpinner::start(
//!     "Loading PR #2972…",
//! );
//! let pr = expensive_network_call()?;
//! spinner.stop(); // explicit — clears the line before the TUI starts
//! tui::run(pr)
//! ```
//!
//! `stop()` is explicit so the caller can sequence cleanup deterministically
//! before entering raw mode. The `Drop` implementation also stops the spinner
//! as a safety net if the guard is dropped without calling `stop`, but
//! relying on Drop ordering across the TUI boundary is fragile — prefer the
//! explicit call.
//!
//! ## TTY-only
//!
//! If `stderr` is not a terminal (e.g., output redirected to a file or
//! piped), `start` returns a no-op guard. The spinner uses `\r` carriage
//! returns to overwrite itself in place; that pattern is meaningless outside
//! a TTY and would litter the output with control characters.

use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Braille spinner frames. Renders smoothly on any terminal that supports
/// Unicode; falls back to garbled output on legacy terminals, which is
/// acceptable given the TUI itself uses Unicode glyphs (sigils, separators).
const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Frame interval. 100ms is the standard rate for terminal spinners — slow
/// enough not to dominate visual attention, fast enough to read as motion.
const FRAME_INTERVAL_MS: u64 = 100;

/// Delay before the first frame is drawn. Fast operations (under this
/// threshold) complete before any spinner output is emitted, avoiding a
/// flicker that would be more distracting than informative.
const FIRST_FRAME_DELAY_MS: u64 = 200;

/// Background-driven spinner with a Drop-on-stop guard.
pub struct StartupSpinner {
    /// Signal the worker thread to exit at its next tick.
    stop_flag: Arc<AtomicBool>,
    /// `Some` while the worker is live; taken on stop to join cleanly.
    thread: Option<JoinHandle<()>>,
}

impl StartupSpinner {
    /// Start a spinner that prints `<frame> <message>` to stderr and animates
    /// until [`StartupSpinner::stop`] is called or the guard is dropped.
    ///
    /// Returns a no-op guard when stderr is not a TTY so non-interactive
    /// invocations (CI, redirected stderr) do not litter the output.
    pub fn start(message: impl Into<String>) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self {
                stop_flag: Arc::new(AtomicBool::new(true)),
                thread: None,
            };
        }
        let message = message.into();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop_flag);
        let thread = thread::spawn(move || {
            // Suppress the spinner for fast operations. Anything under
            // FIRST_FRAME_DELAY_MS never paints.
            thread::sleep(Duration::from_millis(FIRST_FRAME_DELAY_MS));
            if flag.load(Ordering::Relaxed) {
                return;
            }
            let mut i: usize = 0;
            loop {
                let frame = FRAMES[i % FRAMES.len()];
                {
                    let mut stderr = std::io::stderr().lock();
                    let _ = write!(stderr, "\r{frame} {message}");
                    let _ = stderr.flush();
                }
                thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                i = i.wrapping_add(1);
            }
        });
        Self {
            stop_flag,
            thread: Some(thread),
        }
    }

    /// Stop the spinner and clear the status line. Joins the worker thread
    /// before returning so subsequent terminal writes are not interleaved
    /// with spinner frames.
    pub fn stop(mut self) {
        self.stop_internal();
    }

    fn stop_internal(&mut self) {
        // First arrival wins — subsequent calls are no-ops.
        if self.stop_flag.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        // Clear the spinner line: carriage return, ANSI erase-to-end-of-line.
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();
    }
}

impl Drop for StartupSpinner {
    fn drop(&mut self) {
        self.stop_internal();
    }
}
