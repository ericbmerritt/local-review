//! End-to-end TUI tests that drive a real `jjr` binary inside a PTY.
//!
//! ## What these tests catch
//!
//! Bugs in the full integration of: argument parsing, jj subprocess calls,
//! terminal setup, key dispatch, screen transitions, comment storage,
//! teardown. The unit tests in `local-review-core::tui::app` and
//! `local-review-core::tui::composer` cover state-machine logic in
//! isolation; these tests verify that the wired-up binary actually
//! responds to keystrokes and persists state to disk as expected.
//!
//! ## What they don't catch reliably
//!
//! Visual-only regressions (focused-field marker, severity color, scope
//! highlighting). Asserting on the ANSI-encoded alt-screen output is
//! brittle and we have not invested in a render-snapshot strategy. The
//! unit tests cover the state side; visual layout has been stable enough
//! to not justify the cost yet.
//!
//! ## Reliability
//!
//! PTY-driven tests have inherent timing sensitivity. We mitigate by
//! pinning the working directory and `XDG_DATA_HOME` to a tempdir so
//! on-disk artifacts are deterministic, and using `expect` patterns to
//! synchronize against rendered text where possible. The tests skip when
//! `jj` is not on PATH.
//!
//! If a test becomes flaky in CI, mark it `#[ignore]` rather than
//! degrading the assertion: silently-passing tests are worse than no
//! tests.

// Workspace lints deny `expect_used` even though `allow-expect-in-tests = true`
// is set in `clippy.toml`; cargo applies the workspace lints to integration
// tests too. PTY-driven tests use `expect`/`unwrap` heavily for setup
// failures that would mean the test environment is broken, not the code
// under test — those should panic, not be quietly swallowed.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "PTY test setup failures should panic, not be propagated"
)]

use std::path::Path;
use std::process::Command as StdCommand;
use std::time::Duration;

use expectrl::process::unix::{PtyStream, UnixProcess};
use expectrl::process::Process;
use expectrl::session::Session;
use expectrl::{Eof, Expect, Regex};

type PtySession = Session<UnixProcess, PtyStream>;

fn jj_on_path() -> bool {
    StdCommand::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a fresh fixture jj repo via the shared bash script. `XDG_DATA_HOME`
/// is set to `tmp.path()` so jjr's comment storage lands inside the tempdir
/// where we can assert on it.
fn build_fixture(tmp: &tempfile::TempDir, script: &str) -> std::path::PathBuf {
    let repo = tmp.path().join("repo");
    let script_path = std::env::current_dir()
        .expect("test setup")
        .join("tests")
        .join("fixtures")
        .join(script);
    let status = StdCommand::new("bash")
        .arg(&script_path)
        .arg(&repo)
        .env("XDG_DATA_HOME", tmp.path())
        .status()
        .expect("test setup");
    assert!(status.success(), "fixture {script} failed");
    repo
}

/// Comments directory mirrors `store::comments_dir(data_home, repo_root)`.
fn comments_dir(xdg: &Path, repo: &Path) -> std::path::PathBuf {
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_owned());
    let relative = canonical.strip_prefix("/").unwrap_or(&canonical);
    xdg.join("jjr")
        .join("repos")
        .join(relative)
        .join("comments")
}

/// Spawn the `jjr` binary inside a PTY with `cwd` set to `repo` and
/// `XDG_DATA_HOME` set to `xdg`. Pins the PTY window size so the TUI
/// passes its `MIN_COLS` / `MIN_ROWS` checks.
///
/// Builds a `std::process::Command` directly rather than going through
/// `expectrl::spawn(&str)`, which mis-tokenizes nested shell quoting.
fn spawn_jjr(repo: &Path, xdg: &Path, args: &[&str]) -> PtySession {
    let binary = env!("CARGO_BIN_EXE_jjr");
    let mut command = StdCommand::new(binary);
    command
        .current_dir(repo)
        .env("XDG_DATA_HOME", xdg)
        .env("TERM", "xterm-256color");
    for a in args {
        command.arg(a);
    }
    let mut process = UnixProcess::spawn_command(command).expect("spawn jjr in PTY");
    process
        .set_window_size(120, 40)
        .expect("set PTY window size — jjr enforces a MIN_COLS/MIN_ROWS floor");
    let stream = process.open_stream().expect("open PTY stream");
    let mut session = Session::new(process, stream).expect("build PTY session");
    session.set_expect_timeout(Some(Duration::from_secs(15)));
    session
}

/// Verify jjr starts, accepts `q` to quit, and exits cleanly. This is the
/// smoke test for the entire startup → render → key dispatch → teardown
/// pipeline. If `q` does not produce an exit, something between argument
/// parsing and the event loop is broken.
#[test]
fn pty_jjr_starts_and_quits_on_q() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping pty_jjr_starts_and_quits_on_q");
        return;
    }
    let tmp = tempfile::tempdir().expect("test setup");
    let repo = build_fixture(&tmp, "single_change.sh");

    let mut session = spawn_jjr(&repo, tmp.path(), &["@"]);

    // Wait for the first render. The file-header bar shows the filename
    // once setup is complete and the TUI is interactive.
    session
        .expect(Regex(r"hello\.txt"))
        .expect("file header should render hello.txt within timeout");

    session.send("q").expect("send q");
    session.expect(Eof).expect("jjr should exit after q");
}

/// Drive the full save flow: open the file diff, open the composer, type a
/// comment body, save with Ctrl-X, quit. After exit, verify the JSONL file
/// appeared in the comments directory with the expected body.
///
/// This catches any regression in: file-view entry, `c` → composer open,
/// body input, Ctrl-X → save, on-disk persistence.
#[test]
#[ignore = "flaky on cold caches; run with `cargo test --test tui_pty -- --ignored`"]
fn pty_jjr_saves_comment_to_disk() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping pty_jjr_saves_comment_to_disk");
        return;
    }
    let tmp = tempfile::tempdir().expect("test setup");
    let repo = build_fixture(&tmp, "single_change.sh");

    let mut session = spawn_jjr(&repo, tmp.path(), &["@"]);

    // Wait for the entity list to settle.
    session
        .expect(Regex(r"hello\.txt"))
        .expect("initial render");
    std::thread::sleep(Duration::from_millis(300));

    // Open the full file diff (capital F bypasses entity-list dependencies
    // on extractor output for plain-text files).
    session.send("F").expect("send F");
    std::thread::sleep(Duration::from_millis(400));

    // Open the composer.
    session.send("c").expect("send c");
    std::thread::sleep(Duration::from_millis(400));

    // Type a body unique enough to grep for in the persisted JSONL.
    let body = "regression-test-comment-body";
    session.send(body).expect("type body");
    std::thread::sleep(Duration::from_millis(300));

    // Save with Ctrl-X (ASCII 0x18).
    session.send([0x18u8]).expect("send Ctrl-X");
    std::thread::sleep(Duration::from_millis(500));

    // Quit.
    session.send("q").expect("send q");
    let _ = session.expect(Eof);

    // A JSONL file containing the body must appear in the comments dir.
    let cdir = comments_dir(tmp.path(), &repo);
    assert!(
        cdir.exists(),
        "comments dir must exist at {}",
        cdir.display()
    );
    let mut found = false;
    if let Ok(entries) = std::fs::read_dir(&cdir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if content.contains(body) {
                found = true;
                break;
            }
        }
    }
    assert!(
        found,
        "no comment JSONL containing {body:?} found in {}",
        cdir.display()
    );
}
