use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::change_id::ChangeId;
use crate::comment::{format_rfc3339, Anchor, Comment, Status, SCHEMA_VERSION_VALUE};
use crate::error::{JjrError, Result};
use crate::stack::ResolvedStack;

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

/// Write `comments` to `path` atomically.
///
/// `NamedTempFile::persist` writes to a randomized sibling in the same
/// directory and renames into place — so a crash mid-write cannot corrupt
/// the existing file, and the temp file is cleaned up on drop if `persist`
/// is never called. Same-directory placement guarantees the rename stays on
/// one filesystem (cross-device renames would fail).
fn write_file(path: &Path, comments: &[Comment]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| JjrError::Io {
        source: io::Error::other(format!("path has no parent directory: {}", path.display())),
    })?;

    let mut tmp = NamedTempFile::new_in(dir).map_err(|source| JjrError::Io { source })?;
    for comment in comments {
        let line = serde_json::to_string(comment).map_err(|e| JjrError::Io {
            source: io::Error::other(e),
        })?;
        writeln!(tmp, "{line}").map_err(|source| JjrError::Io { source })?;
    }
    tmp.flush().map_err(|source| JjrError::Io { source })?;
    tmp.persist(path).map_err(|e| JjrError::Io {
        source: io::Error::other(e),
    })?;
    Ok(())
}

fn log_warning(msg: &str) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "warning: {msg}");
}

/// Aggregate result of a stale-comment clear pass.
pub struct ClearStats {
    pub changes_touched: usize,
    pub comments_removed: usize,
}

/// `_stack.jsonl` is not touched.
pub fn clear_stale_for_change(repo_root: &Path, change_id: &ChangeId) -> Result<usize> {
    let path = change_file(repo_root, change_id);
    let all = load_file_for_rewrite(&path)?;
    let total = all.len();
    let kept: Vec<Comment> = all
        .into_iter()
        .filter(|c| c.status != Some(Status::Stale))
        .collect();
    let removed = total - kept.len();
    if removed == 0 {
        return Ok(0);
    }
    write_file(&path, &kept)?;
    Ok(removed)
}

pub fn clear_stale_for_stack(repo_root: &Path, stack: &ResolvedStack) -> Result<ClearStats> {
    let mut changes_touched = 0;
    let mut comments_removed = 0;
    for entry in &stack.entries {
        let removed = clear_stale_for_change(repo_root, &entry.change_id)?;
        if removed > 0 {
            changes_touched += 1;
            comments_removed += removed;
        }
    }
    Ok(ClearStats {
        changes_touched,
        comments_removed,
    })
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

    fn make_stale_comment(change_id: ChangeId, created_at: OffsetDateTime, body: &str) -> Comment {
        Comment {
            status: Some(Status::Stale),
            ..make_line_comment(change_id, created_at, body)
        }
    }

    fn make_orphaned_comment(
        change_id: ChangeId,
        created_at: OffsetDateTime,
        body: &str,
    ) -> Comment {
        Comment {
            status: Some(Status::Orphaned),
            ..make_line_comment(change_id, created_at, body)
        }
    }

    fn make_change_scoped_comment(
        change_id: &ChangeId,
        created_at: OffsetDateTime,
        body: &str,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: change_id.clone(),
            },
            repo_root: PathBuf::new(),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Note,
            created_at,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    #[test]
    fn clear_stale_no_jsonl_returns_zero() {
        let dir = tmp();
        let id = cid("abc12345");
        let removed = clear_stale_for_change(dir.path(), &id).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn clear_stale_no_stale_comments_returns_zero() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id.clone(), ts, "pending"), dir.path());
        save_comment(dir.path(), &comment).unwrap();

        let removed = clear_stale_for_change(dir.path(), &id).unwrap();
        assert_eq!(removed, 0);

        let remaining = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn clear_stale_all_stale_removes_all() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let c1 = rooted(make_stale_comment(id.clone(), ts1, "stale1"), dir.path());
        let c2 = rooted(make_stale_comment(id.clone(), ts2, "stale2"), dir.path());
        save_comment(dir.path(), &c1).unwrap();
        save_comment(dir.path(), &c2).unwrap();

        let removed = clear_stale_for_change(dir.path(), &id).unwrap();
        assert_eq!(removed, 2);

        let remaining = load_change_comments(dir.path(), &id).unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn clear_stale_mixed_removes_only_stale_preserves_order() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let ts3 = datetime!(2026-04-29 14:02:00 UTC);
        let pending = rooted(make_line_comment(id.clone(), ts1, "pending"), dir.path());
        let stale = rooted(make_stale_comment(id.clone(), ts2, "stale"), dir.path());
        let pending2 = rooted(make_line_comment(id.clone(), ts3, "pending2"), dir.path());
        save_comment(dir.path(), &pending).unwrap();
        save_comment(dir.path(), &stale).unwrap();
        save_comment(dir.path(), &pending2).unwrap();

        let removed = clear_stale_for_change(dir.path(), &id).unwrap();
        assert_eq!(removed, 1);

        let remaining = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].body, "pending");
        assert_eq!(remaining[1].body, "pending2");
    }

    #[test]
    fn clear_stale_preserves_orphaned_and_change_scoped() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let ts3 = datetime!(2026-04-29 14:02:00 UTC);
        let stale = rooted(make_stale_comment(id.clone(), ts1, "stale"), dir.path());
        let orphaned = rooted(
            make_orphaned_comment(id.clone(), ts2, "orphaned"),
            dir.path(),
        );
        let change_scoped = rooted(
            make_change_scoped_comment(&id, ts3, "change-level"),
            dir.path(),
        );
        save_comment(dir.path(), &stale).unwrap();
        save_comment(dir.path(), &orphaned).unwrap();
        save_comment(dir.path(), &change_scoped).unwrap();

        let removed = clear_stale_for_change(dir.path(), &id).unwrap();
        assert_eq!(removed, 1);

        let remaining = load_change_comments(dir.path(), &id).unwrap();
        assert_eq!(remaining.len(), 2);
        let bodies: Vec<&str> = remaining.iter().map(|c| c.body.as_str()).collect();
        assert!(bodies.contains(&"orphaned"));
        assert!(bodies.contains(&"change-level"));
    }

    fn make_resolved_stack(entries: Vec<(&str, &str)>) -> ResolvedStack {
        use crate::change_id::CommitId;
        use crate::stack::{RevsetHash, StackEntry};
        ResolvedStack {
            revset: "@".to_owned(),
            revset_hash: RevsetHash::from_revset("@"),
            entries: entries
                .into_iter()
                .map(|(cid_str, desc)| StackEntry {
                    change_id: ChangeId::parse(cid_str).unwrap(),
                    commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
                    description: desc.to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn clear_stale_for_stack_empty_stack_returns_zero_stats() {
        let dir = tmp();
        let stack = make_resolved_stack(vec![]);
        let stats = clear_stale_for_stack(dir.path(), &stack).unwrap();
        assert_eq!(stats.changes_touched, 0);
        assert_eq!(stats.comments_removed, 0);
    }

    #[test]
    fn clear_stale_for_stack_one_change_with_stale() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let stale = rooted(make_stale_comment(id.clone(), ts, "stale"), dir.path());
        save_comment(dir.path(), &stale).unwrap();

        let stack = make_resolved_stack(vec![("abc12345", "first")]);
        let stats = clear_stale_for_stack(dir.path(), &stack).unwrap();
        assert_eq!(stats.changes_touched, 1);
        assert_eq!(stats.comments_removed, 1);

        assert!(load_change_comments(dir.path(), &id).unwrap().is_empty());
    }

    #[test]
    fn clear_stale_for_stack_multiple_changes_partial_stale() {
        let dir = tmp();
        let id1 = cid("abc11111");
        let id2 = cid("abc22222");
        let id3 = cid("abc33333");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let ts3 = datetime!(2026-04-29 14:02:00 UTC);

        let pending1 = rooted(make_line_comment(id1.clone(), ts1, "pending"), dir.path());
        save_comment(dir.path(), &pending1).unwrap();

        let stale2 = rooted(make_stale_comment(id2.clone(), ts2, "stale"), dir.path());
        save_comment(dir.path(), &stale2).unwrap();

        let stale3a = rooted(make_stale_comment(id3.clone(), ts3, "stale3a"), dir.path());
        let ts3b = datetime!(2026-04-29 14:03:00 UTC);
        let pending3b = rooted(
            make_line_comment(id3.clone(), ts3b, "pending3b"),
            dir.path(),
        );
        save_comment(dir.path(), &stale3a).unwrap();
        save_comment(dir.path(), &pending3b).unwrap();

        let stack = make_resolved_stack(vec![
            ("abc11111", "first"),
            ("abc22222", "second"),
            ("abc33333", "third"),
        ]);
        let stats = clear_stale_for_stack(dir.path(), &stack).unwrap();
        assert_eq!(stats.changes_touched, 2);
        assert_eq!(stats.comments_removed, 2);

        assert_eq!(
            load_change_comments(dir.path(), &id1).unwrap().len(),
            1,
            "pending on id1 preserved"
        );
        assert!(
            load_change_comments(dir.path(), &id2).unwrap().is_empty(),
            "id2 stale removed"
        );
        let id3_remaining = load_change_comments(dir.path(), &id3).unwrap();
        assert_eq!(id3_remaining.len(), 1);
        assert_eq!(id3_remaining[0].body, "pending3b");
    }

    /// Atomic-rewrite guarantee: every `write_file` caller (here exercised via
    /// `clear_stale_for_change`, `update_comment`, `delete_comment`) must leave
    /// the comments directory free of leftover sibling tempfiles after a
    /// successful operation. `NamedTempFile::persist` removes the temp on
    /// rename, and `Drop` removes it on early exit. A leak here would mean we
    /// regressed back to the truncate-and-overwrite pattern.
    #[test]
    fn write_file_leaves_no_temp_files_in_comments_dir() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts1 = datetime!(2026-04-29 14:00:00 UTC);
        let ts2 = datetime!(2026-04-29 14:01:00 UTC);
        let stale = rooted(make_stale_comment(id.clone(), ts1, "stale"), dir.path());
        let pending = rooted(make_line_comment(id.clone(), ts2, "pending"), dir.path());
        save_comment(dir.path(), &stale).unwrap();
        save_comment(dir.path(), &pending).unwrap();

        // Exercise all three write_file callers.
        let updated = Comment {
            body: "updated".to_owned(),
            ..pending.clone()
        };
        update_comment(dir.path(), &updated).unwrap();
        clear_stale_for_change(dir.path(), &id).unwrap();
        delete_comment(dir.path(), &updated).unwrap();

        let comments_dir = dir.path().join(".jj-review").join("comments");
        let leftovers: Vec<_> = std::fs::read_dir(&comments_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| {
                Path::new(n)
                    .extension()
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("jsonl"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected only .jsonl files in comments dir; found: {leftovers:?}"
        );
    }

    /// If `write_file`'s atomic rename never completes (here simulated by an
    /// unwritable destination via a missing parent path), the original file
    /// must be untouched. This is the load-bearing crash-safety property.
    #[test]
    fn write_file_failure_leaves_original_file_intact() {
        let dir = tmp();
        let id = cid("abc12345");
        let ts = datetime!(2026-04-29 14:00:00 UTC);
        let comment = rooted(make_line_comment(id.clone(), ts, "original"), dir.path());
        save_comment(dir.path(), &comment).unwrap();
        let path = change_file(dir.path(), &id);
        let before = std::fs::read_to_string(&path).unwrap();

        // Force write_file to fail by passing a path whose parent does not
        // exist. NamedTempFile::new_in will reject the missing directory and
        // the function must return an Err without disturbing the real file.
        let bogus = dir.path().join("does/not/exist/sentinel.jsonl");
        let err = write_file(&bogus, std::slice::from_ref(&comment));
        assert!(err.is_err(), "expected Io error from missing parent dir");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "real file must be untouched when write_file errors"
        );
    }
}
