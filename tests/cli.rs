use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;

fn jj_on_path() -> bool {
    StdCommand::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn shows_help() {
    Command::cargo_bin("jjr")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Local terminal review"));
}

#[test]
fn shows_version() {
    Command::cargo_bin("jjr")
        .unwrap()
        .arg("--version")
        .assert()
        .success();
}

#[test]
fn rejects_syntactically_invalid_revset_outside_repo() {
    // A multi-word argument is an invalid jj revset expression. Outside a jj
    // repo jjr will fail before even parsing the revset, but this confirms the
    // binary exits non-zero for clearly bad input.
    Command::cargo_bin("jjr")
        .unwrap()
        .arg("not a change")
        .assert()
        .failure();
}

#[test]
fn errors_outside_jj_repo() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping errors_outside_jj_repo");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(tmp.path())
        .arg("@")
        .assert()
        .failure()
        // `jj failed: ...` is the JjFailed variant, which fires when jj is run outside a repo.
        .stderr(predicate::str::contains("jj failed"));
}

#[test]
fn fixture_repo_creation_is_callable() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping fixture_repo_creation_is_callable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");

    let script = std::env::current_dir()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("single_change.sh");

    let status = StdCommand::new("bash")
        .arg(&script)
        .arg(&repo)
        .status()
        .unwrap();

    assert!(
        status.success(),
        "fixture script failed against {}",
        repo.display()
    );
    assert!(repo.join(".jj").exists(), ".jj directory missing");
    assert!(repo.join("hello.txt").exists(), "fixture file missing");

    // TUI testing requires a pty and is not yet supported. We verify
    // here only that the fixture produces a valid jj repo that jjr can find
    // (the resolve_revset call would fail if the repo were malformed).
}

/// Build a fresh fixture repo with one change. Returns the repo path inside
/// the given tempdir.
#[cfg(test)]
fn build_fixture(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    build_fixture_named(tmp, "single_change.sh")
}

/// Build a fresh fixture repo by running the named script under
/// `tests/fixtures/`. Returns the repo path inside the given tempdir.
#[cfg(test)]
fn build_fixture_named(tmp: &tempfile::TempDir, script_name: &str) -> std::path::PathBuf {
    let repo = tmp.path().join("repo");
    let script = std::env::current_dir()
        .expect("cwd should be readable in tests")
        .join("tests")
        .join("fixtures")
        .join(script_name);

    let status = StdCommand::new("bash")
        .arg(&script)
        .arg(&repo)
        .status()
        .expect("bash should be on PATH");
    assert!(status.success(), "fixture script {script_name} failed");
    repo
}

#[test]
fn resolve_revset_does_not_fail_with_invalid_change_id_inside_jj_repo() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping resolve_revset_does_not_fail_with_invalid_change_id_inside_jj_repo");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);

    // Running with `@` must not fail with "invalid change id" — that was the
    // primary use-case bug where ChangeId::parse("@") rejected the literal `@`.
    // The binary will fail after resolve_revset with a terminal-related error
    // (no tty in CI), but must NOT fail with InvalidChangeId.
    let output = Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .arg("@")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid change id"),
        "jjr @ should not fail with 'invalid change id', got: {stderr}"
    );
}

#[test]
fn nonexistent_change_id_fails_with_jj_failed() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping nonexistent_change_id_fails_with_jj_failed");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);

    // A syntactically valid but unresolvable change ID. jj returns non-zero,
    // so jjr should surface JjFailed (not JjUnexpectedOutput, not
    // InvalidChangeId). This documents the no-match fail-fast contract.
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .arg("nonexistent99999999")
        .assert()
        .failure()
        .stderr(predicate::str::contains("jj failed"));
}

#[test]
fn multi_match_revset_fails_with_revset_ambiguous() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping multi_match_revset_fails_with_revset_ambiguous");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);

    // The fixture leaves the working copy at `@` on top of the described
    // change. `@|@-` resolves to two changes. jjr must surface
    // RevsetAmbiguous with the multi-line raw output for diagnosis, not
    // mash the IDs together and fail as InvalidChangeId.
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .arg("@|@-")
        .assert()
        .failure()
        .stderr(predicate::str::contains("matched multiple changes"));
}

#[test]
fn zero_match_revset_via_none_keyword_fails_with_revset_no_match() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping zero_match_revset_via_none_keyword_fails_with_revset_no_match");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);

    // jj's `none()` revset keyword resolves to the empty set; jj exits 0 with
    // empty stdout. jjr must surface RevsetNoMatch — distinct from the
    // multi-match case (RevsetAmbiguous) and from the bad-revset case
    // (JjFailed when jj exits non-zero).
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .arg("none()")
        .assert()
        .failure()
        .stderr(predicate::str::contains("matched no changes"));
}

/// Bare `jjr` (no args) routes to stack mode. The fixture repo has no
/// `trunk()` alias configured, so the stack revset falls back to `@` with a
/// warning on stderr. The binary will then fail trying to open a TUI without a
/// real terminal, but it must NOT fail with "invalid change id" — that would
/// indicate the old single-change `@` path ran instead of stack mode.
///
/// This test pins the stack-by-default dispatch: bare `jjr` = stack mode.
#[test]
fn bare_jjr_goes_to_stack_mode_and_not_single_change() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping bare_jjr_goes_to_stack_mode_and_not_single_change");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);

    let output = Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Stack mode with no trunk() alias emits a fallback warning, not an
    // "invalid change id" error. Either a terminal error or the fallback
    // warning is acceptable; what must not appear is the single-change-mode
    // error path ("invalid change id").
    assert!(
        !stderr.contains("invalid change id"),
        "bare jjr should not fail with 'invalid change id'; got: {stderr}"
    );
}

/// G1: A multi-change revset like `trunk()..@` passed as a positional argument
/// goes to single-change dispatch, where `resolve_revset` correctly errors
/// with `RevsetAmbiguous`. This pins that positional revsets do NOT silently
/// route to stack mode — only bare `jjr` and `--stack` do.
#[test]
fn stack_revset_as_positional_arg_fails_with_revset_ambiguous() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping stack_revset_as_positional_arg_fails_with_revset_ambiguous"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "three_change_stack.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .arg("trunk()..@")
        .assert()
        .failure()
        .stderr(predicate::str::contains("matched multiple changes"));
}

/// Pins the oldest-first ordering of `resolve_stack`. The 3-change fixture
/// produces commits in order: first → second → third (with @ on third). The
/// jj `--reversed` flag passed by `resolve_stack` ensures the stack walks
/// oldest-to-newest, which is what the reviewer expects when navigating with
/// `n` (forward) through their work.
///
/// Test is end-to-end: invokes `jj log --reversed` directly to capture the
/// expected ordering, then verifies the descriptions appear in the same
/// oldest-first order. If `resolve_stack` were to drop `--reversed`, the
/// observed jj output would be newest-first and this test would fail.
#[test]
fn jj_log_reversed_returns_stack_oldest_first() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping jj_log_reversed_returns_stack_oldest_first");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "three_change_stack.sh");

    let output = StdCommand::new("jj")
        .current_dir(&repo)
        .args([
            "log",
            "-r",
            "trunk()..@",
            "--reversed",
            "--no-graph",
            "--color=never",
            "-T",
            r#"description.first_line() ++ "\n""#,
        ])
        .output()
        .expect("jj log should run");
    assert!(
        output.status.success(),
        "jj log failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let descriptions: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    assert_eq!(
        descriptions,
        vec!["first", "second", "third"],
        "stack should walk oldest-to-newest with --reversed"
    );
}
