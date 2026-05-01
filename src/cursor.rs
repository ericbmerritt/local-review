use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::change_id::ChangeId;
use crate::error::{JjrError, Result};
use crate::stack::RevsetHash;
use crate::util::atomic_write_bytes;

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
pub fn save(repo_root: &Path, cursor: &Cursor) -> Result<()> {
    let dir = repo_root.join(".jj-review");
    let path = cursor_path(repo_root);
    let json = serde_json::to_string_pretty(cursor).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })?;
    atomic_write_bytes(&dir, &path, json.as_bytes())
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
/// [`resume_index_from_cursor`]. On any load error, falls through to the
/// `is_fully_reviewed`-driven fallback so the reviewer still lands on
/// something useful.
pub(crate) fn resume_index(
    repo_root: &Path,
    hash: RevsetHash,
    stack_change_ids: &[ChangeId],
    has_comments: &dyn Fn(&ChangeId) -> bool,
    is_fully_reviewed: &dyn Fn(&ChangeId) -> bool,
) -> usize {
    let cursor = load(repo_root).unwrap_or_default();
    resume_index_from_cursor(
        &cursor,
        hash,
        stack_change_ids,
        has_comments,
        is_fully_reviewed,
    )
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
/// 2. Otherwise (no cursor entry, or its `last_change_id` left the stack)
///    walk LATEST → OLDEST and return the index of the most-recent change
///    that is NOT fully reviewed. If every change is fully reviewed (or
///    there is no reviewed-state for this stack), return the LATEST index.
///    This rule never falls back to the OLDEST change, so a reviewer
///    returning to a half-reviewed stack lands at the front of the work,
///    not the back.
pub(crate) fn resume_index_from_cursor(
    cursor: &Cursor,
    hash: RevsetHash,
    stack_change_ids: &[ChangeId],
    has_comments: &dyn Fn(&ChangeId) -> bool,
    is_fully_reviewed: &dyn Fn(&ChangeId) -> bool,
) -> usize {
    if let Some(entry) = cursor.revsets.get(&hash.hex()) {
        if let Some(last_pos) = stack_change_ids
            .iter()
            .position(|id| id == &entry.last_change_id)
        {
            // The cursor change itself qualifies if the reviewer never
            // commented on it.
            if !has_comments(&stack_change_ids[last_pos]) {
                return last_pos;
            }

            // Scan forward for the first unreviewed change.
            let next = stack_change_ids[last_pos + 1..]
                .iter()
                .position(|id| !has_comments(id))
                .map(|offset| last_pos + 1 + offset);

            return next.unwrap_or(last_pos);
        }
    }

    smart_resume_index(stack_change_ids, is_fully_reviewed)
}

/// Walk LATEST → OLDEST and pick the most-recent change that is NOT fully
/// reviewed. Falls back to LATEST when every change is reviewed (or the
/// reviewed-state is empty), never to OLDEST. Returns 0 only when the stack
/// itself is empty — a degenerate case the caller should already have ruled
/// out before opening the TUI.
fn smart_resume_index(
    stack_change_ids: &[ChangeId],
    is_fully_reviewed: &dyn Fn(&ChangeId) -> bool,
) -> usize {
    if stack_change_ids.is_empty() {
        return 0;
    }
    let latest = stack_change_ids.len() - 1;
    stack_change_ids
        .iter()
        .enumerate()
        .rev()
        .find(|(_, id)| !is_fully_reviewed(id))
        .map_or(latest, |(idx, _)| idx)
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
    fn resume_index_falls_back_to_smart_rule_on_corrupt_json() {
        // Corrupt cursor.json must not blow up the TUI. Old behavior was to
        // return 0 (oldest); new behavior delegates to the smart fallback,
        // which lands on the LATEST change when nothing is fully reviewed.
        let dir = tmp();
        let path = dir.path().join(".jj-review");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("cursor.json"), b"{not json").unwrap();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222")];
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|_| false);
        assert_eq!(idx, ids.len() - 1, "smart fallback should pick LATEST");
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
    fn resume_index_no_cursor_falls_back_to_latest_when_unreviewed() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        // No cursor file → smart fallback → walk LATEST→OLDEST. Nothing is
        // reviewed, so the LATEST (index 2) is the most-recent unreviewed.
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|_| false);
        assert_eq!(idx, ids.len() - 1);
    }

    #[test]
    fn resume_index_no_cursor_picks_latest_unreviewed_skipping_reviewed_top() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![
            cid("abc11111"),
            cid("abc22222"),
            cid("abc33333"),
            cid("abc44444"),
        ];
        // Index 3 (LATEST) is fully reviewed → smart-resume walks back to
        // index 2 as the most-recent unreviewed change.
        let reviewed_id = ids[3].clone();
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|id| id == &reviewed_id);
        assert_eq!(idx, 2, "must walk back from LATEST to first unreviewed");
    }

    #[test]
    fn resume_index_no_cursor_all_reviewed_returns_latest() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|_| true);
        assert_eq!(
            idx,
            ids.len() - 1,
            "fully-reviewed stack reopens at LATEST, not OLDEST"
        );
    }

    #[test]
    fn resume_index_last_not_in_stack_uses_smart_fallback() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        // Cursor's last_change_id `ffffffff` is gone (rebased / abandoned).
        let cursor = make_cursor_with_entry("@", cid("ffffffff"));
        save(dir.path(), &cursor).unwrap();
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        // Index 2 is reviewed → smart fallback picks index 1 (latest unreviewed).
        let reviewed_id = ids[2].clone();
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|id| id == &reviewed_id);
        assert_eq!(idx, 1);
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
        let idx = resume_index(dir.path(), hash, &ids, &|_| true, &|_| false);
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
        let idx = resume_index(dir.path(), hash, &ids, &|id| id != &no_comment_id, &|_| {
            false
        });
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
        let idx = resume_index(dir.path(), hash, &ids, &|_| false, &|_| false);
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
        let idx = resume_index(dir.path(), hash, &ids, &|id| id != &cursor_id, &|_| false);
        assert_eq!(idx, 1, "cursor change with no comments should resume there");
    }

    #[test]
    fn smart_resume_empty_stack_returns_zero() {
        let idx = smart_resume_index(&[], &|_| false);
        assert_eq!(idx, 0);
    }

    #[test]
    fn smart_resume_walks_latest_to_oldest() {
        let ids = vec![
            cid("abc11111"),
            cid("abc22222"),
            cid("abc33333"),
            cid("abc44444"),
        ];
        // ids[3] fully reviewed, ids[2] not → returns 2.
        let reviewed = ids[3].clone();
        let idx = smart_resume_index(&ids, &|id| id == &reviewed);
        assert_eq!(idx, 2);
    }
}
