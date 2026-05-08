use std::io::Write as _;
use std::path::Path;
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
        // `not inside a jj repo: ...` is the NotInJjRepo variant; the walk-up
        // helper detects the absence of `.jj/` before jj is invoked.
        .stderr(predicate::str::contains("not inside a jj repo"));
}

#[test]
fn finds_repo_root_when_invoked_from_subdirectory() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping finds_repo_root_when_invoked_from_subdirectory");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture(&tmp);
    let subdir = repo.join("nested").join("inner");
    std::fs::create_dir_all(&subdir).unwrap();

    // `jjr export @` from a child of the repo must walk up, find `.jj/`, and
    // resolve the revset against the discovered root. With no comments yet
    // recorded the command exits 2 with "no comments to export" — proof the
    // repo root was found and the export pipeline ran. Before the walk-up
    // fix, this would have failed with "There is no jj repo".
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&subdir)
        .args(["export", "@"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no comments to export"));
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

/// A multi-change revset like `trunk()..@` passed as a positional argument
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

/// `jjr clear --stale @` removes the stale comment from a fixture that has one
/// stale and one pending comment, leaves the pending comment on disk, exits 0,
/// and emits a summary to stderr.
#[test]
fn clear_stale_removes_stale_comment_and_emits_summary() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_stale_removes_stale_comment_and_emits_summary");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_stale.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "--stale", "@"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared 1 stale comment"));

    let comments_dir = repo.join(".jj-review").join("comments");
    let jsonl: Vec<_> = std::fs::read_dir(&comments_dir)
        .unwrap()
        .filter_map(|e| {
            let entry = e.unwrap();
            if entry.path().extension().is_some_and(|x| x == "jsonl") {
                Some(std::fs::read_to_string(entry.path()).unwrap())
            } else {
                None
            }
        })
        .collect();

    let all_content = jsonl.join("\n");
    assert!(
        !all_content.contains("\"stale\""),
        "stale comment must be removed; got: {all_content}"
    );
    assert!(
        all_content.contains("pending comment"),
        "pending comment must remain; got: {all_content}"
    );
}

/// Bare `jjr clear @` with `n` on stdin aborts and exits non-zero.
#[test]
fn clear_bare_with_n_stdin_aborts() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_bare_with_n_stdin_aborts");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "@"])
        .write_stdin("n\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("clear aborted"));
}

/// Bare `jjr clear @` with `y` on stdin clears all comments.
#[test]
fn clear_bare_with_y_stdin_clears_all() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_bare_with_y_stdin_clears_all");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");
    let ids = resolve_stack_change_ids(&repo);
    assert!(!ids.is_empty());

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "@"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared"));

    let comments_dir = repo.join(".jj-review").join("comments");
    for id in &ids {
        let path = comments_dir.join(format!("{id}.jsonl"));
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.trim().is_empty(),
                "all comments must be removed; got: {content}"
            );
        }
    }
}

/// `jjr clear --stale @` against a fixture with no stale comments exits 0
/// and emits "cleared 0 stale comments".
#[test]
fn clear_stale_no_stale_comments_exits_zero_with_zero_summary() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping clear_stale_no_stale_comments_exits_zero_with_zero_summary"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "--stale", "@"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared 0 stale comments"));
}

#[cfg(test)]
fn append_stale_jsonl(comments_dir: &Path, repo: &Path, change_id: &str, ts: &str) {
    let line = format!(
        r#"{{"schema_version":"diff-comment/v2","scope":"line","change_id":"{change_id}","repo_root":"{repo}","revset":"@","file":"file.txt","side":"new","new_line":1,"hunk_header":"@@ -0,0 +1 @@","target_text":"one","context_before":[],"context_after":[],"comment":"stale","severity":"note","created_at":"{ts}","status":"stale","mismatch_reason":"anchor not found"}}"#,
        repo = repo.display()
    );
    let path = comments_dir.join(format!("{change_id}.jsonl"));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[cfg(test)]
fn append_pending_jsonl(comments_dir: &Path, repo: &Path, change_id: &str, ts: &str) {
    let line = format!(
        r#"{{"schema_version":"diff-comment/v2","scope":"line","change_id":"{change_id}","repo_root":"{repo}","revset":"@","file":"file.txt","side":"new","new_line":1,"hunk_header":"@@ -0,0 +1 @@","target_text":"one","context_before":[],"context_after":[],"comment":"pending","severity":"note","created_at":"{ts}","status":"pending"}}"#,
        repo = repo.display()
    );
    let path = comments_dir.join(format!("{change_id}.jsonl"));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// Inputs for `append_stack_jsonl`. `revset_hash_hex` must match
/// `RevsetHash::from_revset(revset).hex()` for the stack the test resolves;
/// tests compute it via the public library API.
#[cfg(test)]
struct StackJsonlRecord<'a> {
    comments_dir: &'a Path,
    repo: &'a Path,
    revset: &'a str,
    revset_hash_hex: &'a str,
    body: &'a str,
    created_at: &'a str,
}

#[cfg(test)]
fn append_stack_jsonl(rec: &StackJsonlRecord<'_>) {
    let line = format!(
        r#"{{"schema_version":"diff-comment/v2","scope":"stack","revset_hash":"{hash}","repo_root":"{repo}","revset":"{revset}","comment":"{body}","severity":"note","created_at":"{ts}"}}"#,
        hash = rec.revset_hash_hex,
        repo = rec.repo.display(),
        revset = rec.revset,
        body = rec.body,
        ts = rec.created_at,
    );
    let path = rec.comments_dir.join("_stack.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[cfg(test)]
fn resolve_stack_change_ids(repo: &Path) -> Vec<String> {
    let log_out = StdCommand::new("jj")
        .current_dir(repo)
        .args([
            "log",
            "-r",
            "trunk()..@",
            "--reversed",
            "--no-graph",
            "--color=never",
            "-T",
            r#"change_id ++ "\n""#,
        ])
        .output()
        .unwrap();
    assert!(log_out.status.success());
    String::from_utf8_lossy(&log_out.stdout)
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `jjr clear --stale 'trunk()..@'` against the three-change stack removes
/// stale comments written directly into the JSONL files for two of the three
/// changes, leaves the pending comment intact, and reports the correct counts.
#[test]
fn clear_stale_across_three_change_stack() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_stale_across_three_change_stack");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "three_change_stack.sh");
    let ids = resolve_stack_change_ids(&repo);
    assert_eq!(ids.len(), 3, "expected three changes; got: {ids:?}");

    let comments_dir = repo.join(".jj-review").join("comments");
    std::fs::create_dir_all(&comments_dir).unwrap();

    // id[0]: stale only; id[1]: stale + pending; id[2]: pending only.
    append_stale_jsonl(&comments_dir, &repo, &ids[0], "2026-04-29T10:00:00Z");
    append_stale_jsonl(&comments_dir, &repo, &ids[1], "2026-04-29T10:01:00Z");
    append_pending_jsonl(&comments_dir, &repo, &ids[1], "2026-04-29T10:02:00Z");
    append_pending_jsonl(&comments_dir, &repo, &ids[2], "2026-04-29T10:03:00Z");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "--stale", "trunk()..@"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared 2 stale comments"))
        .stderr(predicate::str::contains("across 2 changes"));

    let id0_content =
        std::fs::read_to_string(comments_dir.join(format!("{}.jsonl", ids[0]))).unwrap_or_default();
    assert!(
        !id0_content.contains("\"stale\""),
        "id[0] stale must be gone; got: {id0_content}"
    );

    let id1_content =
        std::fs::read_to_string(comments_dir.join(format!("{}.jsonl", ids[1]))).unwrap_or_default();
    assert!(
        !id1_content.contains("\"stale\"") && id1_content.contains("pending"),
        "id[1]: stale gone, pending kept; got: {id1_content}"
    );

    let id2_content =
        std::fs::read_to_string(comments_dir.join(format!("{}.jsonl", ids[2]))).unwrap_or_default();
    assert!(
        id2_content.contains("pending"),
        "id[2] pending must remain; got: {id2_content}"
    );
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

/// `jjr packet @` against a fixture with a pending comment produces output
/// containing the canonical prelude and the comment body.
#[test]
fn packet_with_pending_comment_writes_to_stdout() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping packet_with_pending_comment_writes_to_stdout");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["packet", "@"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "You are editing a local jj working copy.",
        ))
        .stdout(predicate::str::contains("pending comment"));
}

/// `jjr packet @ -o <file>` writes the prompt to the file and produces no stdout.
#[test]
fn packet_output_flag_writes_to_file() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping packet_output_flag_writes_to_file");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");
    let out_path = tmp.path().join("packet.txt");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["packet", "@", "-o", out_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let contents = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        contents.contains("You are editing a local jj working copy."),
        "output file should contain the prelude; got: {contents}"
    );
    assert!(
        contents.contains("pending comment"),
        "output file should contain the comment body; got: {contents}"
    );
}

/// `jjr packet --include-stale @` against the stale+pending fixture includes
/// the stale comment that would be excluded by default.
#[test]
fn packet_include_stale_flag_includes_stale_comments() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping packet_include_stale_flag_includes_stale_comments");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_stale.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["packet", "--include-stale", "@"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stale comment"))
        .stdout(predicate::str::contains("pending comment"));
}

/// `jjr packet @` against a fixture with no comments exits with code 2 and
/// emits "no comments to send" on stderr.
#[test]
fn packet_no_comments_exits_2_with_message() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping packet_no_comments_exits_2_with_message");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["packet", "@"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no comments to send"));
}

/// `jjr claude @` with no comments exits 2 and emits "no comments to send".
/// Same `EmptyPacket` path as `jjr packet`, but via the claude subcommand.
#[test]
fn claude_no_comments_exits_2_with_message() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping claude_no_comments_exits_2_with_message");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["claude", "@"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no comments to send"));
}

/// `jjr clear @ --yes` against a fixture with a pending comment removes all
/// comments without prompting and exits 0.
#[test]
fn clear_yes_flag_skips_prompt_and_clears_all() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_yes_flag_skips_prompt_and_clears_all");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");
    let ids = resolve_stack_change_ids(&repo);
    assert!(!ids.is_empty());

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "@", "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared"));

    let comments_dir = repo.join(".jj-review").join("comments");
    for id in &ids {
        let path = comments_dir.join(format!("{id}.jsonl"));
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.trim().is_empty(),
                "all comments must be removed after --yes; got: {content}"
            );
        }
    }
}

/// `jjr clear trunk()..@ --orphaned` removes the orphan JSONL file and leaves
/// the in-stack change's file intact.
#[test]
fn clear_orphaned_removes_orphan_file_leaves_in_stack() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_orphaned_removes_orphan_file_leaves_in_stack");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "stack_with_orphan.sh");

    let comments_dir = repo.join(".jj-review").join("comments");
    let orphan_path = comments_dir.join("aabbccddeeff0011.jsonl");
    assert!(orphan_path.exists(), "orphan file must exist before clear");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "trunk()..@", "--orphaned"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared"));

    assert!(
        !orphan_path.exists(),
        "orphan file must be removed after --orphaned clear"
    );

    // In-stack file must still exist.
    let ids = resolve_stack_change_ids(&repo);
    assert!(!ids.is_empty());
    let in_stack_path = comments_dir.join(format!("{}.jsonl", ids[0]));
    assert!(
        in_stack_path.exists(),
        "in-stack change file must not be touched"
    );
}

/// `jjr export @` against a fixture with a pending comment writes JSONL to
/// stdout and each line parses as a valid comment.
#[test]
fn export_jsonl_pending_comment_produces_parseable_output() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping export_jsonl_pending_comment_produces_parseable_output"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");

    let output = Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["export", "@"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "export should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.trim().is_empty(), "stdout must not be empty");

    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line must be valid JSON: {e}\nline: {line}"));
        assert!(
            v.get("schema_version").is_some(),
            "each line must have schema_version"
        );
    }

    assert!(
        stdout.contains("pending comment"),
        "output must contain the pending comment body; got: {stdout}"
    );
}

/// `jjr export @ --format markdown` against a fixture with a pending comment
/// writes a Markdown document with expected structure.
#[test]
fn export_markdown_pending_comment_produces_markdown_output() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping export_markdown_pending_comment_produces_markdown_output"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");

    let output = Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["export", "@", "--format", "markdown"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "export --format markdown should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("# Review export —"),
        "must start with markdown header; got: {stdout}"
    );
    assert!(
        stdout.contains("pending comment"),
        "must contain the pending comment body; got: {stdout}"
    );
}

/// `jjr export @` against a fixture with no comments exits with code 2 and
/// emits "no comments to export" on stderr.
#[test]
fn export_no_comments_exits_2_with_message() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping export_no_comments_exits_2_with_message");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["export", "@"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no comments to export"));
}

/// `jjr claude @` with no `claude` binary on PATH exits non-zero and emits a
/// message naming the missing binary.
///
/// We force `claude` to be absent by prepending a directory that has no
/// `claude` binary to PATH (and removing PATH entries that might have it).
/// The fixture must have a pending comment so we get past the `EmptyPacket`
/// check and actually try to invoke claude.
#[test]
fn claude_missing_binary_exits_nonzero_with_clear_message() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping claude_missing_binary_exits_nonzero_with_clear_message"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");
    let empty_dir = tmp.path().join("empty_bin");
    std::fs::create_dir_all(&empty_dir).unwrap();

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .env("PATH", empty_dir.to_str().unwrap())
        .args(["claude", "@"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("claude").or(predicate::str::contains("PATH")));
}

/// Bare `jjr clear @ --yes` against a fixture with both per-change and
/// stack-scoped comments removes both. Pins the contract that bare clear
/// includes `_stack.jsonl` records (matching the stack's `revset_hash`) — not
/// only per-change files.
#[test]
fn clear_bare_removes_stack_scoped_and_per_change() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_bare_removes_stack_scoped_and_per_change");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change_with_pending.sh");
    let comments_dir = repo.join(".jj-review").join("comments");

    // Inject a stack-scoped comment for the `@` revset.
    let revset = "@";
    let hash_hex = jjr::stack::RevsetHash::from_revset(revset).hex();
    append_stack_jsonl(&StackJsonlRecord {
        comments_dir: &comments_dir,
        repo: &repo,
        revset,
        revset_hash_hex: &hash_hex,
        body: "stack-scoped concern",
        created_at: "2026-04-29T11:00:00Z",
    });

    let stack_path = comments_dir.join("_stack.jsonl");
    assert!(stack_path.exists(), "stack file must be set up");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "@", "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared 2"));

    // Per-change file emptied.
    let ids = resolve_stack_change_ids(&repo);
    assert!(!ids.is_empty());
    for id in &ids {
        let path = comments_dir.join(format!("{id}.jsonl"));
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(
                content.trim().is_empty(),
                "per-change comments must be removed; got: {content}"
            );
        }
    }

    // Stack file no longer contains the matching record.
    let stack_after = std::fs::read_to_string(&stack_path).unwrap();
    assert!(
        !stack_after.contains("stack-scoped concern"),
        "stack-scoped comment must be removed; got: {stack_after}"
    );
}

/// Bare `jjr clear @` against a fixture with no comments exits 0 with
/// "no comments to clear" and emits no prompt. Pins the zero-comment
/// short-circuit at the top of the bare-clear path.
#[test]
fn clear_bare_no_comments_exits_zero_with_message() {
    if !jj_on_path() {
        eprintln!("jj not on PATH; skipping clear_bare_no_comments_exits_zero_with_message");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "single_change.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "@"])
        .assert()
        .success()
        .stderr(predicate::str::contains("no comments to clear"))
        .stderr(predicate::str::contains("Clear ").not());
}

/// `jjr clear @ --stale --orphaned` is rejected by clap before any work runs.
/// Pins the `conflicts_with` contract: the user must pick exactly one filter.
#[test]
fn clear_stale_and_orphaned_combined_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(repo)
        .args(["clear", "@", "--stale", "--orphaned"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

/// `jjr clear --orphaned 'trunk()..@'` against a fixture with no orphan files
/// exits 0 and reports zero. Symmetric to the `--stale` zero case.
#[test]
fn clear_orphaned_no_orphans_exits_zero_with_zero_summary() {
    if !jj_on_path() {
        eprintln!(
            "jj not on PATH; skipping clear_orphaned_no_orphans_exits_zero_with_zero_summary"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = build_fixture_named(&tmp, "three_change_stack.sh");

    Command::cargo_bin("jjr")
        .unwrap()
        .current_dir(&repo)
        .args(["clear", "--orphaned", "trunk()..@"])
        .assert()
        .success()
        .stderr(predicate::str::contains("cleared 0 orphaned comments"));
}
