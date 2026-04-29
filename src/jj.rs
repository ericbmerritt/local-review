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
    let diff = show_diff(change_id)?;
    Ok(ChangeDetails {
        change_id: metadata.change_id,
        commit_id: metadata.commit_id,
        description: metadata.description,
        diff,
    })
}

struct LogMetadata {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
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

    let mut entries = parse_log_template_records(&output)?;
    let entry = entries.pop().ok_or_else(|| JjrError::JjUnexpectedOutput {
        raw: output.clone(),
    })?;

    Ok(LogMetadata {
        change_id: entry.change_id,
        commit_id: entry.commit_id,
        description: entry.description,
    })
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
    /// `log_metadata` enforces single-record semantics on top via `.pop()`,
    /// silently dropping earlier entries — that's the caller's responsibility,
    /// not the parser's.
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
}
