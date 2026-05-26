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
/// Stored under `data_home` at `repos/<repo>/cursor.json`. The key is a
/// lowercase hex encoding of the BLAKE3 hash of the canonicalized revset
/// string so the file is human-readable without requiring the reviewer to
/// know the hash algorithm.
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

fn cursor_path(data_home: &Path, repo_root: &Path) -> PathBuf {
    crate::store::repo_data_dir(data_home, repo_root).join("cursor.json")
}

/// Load the cursor file from the XDG data home.
///
/// Returns an empty `Cursor` if the file does not exist.
pub fn load(data_home: &Path, repo_root: &Path) -> Result<Cursor> {
    let path = cursor_path(data_home, repo_root);
    if !path.exists() {
        return Ok(Cursor::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|source| JjrError::Io { source })?;
    serde_json::from_str(&raw).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })
}

/// Save the cursor to the XDG data home atomically.
pub fn save(data_home: &Path, repo_root: &Path, cursor: &Cursor) -> Result<()> {
    let path = cursor_path(data_home, repo_root);
    let json = serde_json::to_string_pretty(cursor).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })?;
    atomic_write_bytes(&path, json.as_bytes())
}

/// Update the cursor for a single revset entry and persist.
///
/// Convenience wrapper around `load` + mutate + `save`.
pub(crate) fn record(
    data_home: &Path,
    repo_root: &Path,
    hash: RevsetHash,
    revset: &str,
    last_change_id: &ChangeId,
) -> Result<()> {
    let mut cursor = load(data_home, repo_root)?;
    cursor.revsets.insert(
        hash.hex(),
        RevsetCursor {
            revset: revset.to_owned(),
            last_change_id: last_change_id.clone(),
            updated_at: OffsetDateTime::now_utc(),
        },
    );
    save(data_home, repo_root, &cursor)
}

/// Remove the cursor entry for a revset hash. Used by `--restart`.
pub(crate) fn clear(data_home: &Path, repo_root: &Path, hash: RevsetHash) -> Result<()> {
    let mut cursor = load(data_home, repo_root)?;
    cursor.revsets.remove(&hash.hex());
    save(data_home, repo_root, &cursor)
}

/// Whether the current stack carries any persisted reviewed-state.
///
/// Captured as a named two-state enum (rather than a bool) because the
/// resume-rule dispatch reads more clearly with `Fresh` / `Partial` arms,
/// and the type system catches future callers that would otherwise pass
/// the wrong boolean for the wrong reason (empty-stack guard, error path,
/// etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StackReviewState {
    /// No change in the resolved stack has any entry in `reviewed.json`.
    /// The reviewer has never touched this stack — land at OLDEST so the
    /// natural `n` flow walks bottom→top.
    Fresh,
    /// At least one change in the resolved stack has a reviewed-state entry
    /// (regardless of `commit_id` match or completeness). The reviewer is
    /// mid-review — walk LATEST→OLDEST and pick up the most-recent
    /// unreviewed change.
    Partial,
}

/// Bundle of resume-rule signals consumed by [`resume_index`] and
/// [`resume_index_from_cursor`].
///
/// Each field documents one independent input to the rule. Future signals
/// add a field; the function signatures stay stable.
pub(crate) struct ResumeInputs<'a> {
    /// The resolved stack, oldest-first. Empty stack is a degenerate case
    /// the caller should rule out before opening the TUI; both resume
    /// functions defensively return 0 anyway.
    pub stack_change_ids: &'a [ChangeId],
    /// Predicate: does this `change_id` already carry any persisted
    /// comments? Drives the cursor-branch "did the reviewer engage?" check.
    pub has_comments: &'a dyn Fn(&ChangeId) -> bool,
    /// Predicate: is this `change_id` fully reviewed at its current
    /// `commit_id` (description + every diff path marked)? Drives the
    /// `Partial` smart-resume walk.
    pub is_fully_reviewed: &'a dyn Fn(&ChangeId) -> bool,
    /// Whether the stack has any reviewed-state at all. See
    /// [`StackReviewState`] for the contract behind each variant.
    pub stack_review_state: StackReviewState,
}

/// Resolve the resume index for a stack given the cursor file.
///
/// Shell wrapper: loads `cursor.json`, then delegates to the pure
/// [`resume_index_from_cursor`]. On any load error, falls through to the
/// `is_fully_reviewed`-driven fallback so the reviewer still lands on
/// something useful.
pub(crate) fn resume_index(
    data_home: &Path,
    repo_root: &Path,
    hash: RevsetHash,
    inputs: &ResumeInputs<'_>,
) -> usize {
    let cursor = load(data_home, repo_root).unwrap_or_default();
    resume_index_from_cursor(&cursor, hash, inputs)
}

/// Pure resume-rule implementation.
///
/// Returns the 0-based index into `inputs.stack_change_ids` to open first:
///
/// 1. If a cursor exists for `hash` and `last_change_id` is in the stack
///    (this branch always wins; it short-circuits before the
///    `stack_review_state` dispatch):
///    - If `has_comments(last_change_id)` is false, return that position
///      (the user landed on a change but never commented — resume there).
///    - Otherwise scan forward for the first unreviewed change after it.
///    - If every change at or after the cursor has comments, return the
///      cursor position (treat the stack as fully reviewed; reopen there).
/// 2. Otherwise (no cursor entry, or its `last_change_id` left the stack)
///    dispatch on [`StackReviewState`]:
///    - `Fresh`: land at OLDEST (index 0). A first-time reviewer reads
///      from the bottom up.
///    - `Partial`: walk LATEST → OLDEST and return the index of the
///      most-recent change that is NOT fully reviewed. If every change is
///      fully reviewed, return the LATEST index. A reviewer returning to a
///      half-reviewed stack lands at the front of the new work.
pub(crate) fn resume_index_from_cursor(
    cursor: &Cursor,
    hash: RevsetHash,
    inputs: &ResumeInputs<'_>,
) -> usize {
    if inputs.stack_change_ids.is_empty() {
        return 0;
    }

    if let Some(entry) = cursor.revsets.get(&hash.hex()) {
        if let Some(last_pos) = inputs
            .stack_change_ids
            .iter()
            .position(|id| id == &entry.last_change_id)
        {
            // The cursor change itself qualifies if the reviewer never
            // commented on it.
            if !(inputs.has_comments)(&inputs.stack_change_ids[last_pos]) {
                return last_pos;
            }

            // Scan forward for the first unreviewed change.
            let next = inputs.stack_change_ids[last_pos + 1..]
                .iter()
                .position(|id| !(inputs.has_comments)(id))
                .map(|offset| last_pos + 1 + offset);

            return next.unwrap_or(last_pos);
        }
    }

    match inputs.stack_review_state {
        StackReviewState::Fresh => 0,
        StackReviewState::Partial => {
            smart_resume_index(inputs.stack_change_ids, inputs.is_fully_reviewed)
        }
    }
}

/// Walk LATEST → OLDEST and pick the most-recent change that is NOT fully
/// reviewed. Falls back to LATEST when every change reads as fully reviewed,
/// never to OLDEST. Returns 0 only when the stack itself is empty — a
/// degenerate case the caller should already have ruled out before opening
/// the TUI. The "fresh stack" case (no reviewed-state at all) is intercepted
/// in [`resume_index_from_cursor`] before this function runs.
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

    /// Build a `ResumeInputs` for tests. Closures borrow from caller frames,
    /// so the helper takes references with matching lifetimes.
    fn inputs<'a>(
        ids: &'a [ChangeId],
        has_comments: &'a dyn Fn(&ChangeId) -> bool,
        is_fully_reviewed: &'a dyn Fn(&ChangeId) -> bool,
        stack_review_state: StackReviewState,
    ) -> ResumeInputs<'a> {
        ResumeInputs {
            stack_change_ids: ids,
            has_comments,
            is_fully_reviewed,
            stack_review_state,
        }
    }

    #[test]
    fn load_missing_file_returns_empty_cursor() {
        let dir = tmp();
        let cursor = load(dir.path(), dir.path()).unwrap();
        assert!(cursor.revsets.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_err() {
        let dir = tmp();
        let cursor_file = crate::store::repo_data_dir(dir.path(), dir.path()).join("cursor.json");
        std::fs::create_dir_all(cursor_file.parent().unwrap()).unwrap();
        std::fs::write(&cursor_file, b"{not json").unwrap();
        assert!(load(dir.path(), dir.path()).is_err());
    }

    #[test]
    fn resume_index_falls_back_to_smart_rule_on_corrupt_json() {
        // Corrupt cursor.json must not blow up the TUI. With reviewed-state
        // present somewhere in the stack, the smart fallback walks LATEST→
        // OLDEST and lands on the most-recent unreviewed change (LATEST when
        // nothing is reviewed).
        let dir = tmp();
        let cursor_file = crate::store::repo_data_dir(dir.path(), dir.path()).join("cursor.json");
        std::fs::create_dir_all(cursor_file.parent().unwrap()).unwrap();
        std::fs::write(&cursor_file, b"{not json").unwrap();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Partial),
        );
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
        save(dir.path(), dir.path(), &cursor).unwrap();
        let loaded = load(dir.path(), dir.path()).unwrap();
        assert_eq!(loaded.revsets.len(), 1);
        let entry = &loaded.revsets[&hash.hex()];
        assert_eq!(entry.revset, "@");
        assert_eq!(entry.last_change_id, cid("abc12345"));
    }

    #[test]
    fn record_creates_and_updates_entry() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        record(dir.path(), dir.path(), hash, "@", &cid("abc12345")).unwrap();

        let loaded = load(dir.path(), dir.path()).unwrap();
        let entry = &loaded.revsets[&hash.hex()];
        assert_eq!(entry.last_change_id, cid("abc12345"));

        // Update to a different change.
        record(dir.path(), dir.path(), hash, "@", &cid("def99999")).unwrap();
        let loaded2 = load(dir.path(), dir.path()).unwrap();
        let entry2 = &loaded2.revsets[&hash.hex()];
        assert_eq!(entry2.last_change_id, cid("def99999"));
    }

    #[test]
    fn clear_removes_entry() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        record(dir.path(), dir.path(), hash, "@", &cid("abc12345")).unwrap();
        clear(dir.path(), dir.path(), hash).unwrap();
        let loaded = load(dir.path(), dir.path()).unwrap();
        assert!(loaded.revsets.is_empty());
    }

    #[test]
    fn clear_on_missing_file_is_ok() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        clear(dir.path(), dir.path(), hash).unwrap();
    }

    #[test]
    fn resume_index_no_cursor_falls_back_to_latest_when_unreviewed() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        // No cursor file, partial reviewed-state → smart fallback → walk
        // LATEST→OLDEST. `is_fully_reviewed` = false everywhere, so LATEST
        // (index 2) wins.
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Partial),
        );
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
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|_| false,
                &|id| id == &reviewed_id,
                StackReviewState::Partial,
            ),
        );
        assert_eq!(idx, 2, "must walk back from LATEST to first unreviewed");
    }

    #[test]
    fn resume_index_no_cursor_all_reviewed_returns_latest() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| true, StackReviewState::Partial),
        );
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
        save(dir.path(), dir.path(), &cursor).unwrap();
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        // Index 2 is reviewed → smart fallback picks index 1 (latest unreviewed).
        let reviewed_id = ids[2].clone();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|_| false,
                &|id| id == &reviewed_id,
                StackReviewState::Partial,
            ),
        );
        assert_eq!(idx, 1);
    }

    #[test]
    fn resume_index_no_comments_after_last_returns_last_pos() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[1].clone());
        save(dir.path(), dir.path(), &cursor).unwrap();
        // All changes have comments — the cursor change qualifies under the
        // first rule (its own check) only if it has no comments. With every
        // change carrying comments, the resume rule falls through to the
        // last-resort and returns last_pos=1.
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| true, &|_| false, StackReviewState::Partial),
        );
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
        save(dir.path(), dir.path(), &cursor).unwrap();
        // ids[1] has comments, ids[2] does not → next unreviewed is index 2.
        let no_comment_id = ids[2].clone();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|id| id != &no_comment_id,
                &|_| false,
                StackReviewState::Partial,
            ),
        );
        assert_eq!(idx, 2);
    }

    #[test]
    fn resume_index_cursor_at_last_entry_returns_last() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[2].clone());
        save(dir.path(), dir.path(), &cursor).unwrap();
        // has_comments=false everywhere → resume at the cursor change itself
        // (per the corrected resume rule: cursor change qualifies if it has
        // no comments yet).
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Partial),
        );
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
        save(dir.path(), dir.path(), &cursor).unwrap();
        let cursor_id = ids[1].clone();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|id| id != &cursor_id,
                &|_| false,
                StackReviewState::Partial,
            ),
        );
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

    #[test]
    fn resume_index_fresh_stack_no_reviewed_state_lands_at_oldest() {
        // Fresh stack: no stored cursor, no reviewed-state for any change in
        // the stack. The reviewer has never seen this stack — land at OLDEST
        // so the progress reads bottom→top instead of dropping the user at
        // the head of the stack with the visual scrolled to the end.
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Fresh),
        );
        assert_eq!(idx, 0, "fresh stack must land at OLDEST");
    }

    #[test]
    fn resume_index_partial_reviewed_state_walks_latest_to_oldest() {
        // Partial review state (some change in the stack has an entry, even
        // if not fully reviewed) → existing LATEST→OLDEST walk picks up the
        // most-recent unreviewed change. Behavior unchanged from before the
        // fresh-stack rule landed.
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![
            cid("abc11111"),
            cid("abc22222"),
            cid("abc33333"),
            cid("abc44444"),
        ];
        let reviewed_id = ids[3].clone();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|_| false,
                &|id| id == &reviewed_id,
                StackReviewState::Partial,
            ),
        );
        assert_eq!(idx, 2, "partial state walks LATEST→OLDEST");
    }

    #[test]
    fn resume_index_stored_cursor_resumes_regardless_of_reviewed_state() {
        // Stored cursor in the stack short-circuits the reviewed-state check.
        // Even with `StackReviewState::Fresh`, the cursor branch wins and
        // lands at the cursor's index.
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let cursor = make_cursor_with_entry("@", ids[1].clone());
        save(dir.path(), dir.path(), &cursor).unwrap();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Fresh),
        );
        assert_eq!(
            idx, 1,
            "stored cursor must take precedence over fresh-stack rule"
        );
    }

    // ---- T1: empty-stack guard ----

    #[test]
    fn resume_index_from_cursor_empty_stack_fresh_returns_zero() {
        // Defensive: callers should reject empty stacks before the TUI opens,
        // but the resume rule pins index 0 on both review-state arms so a
        // future bypass cannot panic on the empty slice.
        let cursor = Cursor::default();
        let hash = RevsetHash::from_revset("@");
        let idx = resume_index_from_cursor(
            &cursor,
            hash,
            &inputs(&[], &|_| false, &|_| false, StackReviewState::Fresh),
        );
        assert_eq!(idx, 0);
    }

    #[test]
    fn resume_index_from_cursor_empty_stack_partial_returns_zero() {
        let cursor = Cursor::default();
        let hash = RevsetHash::from_revset("@");
        let idx = resume_index_from_cursor(
            &cursor,
            hash,
            &inputs(&[], &|_| false, &|_| false, StackReviewState::Partial),
        );
        assert_eq!(idx, 0);
    }

    // ---- T2: single-change stack pins both paths ----

    #[test]
    fn resume_index_single_change_fresh_state_lands_at_oldest_which_is_latest() {
        // With a one-element stack OLDEST and LATEST are the same index. Pin
        // the convergence so a future refactor cannot silently diverge the
        // two arms (e.g., a Partial fast-path that returns `len()` would slip
        // past lossier checks).
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Fresh),
        );
        assert_eq!(idx, 0, "single-change stack: Fresh must land at index 0");
    }

    #[test]
    fn resume_index_single_change_partial_state_walks_to_oldest_which_is_latest() {
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let ids = vec![cid("abc11111")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Partial),
        );
        assert_eq!(
            idx, 0,
            "single-change stack: Partial walk also lands at index 0 (only candidate)"
        );
    }

    // ---- T3: stored cursor with absent change_id falls through to dispatch ----

    #[test]
    fn resume_index_stored_cursor_absent_change_id_falls_through_fresh() {
        // Cursor file present, but `last_change_id` is no longer in the stack
        // (rebased / abandoned). The cursor branch fails to find a position,
        // so dispatch on `StackReviewState`. Fresh → OLDEST.
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let cursor = make_cursor_with_entry("@", cid("ffffffff"));
        save(dir.path(), dir.path(), &cursor).unwrap();
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(&ids, &|_| false, &|_| false, StackReviewState::Fresh),
        );
        assert_eq!(idx, 0, "absent-change-id cursor + Fresh → OLDEST");
    }

    #[test]
    fn resume_index_stored_cursor_absent_change_id_falls_through_partial() {
        // Same setup as the Fresh case, but with Partial review state the
        // smart-resume walk fires and picks the most-recent unreviewed change.
        let dir = tmp();
        let hash = RevsetHash::from_revset("@");
        let cursor = make_cursor_with_entry("@", cid("ffffffff"));
        save(dir.path(), dir.path(), &cursor).unwrap();
        let ids = vec![cid("abc11111"), cid("abc22222"), cid("abc33333")];
        let reviewed_id = ids[2].clone();
        let idx = resume_index(
            dir.path(),
            dir.path(),
            hash,
            &inputs(
                &ids,
                &|_| false,
                &|id| id == &reviewed_id,
                StackReviewState::Partial,
            ),
        );
        assert_eq!(
            idx, 1,
            "absent-change-id cursor + Partial → LATEST→OLDEST walk"
        );
    }
}
