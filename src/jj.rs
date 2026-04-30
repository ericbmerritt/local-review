use std::io::Write as _;
use std::process::Command;

use crate::change_id::{ChangeId, CommitId};
use crate::diff::{self, Diff};
use crate::error::{JjrError, Result};
use crate::stack::{ResolvedStack, RevsetHash, StackEntry};

/// Field separator used in our `jj log -T '<template>'` invocations.
///
/// Using ASCII unit-separator (`\x1F`) rather than newlines keeps parsing
/// unambiguous if a description ever contains embedded newlines.
const FIELD_SEP: char = '\x1F';

#[derive(Debug, Clone)]
pub struct ChangeDetails {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    pub description: String,
    pub diff: Diff,
}

/// Edit the working copy to the given change within `repo_root`.
///
/// Passes `--repository` so the call is safe regardless of the process cwd.
pub fn edit(repo_root: &std::path::Path, change_id: &ChangeId) -> Result<()> {
    let repo_str = repo_root.to_str().unwrap_or(".");
    run_jj_discard(&["--repository", repo_str, "edit", change_id.as_str()])
}

/// Return the change ID currently checked out as `@` in `repo_root`.
///
/// Passes `--repository` so the call is safe regardless of the process cwd.
pub fn current_change(repo_root: &std::path::Path) -> Result<ChangeId> {
    let repo_str = repo_root.to_str().unwrap_or(".");
    let output = run_jj(&[
        "--repository",
        repo_str,
        "log",
        "-r",
        "@",
        "--no-graph",
        "--color=never",
        "-T",
        r#"change_id ++ "\n""#,
    ])?;
    let non_empty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    match non_empty.as_slice() {
        [single] => ChangeId::parse(single.trim()),
        [] => Err(JjrError::RevsetNoMatch {
            revset: "@".to_owned(),
        }),
        _ => Err(JjrError::RevsetAmbiguous {
            revset: "@".to_owned(),
            raw: output,
        }),
    }
}

/// Resolve a revset expression (including `@`, `@-`, branch names, etc.) to a
/// `ChangeId`. The revset must resolve to exactly one change.
pub fn resolve_revset(revset: &str) -> Result<ChangeId> {
    // Template emits one ID per line followed by `\n` so multi-match revsets
    // are detectable. Without the trailing newline jj concatenates all matching
    // IDs on a single line and a "is there a second line?" check would silently
    // pass for multi-match revsets.
    let output = run_jj(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "--color=never",
        "-T",
        r#"change_id ++ "\n""#,
    ])?;
    let non_empty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    match non_empty.as_slice() {
        [single] => ChangeId::parse(single.trim()),
        [] => Err(JjrError::RevsetNoMatch {
            revset: revset.to_owned(),
        }),
        _ => Err(JjrError::RevsetAmbiguous {
            revset: revset.to_owned(),
            raw: output,
        }),
    }
}

pub fn show(change_id: &ChangeId) -> Result<ChangeDetails> {
    let metadata = log_metadata(change_id)?;
    let description = fetch_description(change_id)?;
    let diff = show_diff(change_id)?;
    Ok(ChangeDetails {
        change_id: metadata.change_id,
        commit_id: metadata.commit_id,
        description,
        diff,
    })
}

/// Full multi-line description for a single change. Trailing newline stripped
/// so callers can split on `\n` without an empty trailing line.
pub fn fetch_description(change_id: &ChangeId) -> Result<String> {
    let output = run_jj(&[
        "log",
        "-r",
        change_id.as_str(),
        "--no-graph",
        "--color=never",
        "-T",
        "description",
    ])?;
    Ok(strip_trailing_newline(&output).to_owned())
}

/// Strip trailing `\n` and `\r` so callers can split on `\n` without an
/// empty trailing element. Handles CRLF (`"line\r\n"` → `"line"`), bare LF,
/// and runs of trailing newlines. Internal `\r` characters are preserved.
pub(crate) fn strip_trailing_newline(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r'])
}

/// Fetch the unified diff for a single change.
pub fn diff_for_change(change_id: &ChangeId) -> Result<Diff> {
    show_diff(change_id)
}

#[derive(Debug)]
struct LogMetadata {
    change_id: ChangeId,
    commit_id: CommitId,
}

fn log_metadata(change_id: &ChangeId) -> Result<LogMetadata> {
    let output = run_jj(&[
        "log",
        "-r",
        change_id.as_str(),
        "--no-graph",
        "--color=never",
        "-T",
        log_template(),
    ])?;

    let entries = parse_log_template_records(&output)?;
    log_metadata_from_records(change_id, entries, output)
}

/// Pure record-count enforcement for `log_metadata`. Splitting this out keeps
/// the divergent-change-id branch unit-testable without spawning jj.
fn log_metadata_from_records(
    change_id: &ChangeId,
    entries: Vec<LogRecord>,
    raw: String,
) -> Result<LogMetadata> {
    let mut iter = entries.into_iter();
    match (iter.next(), iter.next()) {
        (Some(entry), None) => Ok(LogMetadata {
            change_id: entry.change_id,
            commit_id: entry.commit_id,
        }),
        (None, _) => Err(JjrError::JjUnexpectedOutput { raw }),
        (Some(_), Some(_)) => Err(JjrError::RevsetAmbiguous {
            revset: change_id.as_str().to_owned(),
            raw,
        }),
    }
}

fn show_diff(change_id: &ChangeId) -> Result<Diff> {
    let output = run_jj(&["show", change_id.as_str(), "--git", "--color=never"])?;
    diff::parse(&output)
}

/// Default revset used when stack mode runs without an explicit revset.
///
/// `trunk()..@` is exclusive on the left (excludes trunk itself) and inclusive
/// on the right (includes @), giving exactly the user's work since trunk.
pub const DEFAULT_STACK_REVSET: &str = "trunk()..@";

/// Decide whether a jj error message indicates the revset should fall back to
/// `@`. Pure helper so the heuristic is testable without spawning jj.
///
/// Triggered when `trunk()` is unconfigured or otherwise unresolvable — jj
/// surfaces this through any of these phrases in stderr.
fn should_fall_back_on_jj_error(message: &str) -> bool {
    message.contains("alias not configured")
        || message.contains("trunk")
        || message.contains("undefined")
}

/// Decide whether a successful-but-empty stack result should fall back to `@`.
/// Pure helper for symmetry with `should_fall_back_on_jj_error`; the rule is
/// trivially "the entries vec is empty," but expressing it this way keeps the
/// fallback policy in one named place.
fn should_fall_back_on_empty(entries: &[StackEntry]) -> bool {
    entries.is_empty()
}

/// Resolve a revset to an ordered `ResolvedStack` (oldest-to-newest).
///
/// If the revset resolves to an empty set (for example when `trunk()` is not
/// configured), this function falls back to `@` and emits a warning on stderr.
/// If jj exits non-zero with a message indicating `trunk()` is not configured,
/// the same fallback applies.
///
/// When fallback fires, the returned `ResolvedStack` reports `revset = "@"`
/// (and the corresponding hash) so the cursor key matches the actual entries —
/// a future run with the original revset that also falls back will hash to the
/// same key.
pub fn resolve_stack(revset: &str) -> Result<ResolvedStack> {
    let template = log_template();

    // `--reversed` makes jj emit records oldest-first. Without it jj's default
    // ordering is newest-first, which would put @ at index 0 and the oldest
    // change at index N — backwards from the reviewer's mental model of
    // walking the stack forward.
    let primary = run_jj(&[
        "log",
        "-r",
        revset,
        "--reversed",
        "--no-graph",
        "--color=never",
        "-T",
        template,
    ]);

    let (effective_revset, raw_output, used_fallback) = match primary {
        Ok(output) => (revset.to_owned(), output, false),
        Err(JjrError::JjFailed { ref message, .. }) if should_fall_back_on_jj_error(message) => {
            warn_fallback_to_at(revset);
            let fallback_raw = run_jj(&[
                "log",
                "-r",
                "@",
                "--reversed",
                "--no-graph",
                "--color=never",
                "-T",
                template,
            ])?;
            ("@".to_owned(), fallback_raw, true)
        }
        Err(e) => return Err(e),
    };

    let mut entries = parse_stack_entries(&raw_output)?;
    let mut effective_revset = effective_revset;

    // Empty primary result also triggers fallback (e.g. `trunk()` returned 0
    // changes, not an error).
    if should_fall_back_on_empty(&entries) && !used_fallback {
        warn_fallback_to_at(revset);
        let fallback_raw = run_jj(&[
            "log",
            "-r",
            "@",
            "--reversed",
            "--no-graph",
            "--color=never",
            "-T",
            template,
        ])?;
        entries = parse_stack_entries(&fallback_raw)?;
        "@".clone_into(&mut effective_revset);
    }

    if entries.is_empty() {
        return Err(JjrError::RevsetNoMatch {
            revset: format!("{revset} (fell back to @, which is also empty)"),
        });
    }

    Ok(ResolvedStack {
        revset_hash: RevsetHash::from_revset(&effective_revset),
        revset: effective_revset,
        entries,
    })
}

/// jj log template emitting one record per change as
/// `<change_id>\x1F<commit_id>\x1F<description_first_line>\n`.
///
/// Used by both `log_metadata` (single record) and `resolve_stack`
/// (multi-record). Unit-separator delimiters keep parsing immune to multi-line
/// descriptions.
fn log_template() -> &'static str {
    r#"change_id ++ "\x1F" ++ commit_id ++ "\x1F" ++ description.first_line() ++ "\n""#
}

/// Parsed log-template record. Mirrors `LogMetadata` but is also used as the
/// shared shape returned by [`parse_log_template_records`].
struct LogRecord {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
}

/// Parse zero-or-more `\x1F`-delimited log records, one per non-empty line.
///
/// Trailing whitespace on each line is trimmed (the template appends `\n` and
/// jj sometimes adds extra spacing); a malformed line returns
/// `JjUnexpectedOutput`.
fn parse_log_template_records(raw: &str) -> Result<Vec<LogRecord>> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(3, FIELD_SEP).collect();
        let (raw_change, raw_commit, description) = match parts.as_slice() {
            [c, co, d] => (*c, *co, *d),
            _ => {
                return Err(JjrError::JjUnexpectedOutput {
                    raw: trimmed.to_owned(),
                })
            }
        };
        entries.push(LogRecord {
            change_id: ChangeId::parse(raw_change.trim())?,
            commit_id: CommitId::parse(raw_commit.trim())?,
            description: description.trim().to_owned(),
        });
    }
    Ok(entries)
}

fn parse_stack_entries(raw: &str) -> Result<Vec<StackEntry>> {
    Ok(parse_log_template_records(raw)?
        .into_iter()
        .map(|r| StackEntry {
            change_id: r.change_id,
            commit_id: r.commit_id,
            description: r.description,
        })
        .collect())
}

fn warn_fallback_to_at(revset: &str) {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(
        handle,
        "warning: revset {revset:?} resolved empty or errored; falling back to @"
    );
}

fn run_jj(args: &[&str]) -> Result<String> {
    let output = Command::new("jj").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            JjrError::JjMissing { source: e }
        } else {
            JjrError::Io { source: e }
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(JjrError::JjFailed {
            message: stderr,
            exit_code: output.status.code(),
        });
    }

    String::from_utf8(output.stdout).map_err(|source| JjrError::JjOutputEncoding { source })
}

/// Like [`run_jj`] but discards stdout.
fn run_jj_discard(args: &[&str]) -> Result<()> {
    run_jj(args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stack_entries_happy_multi_record() {
        let raw = "abc11111\x1Faabbccdd11223344\x1Ffirst\n\
                   abc22222\x1Feeff112233445566\x1Fsecond line\n";
        let entries = parse_stack_entries(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].change_id.as_str(), "abc11111");
        assert_eq!(entries[0].description, "first");
        assert_eq!(entries[1].change_id.as_str(), "abc22222");
        assert_eq!(entries[1].description, "second line");
    }

    #[test]
    fn parse_stack_entries_empty_input() {
        let entries = parse_stack_entries("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_stack_entries_skips_blank_lines() {
        let raw = "\nabc11111\x1Faabbccdd11223344\x1Fdesc\n\n";
        let entries = parse_stack_entries(raw).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn parse_stack_entries_malformed_line_errors() {
        // Missing the third field entirely.
        let raw = "abc11111\x1Faabbccdd11223344\n";
        assert!(parse_stack_entries(raw).is_err());
    }

    #[test]
    fn parse_stack_entries_change_id_parse_failure_propagates() {
        let raw = "not-a-valid-change!\x1Faabbccdd11223344\x1Fdesc\n";
        assert!(parse_stack_entries(raw).is_err());
    }

    #[test]
    fn parse_stack_entries_commit_id_parse_failure_propagates() {
        let raw = "abc11111\x1Fnot-hex\x1Fdesc\n";
        assert!(parse_stack_entries(raw).is_err());
    }

    #[test]
    fn default_stack_revset_is_trunk_dotdot_at() {
        // Pin the constant so any future edit is deliberate.
        assert_eq!(DEFAULT_STACK_REVSET, "trunk()..@");
    }

    // ---- fallback-decision predicates ----

    #[test]
    fn should_fall_back_on_jj_error_matches_alias_not_configured() {
        assert!(should_fall_back_on_jj_error(
            "Error: revset alias not configured: trunk()"
        ));
    }

    #[test]
    fn should_fall_back_on_jj_error_matches_trunk_substring() {
        assert!(should_fall_back_on_jj_error(
            "Error: trunk() could not be resolved"
        ));
    }

    #[test]
    fn should_fall_back_on_jj_error_matches_undefined() {
        assert!(should_fall_back_on_jj_error("undefined symbol foo"));
    }

    #[test]
    fn should_fall_back_on_jj_error_rejects_unrelated_messages() {
        assert!(!should_fall_back_on_jj_error("permission denied"));
        assert!(!should_fall_back_on_jj_error("no such file or directory"));
        assert!(!should_fall_back_on_jj_error(""));
    }

    #[test]
    fn should_fall_back_on_empty_true_for_empty_slice() {
        assert!(should_fall_back_on_empty(&[]));
    }

    #[test]
    fn should_fall_back_on_empty_false_for_non_empty() {
        let entry = StackEntry {
            change_id: ChangeId::parse("abc11111").unwrap(),
            commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
            description: "first".to_owned(),
        };
        assert!(!should_fall_back_on_empty(&[entry]));
    }

    /// Pin the raw parser's contract: it returns *all* records in input order.
    /// `log_metadata` enforces single-record semantics on top by erroring with
    /// `RevsetAmbiguous` when more than one record is present (e.g. divergent
    /// change IDs); the parser itself stays oblivious.
    #[test]
    fn parse_log_template_records_returns_all_records_caller_enforces_count() {
        let raw = "abc11111\x1Faabbccdd11223344\x1Ffirst\n\
                   abc22222\x1Feeff112233445566\x1Fsecond\n";
        let records = parse_log_template_records(raw).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].description, "first");
        assert_eq!(records[1].description, "second");
        assert_eq!(records[0].change_id.as_str(), "abc11111");
        assert_eq!(records[1].change_id.as_str(), "abc22222");
    }

    /// Divergent change IDs cause jj to emit two records for a single change-id
    /// argument. Pin that `log_metadata_from_records` rejects this with
    /// `RevsetAmbiguous` rather than silently keeping one of the commits — a
    /// silent pick would route Claude at the wrong diff.
    #[test]
    fn log_metadata_from_records_two_records_returns_revset_ambiguous() {
        let change_id = ChangeId::parse("abc11111").unwrap();
        let entries = vec![
            LogRecord {
                change_id: ChangeId::parse("abc11111").unwrap(),
                commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
                description: "first".to_owned(),
            },
            LogRecord {
                change_id: ChangeId::parse("abc11111").unwrap(),
                commit_id: CommitId::parse("eeff112233445566").unwrap(),
                description: "second".to_owned(),
            },
        ];
        let raw = "abc11111\x1Faabbccdd11223344\x1Ffirst\n\
                   abc11111\x1Feeff112233445566\x1Fsecond\n"
            .to_owned();
        let err = log_metadata_from_records(&change_id, entries, raw).unwrap_err();
        assert!(
            matches!(err, JjrError::RevsetAmbiguous { ref revset, .. } if revset == "abc11111"),
            "expected RevsetAmbiguous for divergent change-id, got {err:?}"
        );
    }

    #[test]
    fn log_metadata_from_records_zero_records_returns_unexpected_output() {
        let change_id = ChangeId::parse("abc11111").unwrap();
        let err = log_metadata_from_records(&change_id, vec![], String::new()).unwrap_err();
        assert!(
            matches!(err, JjrError::JjUnexpectedOutput { .. }),
            "expected JjUnexpectedOutput for empty records, got {err:?}"
        );
    }

    // -- T-G4: pure helper that owns the post-processing of `jj log -T
    //   description` output. Pinning here keeps `fetch_description`'s
    //   contract honest without spawning jj.
    #[test]
    fn strip_trailing_newline_handles_multi_line_input() {
        assert_eq!(
            strip_trailing_newline("First line\nSecond line\n"),
            "First line\nSecond line"
        );
    }

    #[test]
    fn strip_trailing_newline_handles_only_newline() {
        assert_eq!(strip_trailing_newline("\n"), "");
    }

    #[test]
    fn strip_trailing_newline_leaves_no_newline_input_unchanged() {
        assert_eq!(
            strip_trailing_newline("Add retry policy"),
            "Add retry policy"
        );
    }

    // -- C1: CRLF inputs (jj output normalized through tools that emit `\r\n`)
    //   must lose both characters; downstream consumers don't expect a stray CR.
    #[test]
    fn strip_trailing_newline_strips_crlf() {
        assert_eq!(strip_trailing_newline("line\r\n"), "line");
    }

    #[test]
    fn strip_trailing_newline_strips_multiple_trailing_newlines() {
        assert_eq!(strip_trailing_newline("line\n\n\n"), "line");
    }

    #[test]
    fn strip_trailing_newline_preserves_internal_carriage_returns() {
        assert_eq!(strip_trailing_newline("a\r\nb"), "a\r\nb");
    }

    #[test]
    fn log_metadata_from_records_one_record_returns_metadata() {
        let change_id = ChangeId::parse("abc11111").unwrap();
        let entries = vec![LogRecord {
            change_id: ChangeId::parse("abc11111").unwrap(),
            commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
            description: "only".to_owned(),
        }];
        let raw = "abc11111\x1Faabbccdd11223344\x1Fonly\n".to_owned();
        let meta = log_metadata_from_records(&change_id, entries, raw).unwrap();
        assert_eq!(meta.change_id.as_str(), "abc11111");
        assert_eq!(meta.commit_id.as_str(), "aabbccdd11223344");
    }
}
