use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};

use crate::change_id::ChangeId;
use crate::comment::{format_rfc3339, Anchor, Comment, SCHEMA_VERSION_VALUE};
use crate::error::{JjrError, Result};

/// Filename reserved for stack-scoped comments. jj change IDs never start with
/// `_`, so there is no collision risk.
pub(crate) const STACK_FILENAME: &str = "_stack.jsonl";

fn comments_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".jj-review").join("comments")
}

fn change_file(repo_root: &Path, change_id: &ChangeId) -> PathBuf {
    comments_dir(repo_root).join(format!("{}.jsonl", change_id.to_filename()))
}

fn anchor_file(repo_root: &Path, comment: &Comment) -> PathBuf {
    match &comment.anchor {
        Anchor::Line { change_id, .. } | Anchor::Change { change_id } => {
            change_file(repo_root, change_id)
        }
        Anchor::Stack => comments_dir(repo_root).join(STACK_FILENAME),
    }
}

/// Creates `.jj-review/comments/` under `repo_root` if it does not already exist. Idempotent.
pub(crate) fn ensure_review_dir(repo_root: &Path) -> Result<()> {
    std::fs::create_dir_all(comments_dir(repo_root)).map_err(|source| JjrError::Io { source })
}

/// Idempotently append `/.jj-review` to `.gitignore` and `.jjignore`.
///
/// Each ignore file is created if absent. The entry is not duplicated if it
/// already exists.
pub(crate) fn ensure_ignored(repo_root: &Path) -> Result<()> {
    ensure_entry_in_file(repo_root, ".gitignore", "/.jj-review")?;
    ensure_entry_in_file(repo_root, ".jjignore", "/.jj-review")?;
    Ok(())
}

fn ensure_entry_in_file(repo_root: &Path, filename: &str, entry: &str) -> Result<()> {
    let path = repo_root.join(filename);

    let existing = if path.exists() {
        std::fs::read_to_string(&path).map_err(|source| JjrError::Io { source })?
    } else {
        String::new()
    };

    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| JjrError::Io { source })?;

    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file).map_err(|source| JjrError::Io { source })?;
    }
    writeln!(file, "{entry}").map_err(|source| JjrError::Io { source })?;
    Ok(())
}

/// Malformed lines are written to stderr and skipped; valid records are
/// returned. A schema-version mismatch on any parseable record is a hard error.
pub fn load_change_comments(repo_root: &Path, change_id: &ChangeId) -> Result<Vec<Comment>> {
    let path = change_file(repo_root, change_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    load_jsonl_file(&path)
}

/// Also calls `ensure_review_dir` and `ensure_ignored` (idempotent).
///
/// Performs a pre-write uniqueness check on `created_at`: if any existing
/// record in the target file shares the same timestamp,
/// `DuplicateCommentTimestamp` is returned and nothing is written. Catching
/// this at save time keeps the file in a state that `update`/`delete` can
/// still operate on by `created_at` alone.
pub fn save_comment(repo_root: &Path, comment: &Comment) -> Result<()> {
    ensure_review_dir(repo_root)?;
    ensure_ignored(repo_root)?;

    let path = anchor_file(repo_root, comment);
    let key = format_rfc3339(comment.created_at)?;

    let existing = load_file_for_rewrite(&path)?;
    if existing.iter().any(|c| {
        format_rfc3339(c.created_at)
            .map(|ts| ts == key)
            .unwrap_or(false)
    }) {
        return Err(JjrError::DuplicateCommentTimestamp { timestamp: key });
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| JjrError::Io { source })?;

    let line = serde_json::to_string(comment).map_err(|e| JjrError::Io {
        source: io::Error::other(e),
    })?;
    writeln!(file, "{line}").map_err(|source| JjrError::Io { source })?;
    Ok(())
}

/// Errors if the timestamp is not found or if two records share the same timestamp.
pub fn update_comment(repo_root: &Path, comment: &Comment) -> Result<()> {
    let path = anchor_file(repo_root, comment);
    let key = format_rfc3339(comment.created_at)?;
    let existing = load_file_for_rewrite(&path)?;
    let updated = replace_by_timestamp(existing, &key, comment, &path)?;
    write_file(&path, &updated)
}

/// Removes the record identified by `created_at` from the JSONL file resolved
/// from `comment`'s anchor (line and change scope route to the change file;
/// stack scope routes to `_stack.jsonl`).
///
/// Taking `&Comment` rather than `&ChangeId` ensures stack-scoped comments —
/// which have no `change_id` — can be deleted via the same entry point.
pub fn delete_comment(repo_root: &Path, comment: &Comment) -> Result<()> {
    let path = anchor_file(repo_root, comment);
    let key = format_rfc3339(comment.created_at)?;
    let existing = load_file_for_rewrite(&path)?;
    let updated = delete_by_timestamp(existing, &key, &path)?;
    write_file(&path, &updated)
}

fn load_jsonl_file(path: &Path) -> Result<Vec<Comment>> {
    let file = std::fs::File::open(path).map_err(|source| JjrError::Io { source })?;
    let reader = io::BufReader::new(file);

    let mut comments = Vec::new();
    for (idx, result) in reader.lines().enumerate() {
        let line = match result {
            Ok(l) => l,
            Err(e) => {
                log_warning(&format!(
                    "skipping unreadable line {} in {}: {e}",
                    idx + 1,
                    path.display()
                ));
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_record(trimmed, path) {
            Ok(c) => comments.push(c),
            Err(e @ JjrError::SchemaVersionMismatch { .. }) => return Err(e),
            Err(e) => {
                log_warning(&format!(
                    "skipping malformed line {} in {}: {e}",
                    idx + 1,
                    path.display()
                ));
            }
        }
    }
    Ok(comments)
}

fn load_file_for_rewrite(path: &Path) -> Result<Vec<Comment>> {
    if path.exists() {
        load_jsonl_file(path)
    } else {
        Ok(Vec::new())
    }
}

fn parse_record(line: &str, path: &Path) -> Result<Comment> {
    let raw: serde_json::Value = serde_json::from_str(line).map_err(|e| JjrError::Io {
        source: io::Error::other(format!("JSON parse error in {}: {e}", path.display())),
    })?;

    // Schema version is checked manually before delegating to serde so that
    // a wrong-version record returns the typed `SchemaVersionMismatch` error
    // (a hard error, propagated up to fail the load) instead of a generic
    // serde parse failure (which would be logged and the line skipped). A
    // record with no `schema_version` field at all is also a mismatch — not
    // a malformed line — because the spec requires the field to be present.
    match raw.get("schema_version").and_then(|v| v.as_str()) {
        Some(v) if v == SCHEMA_VERSION_VALUE => {}
        Some(other) => {
            return Err(JjrError::SchemaVersionMismatch {
                found: other.to_owned(),
                expected: SCHEMA_VERSION_VALUE.to_owned(),
            });
        }
        None => {
            return Err(JjrError::SchemaVersionMismatch {
                found: "(missing)".to_owned(),
                expected: SCHEMA_VERSION_VALUE.to_owned(),
            });
        }
    }

    serde_json::from_value(raw).map_err(|e| JjrError::Io {
        source: io::Error::other(format!(
            "comment deserialize error in {}: {e}",
            path.display()
        )),
    })
}

fn replace_by_timestamp(
    mut existing: Vec<Comment>,
    key: &str,
    replacement: &Comment,
    path: &Path,
) -> Result<Vec<Comment>> {
    let indices: Vec<usize> = existing
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            format_rfc3339(c.created_at)
                .map(|ts| ts == key)
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    match indices.as_slice() {
        [] => Err(JjrError::CommentNotFound {
            file: path.to_owned(),
            timestamp: key.to_owned(),
        }),
        [index] => {
            existing[*index] = replacement.clone();
            Ok(existing)
        }
        _ => Err(JjrError::DuplicateCommentTimestamp {
            timestamp: key.to_owned(),
        }),
    }
}

fn delete_by_timestamp(existing: Vec<Comment>, key: &str, path: &Path) -> Result<Vec<Comment>> {
    let match_count = existing
        .iter()
        .filter(|c| {
            format_rfc3339(c.created_at)
                .map(|ts| ts == key)
                .unwrap_or(false)
        })
        .count();

    match match_count {
        0 => Err(JjrError::CommentNotFound {
            file: path.to_owned(),
            timestamp: key.to_owned(),
        }),
        1 => {
            let kept = existing
                .into_iter()
                .filter(|c| {
                    format_rfc3339(c.created_at)
                        .map(|ts| ts != key)
                        .unwrap_or(true)
                })
                .collect();
            Ok(kept)
        }
        _ => Err(JjrError::DuplicateCommentTimestamp {
            timestamp: key.to_owned(),
        }),
    }
}

fn write_file(path: &Path, comments: &[Comment]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|source| JjrError::Io { source })?;

    for comment in comments {
        let line = serde_json::to_string(comment).map_err(|e| JjrError::Io {
            source: io::Error::other(e),
        })?;
        writeln!(file, "{line}").map_err(|source| JjrError::Io { source })?;
    }
    Ok(())
}

fn log_warning(msg: &str) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "warning: {msg}");
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use time::macros::datetime;
    use time::OffsetDateTime;

    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::comment::{
        Anchor, LineAnchor, SchemaVersion, Severity, Side, Status, BAD_V1_FIXTURE,
    };

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn make_line_comment(change_id: ChangeId, created_at: OffsetDateTime, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id,
                location: LineAnchor {
                    file: PathBuf::from("src/foo.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(42),
                    hunk_header: "@@ -1,3 +1,4 @@".to_owned(),
                    target_text: "target line".to_owned(),
                    context_before: vec!["before".to_owned()],
                    context_after: vec!["after".to_owned()],
                },
            },
            repo_root: PathBuf::new(),
            revset: "@".to_owned(),
            commit_id: Some(CommitId::parse("deadbeef").unwrap()),
            body: body.to_owned(),
            severity: Severity::Suggestion,
            created_at,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    fn rooted(mut comment: Comment, root: &Path) -> Comment {
        comment.repo_root = root.to_owned();
        comment
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp();
        let id = cid("abc12345");
        let comment = rooted(
            make_line_comment(id.clone(), datetime!(2026-04-29 14:00:00 UTC), "body"),
            dir.path(),
        );

        save_comment(dir.path(), &comment).unwrap();
        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "body");
    }

    #[test]
    fn save_multiple_load_returns_all() {
        let dir = tmp();
        let id = cid("abc12345");
        let c1 = rooted(
            make_line_comment(id.clone(), datetime!(2026-04-29 14:00:00 UTC), "first"),
            dir.path(),
        );
        let c2 = rooted(
            make_line_comment(id.clone(), datetime!(2026-04-29 14:01:00 UTC), "second"),
            dir.path(),
        );

        save_comment(dir.path(), &c1).unwrap();
        save_comment(dir.path(), &c2).unwrap();

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 2);
        let bodies: Vec<&str> = loaded.iter().map(|c| c.body.as_str()).collect();
        assert!(bodies.contains(&"first"));
        assert!(bodies.contains(&"second"));
    }

    #[test]
    fn load_nonexistent_file_returns_empty_vec() {
        let dir = tmp();
        let id = cid("abc12345");
        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn update_changes_body_preserves_created_at() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let original = rooted(make_line_comment(id.clone(), ts, "original"), dir.path());
        save_comment(dir.path(), &original).unwrap();

        let updated = Comment {
            body: "updated body".to_owned(),
            severity: Severity::Required,
            ..original
        };
        update_comment(dir.path(), &updated).unwrap();

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "updated body");
        assert_eq!(loaded[0].severity, Severity::Required);
        assert_eq!(loaded[0].created_at, ts);
    }

    #[test]
    fn update_nonexistent_comment_errors() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id, ts, "body"), dir.path());

        let result = update_comment(dir.path(), &comment);
        assert!(
            result.is_err(),
            "expected error updating non-existent comment"
        );
        assert!(matches!(result, Err(JjrError::CommentNotFound { .. })));
    }

    #[test]
    fn delete_removes_target_preserves_others() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let c1 = rooted(make_line_comment(id.clone(), ts1, "first"), dir.path());
        let c2 = rooted(make_line_comment(id.clone(), ts2, "second"), dir.path());
        save_comment(dir.path(), &c1).unwrap();
        save_comment(dir.path(), &c2).unwrap();

        delete_comment(dir.path(), &c1).unwrap();

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "second");
    }

    #[test]
    fn delete_nonexistent_errors() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id, ts, "body"), dir.path());
        save_comment(dir.path(), &comment).unwrap();

        let other_ts = datetime!(2026-04-29 15:00:00 UTC);
        // Build a sibling comment whose anchor routes to the same file but
        // whose `created_at` does not exist on disk.
        let phantom = Comment {
            created_at: other_ts,
            ..comment
        };
        let result = delete_comment(dir.path(), &phantom);
        assert!(
            result.is_err(),
            "expected error deleting non-existent comment"
        );
        assert!(matches!(result, Err(JjrError::CommentNotFound { .. })));
    }

    // -- B: Stack-scoped delete routes to _stack.jsonl --

    fn make_stack_comment(created_at: OffsetDateTime, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack,
            repo_root: PathBuf::new(),
            revset: "main..@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Note,
            created_at,
            updated_at: None,
            status: None,
            mismatch_reason: None,
        }
    }

    #[test]
    fn save_and_delete_stack_scoped_comment() {
        let dir = tmp();
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let mut comment = make_stack_comment(ts, "rename retry_wrapper to retry_policy");
        comment.repo_root = dir.path().to_owned();

        save_comment(dir.path(), &comment).unwrap();

        let stack_path = dir
            .path()
            .join(".jj-review")
            .join("comments")
            .join(STACK_FILENAME);
        assert!(stack_path.exists(), "stack jsonl should be created");
        let raw_before = std::fs::read_to_string(&stack_path).unwrap();
        assert!(raw_before.contains("rename retry_wrapper"));

        delete_comment(dir.path(), &comment).unwrap();

        let raw_after = std::fs::read_to_string(&stack_path).unwrap();
        assert!(
            !raw_after.contains("rename retry_wrapper"),
            "deleted stack comment should not appear in _stack.jsonl; got: {raw_after}"
        );
    }

    #[test]
    fn malformed_line_skipped_valid_records_returned() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id.clone(), ts, "good"), dir.path());

        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        let good_line = serde_json::to_string(&comment).unwrap();
        std::fs::write(&path, format!("{{not valid json}}\n{good_line}\n")).unwrap();

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "good");
    }

    #[test]
    fn schema_version_mismatch_returns_error() {
        let dir = tmp();
        let id = cid("abc12345");
        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        std::fs::write(&path, format!("{BAD_V1_FIXTURE}\n")).unwrap();

        let result = load_change_comments(dir.path(), &id);
        assert!(matches!(
            result,
            Err(JjrError::SchemaVersionMismatch { .. })
        ));
    }

    // -- C2: missing schema_version is a hard error, not a silent skip --

    #[test]
    fn missing_schema_version_returns_hard_error() {
        let dir = tmp();
        let id = cid("abc12345");
        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        // No `schema_version` field at all.
        let bad = r#"{"scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        std::fs::write(&path, format!("{bad}\n")).unwrap();

        let result = load_change_comments(dir.path(), &id);
        match result {
            Err(JjrError::SchemaVersionMismatch { found, .. }) => {
                assert_eq!(found, "(missing)");
            }
            other => panic!("expected SchemaVersionMismatch with found=(missing); got {other:?}"),
        }
    }

    // -- E5: store-level v1 backward compatibility (no `scope` field) --

    #[test]
    fn v1_record_without_scope_loads_via_store() {
        let dir = tmp();
        let id = cid("abc12345");
        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        let v1 = r#"{"schema_version":"diff-comment/v2","change_id":"abc12345","repo_root":"/w","revset":"@","file":"src/foo.rs","side":"new","new_line":42,"hunk_header":"@@","target_text":"t","comment":"v1 body","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        std::fs::write(&path, format!("{v1}\n")).unwrap();

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(loaded[0].anchor, Anchor::Line { .. }));
        assert_eq!(loaded[0].body, "v1 body");
    }

    // -- E1: DuplicateCommentTimestamp is reachable on update and delete --

    #[test]
    fn update_with_duplicate_timestamp_in_file_errors() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        // Manually write two records sharing the same `created_at` (bypassing
        // `save_comment`'s uniqueness guard) so we can exercise the error
        // arm in `replace_by_timestamp`.
        let c = rooted(make_line_comment(id.clone(), ts, "first"), dir.path());
        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        let line = serde_json::to_string(&c).unwrap();
        std::fs::write(&path, format!("{line}\n{line}\n")).unwrap();

        let updated = Comment {
            body: "updated".to_owned(),
            ..c
        };
        let result = update_comment(dir.path(), &updated);
        assert!(matches!(
            result,
            Err(JjrError::DuplicateCommentTimestamp { .. })
        ));
    }

    #[test]
    fn delete_with_duplicate_timestamp_in_file_errors() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let c = rooted(make_line_comment(id.clone(), ts, "dup"), dir.path());
        ensure_review_dir(dir.path()).unwrap();
        let path = change_file(dir.path(), &id);
        let line = serde_json::to_string(&c).unwrap();
        std::fs::write(&path, format!("{line}\n{line}\n")).unwrap();

        let result = delete_comment(dir.path(), &c);
        assert!(matches!(
            result,
            Err(JjrError::DuplicateCommentTimestamp { .. })
        ));
    }

    // -- H: pre-save uniqueness check fires DuplicateCommentTimestamp --

    #[test]
    fn save_with_duplicate_timestamp_errors_before_writing() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let c1 = rooted(make_line_comment(id.clone(), ts, "first"), dir.path());
        save_comment(dir.path(), &c1).unwrap();

        let c2 = rooted(make_line_comment(id.clone(), ts, "second"), dir.path());
        let result = save_comment(dir.path(), &c2);
        assert!(matches!(
            result,
            Err(JjrError::DuplicateCommentTimestamp { .. })
        ));

        // First record is the only one; second was rejected before writing.
        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "first");
    }

    #[test]
    fn ensure_review_dir_creates_directory() {
        let dir = tmp();
        let expected = dir.path().join(".jj-review").join("comments");
        assert!(!expected.exists());

        ensure_review_dir(dir.path()).unwrap();
        assert!(expected.is_dir());
    }

    #[test]
    fn ensure_review_dir_is_idempotent() {
        let dir = tmp();
        ensure_review_dir(dir.path()).unwrap();
        ensure_review_dir(dir.path()).unwrap();
        assert!(dir.path().join(".jj-review").join("comments").is_dir());
    }

    #[test]
    fn ensure_ignored_adds_entry_to_gitignore() {
        let dir = tmp();
        ensure_ignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("/.jj-review"));
    }

    #[test]
    fn ensure_ignored_adds_entry_to_jjignore() {
        let dir = tmp();
        ensure_ignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".jjignore")).unwrap();
        assert!(content.contains("/.jj-review"));
    }

    #[test]
    fn ensure_ignored_does_not_duplicate_entries() {
        let dir = tmp();
        ensure_ignored(dir.path()).unwrap();
        ensure_ignored(dir.path()).unwrap();

        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        let count = gitignore.lines().filter(|l| *l == "/.jj-review").count();
        assert_eq!(count, 1);

        let jjignore = std::fs::read_to_string(dir.path().join(".jjignore")).unwrap();
        let count = jjignore.lines().filter(|l| *l == "/.jj-review").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn ensure_ignored_appends_to_existing_file_with_content() {
        let dir = tmp();
        let gitignore_path = dir.path().join(".gitignore");
        std::fs::write(&gitignore_path, "*.log\n/target\n").unwrap();

        ensure_ignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(&gitignore_path).unwrap();
        assert!(content.contains("*.log"));
        assert!(content.contains("/target"));
        assert!(content.contains("/.jj-review"));
    }

    // -- E6: ensure_entry_in_file's newline-guard branch --

    #[test]
    fn ensure_ignored_inserts_separator_when_file_lacks_trailing_newline() {
        let dir = tmp();
        let gitignore_path = dir.path().join(".gitignore");
        // No trailing newline — should trigger the writeln! guard.
        std::fs::write(&gitignore_path, "*.log").unwrap();

        ensure_ignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(&gitignore_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(
            lines.contains(&"*.log"),
            "original content preserved on its own line; got: {content:?}"
        );
        assert!(
            lines.contains(&"/.jj-review"),
            "new entry on its own line; got: {content:?}"
        );
    }

    #[test]
    fn divergent_change_id_uses_underscored_filename() {
        let dir = tmp();
        let id = cid("abc11111/1");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id.clone(), ts, "divergent"), dir.path());

        save_comment(dir.path(), &comment).unwrap();

        let expected_path = dir
            .path()
            .join(".jj-review")
            .join("comments")
            .join("abc11111_1.jsonl");
        assert!(
            expected_path.exists(),
            "expected file at {}",
            expected_path.display()
        );

        let loaded = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "divergent");

        let raw = std::fs::read_to_string(&expected_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(
            v["change_id"], "abc11111/1",
            "canonical change_id must be preserved in JSON"
        );
    }
}
