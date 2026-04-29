use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use time::OffsetDateTime;

use crate::change_id::ChangeId;
use crate::error::{JjrError, Result};
use crate::stack::RevsetHash;

/// Persistent cursor tracking the last-viewed change per revset.
///
/// Stored at `.jj-review/cursor.json`. The key is a lowercase hex encoding of
/// the BLAKE3 hash of the canonicalized revset string so the file is human-
/// readable without requiring the reviewer to know the hash algorithm.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub revsets: BTreeMap<String, RevsetCursor>,
}

/// Per-revset cursor entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevsetCursor {
    /// The original revset string, for human readability.
    pub revset: String,
    /// The last change the reviewer viewed or advanced to.
    pub last_change_id: ChangeId,
    /// When this entry was last updated (RFC 3339).
    pub updated_at: OffsetDateTime,
}

fn cursor_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".jj-review").join("cursor.json")
}

/// Load the cursor file from `repo_root/.jj-review/cursor.json`.
///
/// Returns an empty `Cursor` if the file does not exist.
pub fn load(repo_root: &Path) -> Result<Cursor> {
    let path = cursor_path(repo_root);
    if !path.exists() {
        return Ok(Cursor::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|source| JjrError::Io { source })?;
    serde_json::from_str(&raw).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })
}

/// Save the cursor to `repo_root/.jj-review/cursor.json` atomically.
///
/// `NamedTempFile::persist` writes to a randomized sibling and renames into
/// place — so a crash mid-write cannot corrupt the existing file, and the
/// temp file is cleaned up on drop if `persist` is never called.
pub fn save(repo_root: &Path, cursor: &Cursor) -> Result<()> {
    let dir = repo_root.join(".jj-review");
    std::fs::create_dir_all(&dir).map_err(|source| JjrError::Io { source })?;

    let path = cursor_path(repo_root);
    let json = serde_json::to_string_pretty(cursor).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })?;

    let mut tmp = NamedTempFile::new_in(&dir).map_err(|source| JjrError::Io { source })?;
    tmp.write_all(json.as_bytes())
        .map_err(|source| JjrError::Io { source })?;
    tmp.flush().map_err(|source| JjrError::Io { source })?;
    tmp.persist(&path).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })?;
    Ok(())
}

/// Update the cursor for a single revset entry and persist.
///
/// Convenience wrapper around `load` + mutate + `save`.
pub(crate) fn record(
    repo_root: &Path,
    hash: RevsetHash,
    revset: &str,
    last_change_id: &ChangeId,
) -> Result<()> {
    let mut cursor = load(repo_root)?;
    cursor.revsets.insert(
        hash.hex(),
        RevsetCursor {
            revset: revset.to_owned(),
            last_change_id: last_change_id.clone(),
            updated_at: OffsetDateTime::now_utc(),
        },
    );
    save(repo_root, &cursor)
}

/// Remove the cursor entry for a revset hash. Used by `--restart`.
pub(crate) fn clear(repo_root: &Path, hash: RevsetHash) -> Result<()> {
    let mut cursor = load(repo_root)?;
    cursor.revsets.remove(&hash.hex());
    save(repo_root, &cursor)
}

/// Resolve the resume index for a stack given the cursor file.
///
/// Shell wrapper: loads `cursor.json`, then delegates to the pure
/// [`resume_index_from_cursor`]. On any load error, returns 0 (open at the
/// oldest change) — same fallback as a missing file.
pub(crate) fn resume_index(
    repo_root: &Path,
    hash: RevsetHash,
    stack_change_ids: &[ChangeId],
    has_comments: &dyn Fn(&ChangeId) -> bool,
) -> usize {
    let Ok(cursor) = load(repo_root) else {
        return 0;
    };
    resume_index_from_cursor(&cursor, hash, stack_change_ids, has_comments)
}

/// Pure resume-rule implementation.
///
/// Returns the 0-based index into `stack_change_ids` to open first:
///
/// 1. If a cursor exists for `hash` and `last_change_id` is in the stack:
///    - If `has_comments(last_change_id)` is false, return that position
///      (the user landed on a change but never commented — resume there).
///    - Otherwise scan forward for the first unreviewed change after it.
///    - If every change at or after the cursor has comments, return the
///      cursor position (treat the stack as fully reviewed; reopen there).
/// 2. Otherwise return 0 (open at the oldest change).
pub(crate) fn resume_index_from_cursor(
    cursor: &Cursor,
    hash: RevsetHash,
    stack_change_ids: &[ChangeId],
    has_comments: &dyn Fn(&ChangeId) -> bool,
) -> usize {
    let Some(entry) = cursor.revsets.get(&hash.hex()) else {
        return 0;
    };

    let Some(last_pos) = stack_change_ids
        .iter()
        .position(|id| id == &entry.last_change_id)
    else {
        return 0;
    };

    // The cursor change itself qualifies if the reviewer never commented on it.
    if !has_comments(&stack_change_ids[last_pos]) {
        return last_pos;
    }

    // Scan forward for the first unreviewed change.
    let next = stack_change_ids[last_pos + 1..]
        .iter()
        .position(|id| !has_comments(id))
        .map(|offset| last_pos + 1 + offset);

    next.unwrap_or(last_pos)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use time::macros::datetime;

    use super::*;

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn make_cursor_with_entry(revset: &str, last: ChangeId) -> Cursor {
        let hash = RevsetHash::from_revset(revset);
        let mut c = Cursor::default();
        c.revsets.insert(
            hash.hex(),
            RevsetCursor {
                revset: revset.to_owned(),
                last_change_id: last,
                updated_at: datetime!(2026-04-29 14:00:00 UTC),
            },
        );
        c
    }

    #[test]
    fn load_missing_file_returns_empty_cursor() {
        let dir = tmp();
        let cursor = load(dir.path()).unwrap();
        assert!(cursor.revsets.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_err() {
        let dir = tmp();
        let path = dir.path().join(".jj-review");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("cursor.json"), b"{not json").unwrap();
        assert!(load(dir.path()).is_err());
    }

    #[test]
    fn resume_index_silently_returns_zero_on_corrupt_json() {
        let dir = tmp();
        let path = dir.path().join(".jj-review");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("cursor.json"), b"{not json").unwrap();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222")];
        let idx = resume_index(dir.path(), hash, &ids, &|_| false);
        assert_eq!(idx, 0);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let mut cursor = Cursor::default();
        cursor.revsets.insert(
            hash.hex(),
            RevsetCursor {
                revset: "@".to_owned(),
                last_change_id: cid("abc12345"),
                updated_at: datetime!(2026-04-29 14:00:00 UTC),
            },
        );
        save(dir.path(), &cursor).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.revsets.len(), 1);
        let entry = &loaded.revsets[&hash.hex()];
        assert_eq!(entry.revset, "@");
        assert_eq!(entry.last_change_id, cid("abc12345"));
    }

    #[test]
    fn record_creates_and_updates_entry() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        record(dir.path(), hash, "@", &cid("abc12345")).unwrap();

        let loaded = load(dir.path()).unwrap();
        let entry = &loaded.revsets[&hash.hex()];
        assert_eq!(entry.last_change_id, cid("abc12345"));

        // Update to a different change.
        record(dir.path(), hash, "@", &cid("def99999")).unwrap();
        let loaded2 = load(dir.path()).unwrap();
        let entry2 = &loaded2.revsets[&hash.hex()];
        assert_eq!(entry2.last_change_id, cid("def99999"));
    }

    #[test]
    fn clear_removes_entry() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        record(dir.path(), hash, "@", &cid("abc12345")).unwrap();
        clear(dir.path(), hash).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert!(loaded.revsets.is_empty());
    }

    #[test]
    fn clear_on_missing_file_is_ok() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        clear(dir.path(), hash).unwrap();
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let dir = tmp();
        let cursor = Cursor::default();
        save(dir.path(), &cursor).unwrap();
        let review_dir = dir.path().join(".jj-review");
        assert!(review_dir.join("cursor.json").exists());
        // NamedTempFile uses randomized names; verify nothing other than
        // cursor.json remains in the directory after persist.
        let extras: Vec<_> = std::fs::read_dir(&review_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "cursor.json")
            .collect();
        assert!(extras.is_empty(), "stray files left in dir: {extras:?}");
    }

    #[test]
    fn resume_index_no_cursor_returns_zero() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let idx = resume_index(dir.path(), hash, &ids, &|_| false);
        assert_eq!(idx, 0);
    }

    #[test]
    fn resume_index_last_not_in_stack_returns_zero() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let cursor = make_cursor_with_entry("@", cid("ffffffff"));
        save(dir.path(), &cursor).unwrap();
        let ids = vec![cid("abc11111"), cid("abc22222")];
        let idx = resume_index(dir.path(), hash, &ids, &|_| false);
        assert_eq!(idx, 0);
    }

    #[test]
    fn resume_index_no_comments_after_last_returns_last_pos() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[1].clone());
        save(dir.path(), &cursor).unwrap();
        // All changes have comments — the cursor change qualifies under the
        // first rule (its own check) only if it has no comments. With every
        // change carrying comments, the resume rule falls through to the
        // last-resort and returns last_pos=1.
        let idx = resume_index(dir.path(), hash, &ids, &|_| true);
        assert_eq!(idx, 1);
    }

    #[test]
    fn resume_index_finds_next_unreviewed_after_cursor() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![
            cid("abc11111"),
            cid("abc22222"),
            cid("abc33333"),
            cid("abc44444"),
        ];
        let cursor = make_cursor_with_entry("@", ids[1].clone());
        save(dir.path(), &cursor).unwrap();
        // ids[1] has comments, ids[2] does not → next unreviewed is index 2.
        let no_comment_id = ids[2].clone();
        let idx = resume_index(dir.path(), hash, &ids, &|id| id != &no_comment_id);
        assert_eq!(idx, 2);
    }

    #[test]
    fn resume_index_cursor_at_last_entry_returns_last() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[2].clone());
        save(dir.path(), &cursor).unwrap();
        // has_comments=false everywhere → resume at the cursor change itself
        // (per the corrected resume rule: cursor change qualifies if it has
        // no comments yet).
        let idx = resume_index(dir.path(), hash, &ids, &|_| false);
        assert_eq!(idx, 2);
    }

    #[test]
    fn resume_index_cursor_change_has_no_comments_resumes_there() {
        // The corrected-rule case: cursor at index 1, ids[1] has no comments,
        // all later have comments → returns 1 (do not skip past the cursor).
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[1].clone());
        save(dir.path(), &cursor).unwrap();
        let cursor_id = ids[1].clone();
        let idx = resume_index(dir.path(), hash, &ids, &|id| id != &cursor_id);
        assert_eq!(idx, 1, "cursor change with no comments should resume there");
    }
}
