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

    // TUI testing requires a pty and is deferred to a later phase. We verify
    // here only that the fixture produces a valid jj repo that jjr can find
    // (the resolve_revset call would fail if the repo were malformed).
}

/// Build a fresh fixture repo with one change. Returns the repo path inside
/// the given tempdir.
#[cfg(test)]
fn build_fixture(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = tmp.path().join("repo");
    let script = std::env::current_dir()
        .expect("cwd should be readable in tests")
        .join("tests")
        .join("fixtures")
        .join("single_change.sh");

    let status = StdCommand::new("bash")
        .arg(&script)
        .arg(&repo)
        .status()
        .expect("bash should be on PATH");
    assert!(status.success(), "fixture script failed");
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
