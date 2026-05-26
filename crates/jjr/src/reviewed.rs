use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::change_id::{ChangeId, CommitId};
use crate::error::{JjrError, Result};
use crate::util::{atomic_write_bytes, log_warning};

/// What the user just landed on for review-tracking purposes. The description
/// view (`file_index` = 0 in the TUI) is a first-class target rather than a
/// reserved sentinel `PathBuf`, so the state model never has to encode "is this
/// path actually the description?" as a string contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewTarget {
    Description,
    File(PathBuf),
}

/// Persistent reviewed-state for one revset run.
///
/// Stored under `data_home` at `repos/<repo>/reviewed.json`. Keyed by
/// `ChangeId` so a fresh `jjr --stack` against the same change picks up
/// what was already marked. The stored `commit_id` is the invalidation
/// token: when the change gets amended or rebased the `commit_id` flips,
/// and on next load the stale reviewed bits for that change are dropped.
///
/// `HashMap` (not `BTreeMap`) because `ChangeId` does not derive `Ord` and
/// the on-disk format does not depend on iteration order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReviewedState {
    pub(crate) entries: HashMap<ChangeId, ReviewedEntry>,
}

/// Per-change reviewed-bits. Description and file paths are tracked
/// independently so the "fully reviewed" predicate can require both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewedEntry {
    pub(crate) commit_id: CommitId,
    pub(crate) description_reviewed: bool,
    pub(crate) reviewed_files: BTreeSet<PathBuf>,
}

impl ReviewedEntry {
    fn new(commit_id: CommitId) -> Self {
        Self {
            commit_id,
            description_reviewed: false,
            reviewed_files: BTreeSet::new(),
        }
    }

    /// A change is fully reviewed when the description bit is set AND every
    /// file path in the change's diff is in `reviewed_files`. Empty diff is
    /// covered by description-only.
    pub(crate) fn is_fully_reviewed(&self, diff_file_paths: &[PathBuf]) -> bool {
        if !self.description_reviewed {
            return false;
        }
        diff_file_paths
            .iter()
            .all(|p| self.reviewed_files.contains(p))
    }
}

fn reviewed_path(data_home: &Path, repo_root: &Path) -> PathBuf {
    crate::store::repo_data_dir(data_home, repo_root).join("reviewed.json")
}

impl ReviewedState {
    /// Load `reviewed.json` from the XDG data home, returning an empty state
    /// if the file does not exist. Custom Deserialize at the trust boundary
    /// rejects records missing required fields with a named error.
    pub(crate) fn load(data_home: &Path, repo_root: &Path) -> Result<Self> {
        let path = reviewed_path(data_home, repo_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path).map_err(|source| JjrError::Io { source })?;
        let dto: ReviewedStateDto = serde_json::from_str(&raw).map_err(|e| JjrError::Io {
            source: std::io::Error::other(e),
        })?;
        Ok(dto.into_state())
    }

    /// Atomic save via `util::atomic_write_bytes` — crash-safe rename, same
    /// pattern as cursor.json and the comment store.
    pub(crate) fn save(&self, data_home: &Path, repo_root: &Path) -> Result<()> {
        let path = reviewed_path(data_home, repo_root);
        let dto = ReviewedStateDto::from_state(self);
        let json = serde_json::to_string_pretty(&dto).map_err(|e| JjrError::Io {
            source: std::io::Error::other(e),
        })?;
        atomic_write_bytes(&path, json.as_bytes())
    }

    /// Mark `target` reviewed for `(change_id, commit_id)`.
    ///
    /// Auto-invalidates: if a stored entry for `change_id` carries a different
    /// `commit_id` (the change was amended or rebased) the old entry is
    /// dropped and a fresh one is created. The reviewed bit set here is the
    /// only bit on the new entry — by construction, the user just landed on
    /// `target` for the new commit, so anything else has not been re-reviewed.
    ///
    /// Returns a [`MarkOutcome`] describing whether invalidation just fired
    /// so the caller can surface a one-shot status toast.
    pub(crate) fn mark(
        &mut self,
        change_id: ChangeId,
        commit_id: CommitId,
        target: ReviewTarget,
    ) -> MarkOutcome {
        // Auto-invalidation: if the stored entry's commit_id no longer
        // matches, replace it with a fresh entry for the new commit. By
        // construction the only bit set on the new entry is the `target`
        // we are about to mark — anything else has not been re-reviewed.
        let needs_reset = self
            .entries
            .get(&change_id)
            .is_some_and(|existing| existing.commit_id != commit_id);
        if needs_reset {
            self.entries.remove(&change_id);
        }
        let entry = self
            .entries
            .entry(change_id)
            .or_insert_with(|| ReviewedEntry::new(commit_id));

        match target {
            ReviewTarget::Description => entry.description_reviewed = true,
            ReviewTarget::File(path) => {
                entry.reviewed_files.insert(path);
            }
        }
        if needs_reset {
            MarkOutcome::ResetDueToCommitMismatch
        } else {
            MarkOutcome::NoReset
        }
    }

    /// Clear the reviewed bit for `target` on the entry for
    /// `(change_id, commit_id)`. No-op when the entry is missing or its
    /// stored `commit_id` does not match (the bit being asked about
    /// belongs to an old, invalidated entry).
    ///
    /// Used by the manual `U` keybind so the reviewer can correct an
    /// auto-mark that fired prematurely.
    pub(crate) fn unmark(
        &mut self,
        change_id: &ChangeId,
        commit_id: &CommitId,
        target: &ReviewTarget,
    ) {
        let Some(entry) = self.entries.get_mut(change_id) else {
            return;
        };
        if &entry.commit_id != commit_id {
            return;
        }
        match target {
            ReviewTarget::Description => entry.description_reviewed = false,
            ReviewTarget::File(path) => {
                entry.reviewed_files.remove(path);
            }
        }
    }
}

/// Outcome of [`ReviewedState::mark`]. Tells the caller whether the call
/// dropped a stale entry for the same `change_id` (`commit_id` mismatch) so
/// the TUI can surface a status toast distinguishing "first encounter"
/// (silent) from "amended; reviewed state reset" (toast).
///
/// Today there are exactly two reachable outcomes; if a third reset reason
/// arrives, add a variant and pattern-match exhaustiveness will route every
/// caller through the new case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkOutcome {
    /// Either no prior entry existed for this `change_id`, or the prior
    /// entry's `commit_id` matched — nothing was invalidated.
    NoReset,
    /// A prior entry for this `change_id` carried a different `commit_id`
    /// (the change was amended/rebased) and was dropped before the new
    /// mark was applied.
    ResetDueToCommitMismatch,
}

impl ReviewedState {
    /// True iff `change_id` has any persisted reviewed-state at all,
    /// regardless of whether the stored `commit_id` still matches the live
    /// commit or whether the entry's bits are complete.
    ///
    /// Used by the cursor smart-resume "fresh stack" check: if no change in
    /// the current stack has any entry, the reviewer has never touched this
    /// stack and should land at the OLDEST change. If at least one entry
    /// exists somewhere in the stack, the reviewer is mid-review and we use
    /// the existing LATEST→OLDEST walk.
    pub(crate) fn has_entry(&self, change_id: &ChangeId) -> bool {
        self.entries.contains_key(change_id)
    }

    /// True iff the user has actually marked this `(change_id, commit_id)`
    /// fully reviewed: an entry exists, its `commit_id` matches the live
    /// commit, AND every file in the live diff is in `reviewed_files`.
    ///
    /// Both call sites consume this single predicate today:
    /// - Visual rendering (overview-screen ✓ glyph): `is_marked_fully_reviewed`
    /// - Cursor smart-resume (skip vs. land): `!is_marked_fully_reviewed`
    ///
    /// If the two ever need to diverge — for example, a re-review prompt
    /// that distinguishes "stale" from "missing" — add a parallel method at
    /// that point and split the call sites. Until then, one predicate is
    /// enough.
    pub(crate) fn is_marked_fully_reviewed(
        &self,
        change_id: &ChangeId,
        commit_id: &CommitId,
        diff_file_paths: &[PathBuf],
    ) -> bool {
        let Some(entry) = self.entries.get(change_id) else {
            return false;
        };
        if &entry.commit_id != commit_id {
            return false;
        }
        entry.is_fully_reviewed(diff_file_paths)
    }
}

/// Wire-format DTO. Custom (derived) Deserialize on the inner records goes
/// through `ReviewedEntryDto::into_entry`, which surfaces named errors when
/// fields are missing.
#[derive(Debug, Serialize, Deserialize)]
struct ReviewedStateDto {
    /// Map keyed by `ChangeId.as_str()`. Using the string key here keeps the
    /// JSON shape ergonomic to read by hand without sacrificing the typed
    /// `ChangeId` boundary check on load.
    #[serde(default)]
    entries: BTreeMap<String, ReviewedEntryDto>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReviewedEntryDto {
    commit_id: String,
    #[serde(default)]
    description_reviewed: bool,
    #[serde(default)]
    reviewed_files: Vec<String>,
}

impl ReviewedStateDto {
    fn from_state(state: &ReviewedState) -> Self {
        let entries = state
            .entries
            .iter()
            .map(|(change_id, entry)| {
                (
                    change_id.as_str().to_owned(),
                    ReviewedEntryDto {
                        commit_id: entry.commit_id.as_str().to_owned(),
                        description_reviewed: entry.description_reviewed,
                        reviewed_files: entry
                            .reviewed_files
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    },
                )
            })
            .collect();
        Self { entries }
    }

    /// Convert wire DTO into a typed `ReviewedState`, dropping entries whose
    /// fields fail trust-boundary parsing (invalid `ChangeId` or `CommitId`).
    /// Named missing-field errors on the DTO itself are surfaced by serde
    /// ahead of this conversion; this function only handles the typed-id
    /// validation step.
    ///
    /// Dropped entries emit a `log_warning` to stderr so silent corruption
    /// is observable in logs — matches `store.rs`'s precedent for malformed
    /// records in JSONL files.
    fn into_state(self) -> ReviewedState {
        Self::into_state_with_logger(self, log_warning)
    }

    /// Test seam: same conversion as [`Self::into_state`] but routes warnings
    /// through `logger` so a unit test can assert the canonical key + reason
    /// without capturing real stderr. Production callers go through the
    /// public `into_state` (which forwards to `util::log_warning`).
    fn into_state_with_logger(self, mut logger: impl FnMut(&str)) -> ReviewedState {
        let entries = self
            .entries
            .into_iter()
            .filter_map(|(raw_change, dto)| {
                let Ok(change_id) = ChangeId::parse(&raw_change) else {
                    logger(&format!(
                        "reviewed.json: dropping entry with invalid change_id {raw_change:?}"
                    ));
                    return None;
                };
                let Ok(commit_id) = CommitId::parse(&dto.commit_id) else {
                    logger(&format!(
                        "reviewed.json: dropping entry {raw_change:?} \
                         with invalid commit_id {:?}",
                        dto.commit_id
                    ));
                    return None;
                };
                let reviewed_files = dto
                    .reviewed_files
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<BTreeSet<PathBuf>>();
                Some((
                    change_id,
                    ReviewedEntry {
                        commit_id,
                        description_reviewed: dto.description_reviewed,
                        reviewed_files,
                    },
                ))
            })
            .collect();
        ReviewedState { entries }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn coid(s: &str) -> CommitId {
        CommitId::parse(s).unwrap()
    }

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_missing_file_returns_empty_state() {
        let dir = tmp();
        let state = ReviewedState::load(dir.path(), dir.path()).unwrap();
        assert!(state.entries.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp();
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("src/foo.rs")),
        );
        state.save(dir.path(), dir.path()).unwrap();

        let loaded = ReviewedState::load(dir.path(), dir.path()).unwrap();
        let entry = loaded.entries.get(&cid("abc12345")).unwrap();
        assert_eq!(entry.commit_id, coid("deadbeef"));
        assert!(entry.description_reviewed);
        assert!(entry.reviewed_files.contains(&PathBuf::from("src/foo.rs")));
    }

    #[test]
    fn deserialize_missing_commit_id_fails_with_named_error() {
        // `commit_id` is required on `ReviewedEntryDto`. Serde's auto-derived
        // missing-field error names the offending field — pin that here so a
        // future refactor that hand-rolls Deserialize must preserve it.
        let raw = r#"{"entries": {"abc12345": {"description_reviewed": true}}}"#;
        let err = serde_json::from_str::<ReviewedStateDto>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("commit_id"), "got: {err}");
    }

    #[test]
    fn load_skips_entry_with_invalid_change_id() {
        // Hand-edited JSON must not be able to smuggle malformed change IDs
        // past the trust boundary. `ChangeId::parse` rejects "bad", so the
        // entry is silently dropped on load.
        let dir = tmp();
        let reviewed_file =
            crate::store::repo_data_dir(dir.path(), dir.path()).join("reviewed.json");
        std::fs::create_dir_all(reviewed_file.parent().unwrap()).unwrap();
        std::fs::write(
            &reviewed_file,
            r#"{"entries": {"bad": {"commit_id": "deadbeef"}}}"#,
        )
        .unwrap();
        let state = ReviewedState::load(dir.path(), dir.path()).unwrap();
        assert!(state.entries.is_empty());
    }

    #[test]
    fn load_skips_entry_with_invalid_commit_id() {
        let dir = tmp();
        let reviewed_file =
            crate::store::repo_data_dir(dir.path(), dir.path()).join("reviewed.json");
        std::fs::create_dir_all(reviewed_file.parent().unwrap()).unwrap();
        std::fs::write(
            &reviewed_file,
            r#"{"entries": {"abc12345": {"commit_id": "not-hex!"}}}"#,
        )
        .unwrap();
        let state = ReviewedState::load(dir.path(), dir.path()).unwrap();
        assert!(state.entries.is_empty());
    }

    #[test]
    fn is_fully_reviewed_empty_diff_only_needs_description() {
        let mut entry = ReviewedEntry::new(coid("deadbeef"));
        assert!(!entry.is_fully_reviewed(&[]));
        entry.description_reviewed = true;
        assert!(entry.is_fully_reviewed(&[]));
    }

    #[test]
    fn is_fully_reviewed_partial_files_returns_false() {
        let mut entry = ReviewedEntry::new(coid("deadbeef"));
        entry.description_reviewed = true;
        entry.reviewed_files.insert(PathBuf::from("a.rs"));
        let diff = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        assert!(!entry.is_fully_reviewed(&diff));
    }

    #[test]
    fn is_fully_reviewed_all_files_and_description_returns_true() {
        let mut entry = ReviewedEntry::new(coid("deadbeef"));
        entry.description_reviewed = true;
        entry.reviewed_files.insert(PathBuf::from("a.rs"));
        entry.reviewed_files.insert(PathBuf::from("b.rs"));
        let diff = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        assert!(entry.is_fully_reviewed(&diff));
    }

    #[test]
    fn is_fully_reviewed_files_without_description_returns_false() {
        let mut entry = ReviewedEntry::new(coid("deadbeef"));
        entry.reviewed_files.insert(PathBuf::from("a.rs"));
        let diff = vec![PathBuf::from("a.rs")];
        assert!(!entry.is_fully_reviewed(&diff));
    }

    #[test]
    fn mark_creates_entry_when_missing() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert!(entry.description_reviewed);
        assert!(entry.reviewed_files.is_empty());
    }

    #[test]
    fn mark_is_idempotent() {
        let mut state = ReviewedState::default();
        let path = PathBuf::from("src/foo.rs");
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(path.clone()),
        );
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(path.clone()),
        );
        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert_eq!(entry.reviewed_files.len(), 1);
        assert!(entry.reviewed_files.contains(&path));
    }

    #[test]
    fn mark_drops_stale_entry_on_commit_id_mismatch() {
        // A change can be amended/rebased between sessions. When the commit_id
        // flips, the prior reviewed bits are dropped — the reviewer must see
        // the new commit's contents from scratch.
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("a.rs")),
        );

        // Same change_id, different commit_id — invalidate.
        state.mark(
            cid("abc12345"),
            coid("11223344"),
            ReviewTarget::File(PathBuf::from("b.rs")),
        );

        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert_eq!(entry.commit_id, coid("11223344"));
        assert!(
            !entry.description_reviewed,
            "description bit must reset after commit_id flip"
        );
        assert_eq!(
            entry.reviewed_files.len(),
            1,
            "only the new mark survives invalidation"
        );
        assert!(entry.reviewed_files.contains(&PathBuf::from("b.rs")));
    }

    #[test]
    fn mark_first_touch_returns_no_reset_outcome() {
        let mut state = ReviewedState::default();
        let outcome = state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        assert_eq!(
            outcome,
            MarkOutcome::NoReset,
            "first-touch must not report a reset"
        );
    }

    #[test]
    fn mark_repeat_same_commit_returns_no_reset_outcome() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        let outcome = state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("a.rs")),
        );
        assert_eq!(
            outcome,
            MarkOutcome::NoReset,
            "matching commit_id must not report a reset"
        );
    }

    #[test]
    fn mark_commit_id_mismatch_returns_reset_outcome() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        let outcome = state.mark(cid("abc12345"), coid("11223344"), ReviewTarget::Description);
        assert_eq!(
            outcome,
            MarkOutcome::ResetDueToCommitMismatch,
            "amended commit_id must report a reset"
        );
    }

    #[test]
    fn unmark_clears_description_bit() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.unmark(
            &cid("abc12345"),
            &coid("deadbeef"),
            &ReviewTarget::Description,
        );
        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert!(!entry.description_reviewed);
    }

    #[test]
    fn unmark_removes_file_from_reviewed_files() {
        let mut state = ReviewedState::default();
        let path = PathBuf::from("src/foo.rs");
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(path.clone()),
        );
        state.unmark(
            &cid("abc12345"),
            &coid("deadbeef"),
            &ReviewTarget::File(path.clone()),
        );
        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert!(!entry.reviewed_files.contains(&path));
    }

    #[test]
    fn unmark_is_noop_for_unknown_change_id() {
        let mut state = ReviewedState::default();
        // Nothing exists; unmark must not panic and must not create an entry.
        state.unmark(
            &cid("abc12345"),
            &coid("deadbeef"),
            &ReviewTarget::Description,
        );
        assert!(state.entries.is_empty());
    }

    #[test]
    fn unmark_is_noop_when_commit_id_mismatches() {
        // Stored entry under one commit_id; caller asks to unmark with a
        // different commit_id. Treat as no-op — the bit they think they're
        // unmarking belongs to an old, invalidated entry.
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.unmark(
            &cid("abc12345"),
            &coid("11223344"),
            &ReviewTarget::Description,
        );
        let entry = state.entries.get(&cid("abc12345")).unwrap();
        assert!(
            entry.description_reviewed,
            "unmark with mismatched commit_id must not touch stored bits"
        );
    }

    #[test]
    fn is_marked_fully_reviewed_returns_false_for_unknown_change() {
        let state = ReviewedState::default();
        assert!(!state.is_marked_fully_reviewed(&cid("abc12345"), &coid("deadbeef"), &[]));
    }

    #[test]
    fn is_marked_fully_reviewed_returns_false_when_commit_id_mismatches() {
        // The state remembers reviewed bits for an OLD commit_id. The query
        // uses the NEW commit_id (caller already saw `jj show` for the
        // amended change). Treat as not-reviewed so the smart-resume picks
        // the change up.
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        assert!(!state.is_marked_fully_reviewed(&cid("abc12345"), &coid("11223344"), &[]));
    }

    #[test]
    fn has_entry_returns_true_when_change_id_present() {
        // `has_entry` is the predicate behind the cursor smart-resume
        // fresh-stack check: it must report true as soon as any mark has
        // been written for the change_id, regardless of commit_id match or
        // entry completeness.
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        assert!(state.has_entry(&cid("abc12345")));
    }

    #[test]
    fn has_entry_returns_false_for_unknown_change_id() {
        let state = ReviewedState::default();
        assert!(!state.has_entry(&cid("abc12345")));
    }

    #[test]
    fn is_marked_fully_reviewed_true_when_match_and_full() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("a.rs")),
        );
        let diff = vec![PathBuf::from("a.rs")];
        assert!(state.is_marked_fully_reviewed(&cid("abc12345"), &coid("deadbeef"), &diff));
    }

    /// C1: pin the warning surface. Dropped entries (invalid `change_id` /
    /// `commit_id`) must fire `log_warning` with the canonical key + reason
    /// so silent corruption is observable in stderr.
    #[test]
    fn into_state_logs_warning_for_invalid_change_id() {
        let raw = r#"{"entries": {"bad": {"commit_id": "deadbeef"}}}"#;
        let dto: ReviewedStateDto = serde_json::from_str(raw).unwrap();
        let mut warnings: Vec<String> = Vec::new();
        let state = ReviewedStateDto::into_state_with_logger(dto, |m| warnings.push(m.to_owned()));
        assert!(state.entries.is_empty(), "invalid entry must be dropped");
        assert_eq!(warnings.len(), 1, "exactly one warning expected");
        assert!(
            warnings[0].contains("invalid change_id") && warnings[0].contains("\"bad\""),
            "warning must name the offending key and reason: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn into_state_logs_warning_for_invalid_commit_id() {
        let raw = r#"{"entries": {"abc12345": {"commit_id": "not-hex!"}}}"#;
        let dto: ReviewedStateDto = serde_json::from_str(raw).unwrap();
        let mut warnings: Vec<String> = Vec::new();
        let state = ReviewedStateDto::into_state_with_logger(dto, |m| warnings.push(m.to_owned()));
        assert!(state.entries.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("invalid commit_id")
                && warnings[0].contains("\"abc12345\"")
                && warnings[0].contains("\"not-hex!\""),
            "warning must name change_id and reason: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn into_state_keeps_valid_entries_alongside_dropped_ones() {
        // One valid entry + one invalid entry → valid survives, invalid is
        // dropped with a warning. Confirms drop is per-entry, not all-or-
        // nothing.
        let raw = r#"{
            "entries": {
                "abc12345": {"commit_id": "deadbeef"},
                "bad": {"commit_id": "deadbeef"}
            }
        }"#;
        let dto: ReviewedStateDto = serde_json::from_str(raw).unwrap();
        let mut warnings: Vec<String> = Vec::new();
        let state = ReviewedStateDto::into_state_with_logger(dto, |m| warnings.push(m.to_owned()));
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.contains_key(&cid("abc12345")));
        assert_eq!(warnings.len(), 1);
    }

    // ---- T4: round-trip field combinations ----

    #[test]
    fn description_only_entry_roundtrips() {
        let dir = tmp();
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.save(dir.path(), dir.path()).unwrap();
        let loaded = ReviewedState::load(dir.path(), dir.path()).unwrap();
        let entry = loaded.entries.get(&cid("abc12345")).unwrap();
        assert!(entry.description_reviewed);
        assert!(entry.reviewed_files.is_empty());
    }

    #[test]
    fn files_only_entry_roundtrips() {
        let dir = tmp();
        let mut state = ReviewedState::default();
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("a.rs")),
        );
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("b.rs")),
        );
        state.save(dir.path(), dir.path()).unwrap();
        let loaded = ReviewedState::load(dir.path(), dir.path()).unwrap();
        let entry = loaded.entries.get(&cid("abc12345")).unwrap();
        assert!(!entry.description_reviewed);
        assert_eq!(entry.reviewed_files.len(), 2);
        assert!(entry.reviewed_files.contains(&PathBuf::from("a.rs")));
        assert!(entry.reviewed_files.contains(&PathBuf::from("b.rs")));
    }

    #[test]
    fn empty_entry_with_no_marks_is_not_persisted() {
        // No marks means no entry — saving an empty state writes the JSON
        // shell but no per-change records. Reloading produces an empty map.
        let dir = tmp();
        let state = ReviewedState::default();
        state.save(dir.path(), dir.path()).unwrap();
        let loaded = ReviewedState::load(dir.path(), dir.path()).unwrap();
        assert!(loaded.entries.is_empty());
    }

    /// T5: overview-cache empty-diff-paths fallback.
    ///
    /// When `jj show` fails for an entry, the overview cache substitutes
    /// `Vec::new()` for `diff_paths_per_change[i]`. The single
    /// `is_marked_fully_reviewed` predicate drives both the visual ✓ glyph
    /// (true → show) and cursor smart-resume (true → skip), so verifying it
    /// behaves correctly on empty diff paths covers both call sites.
    #[test]
    fn empty_diff_paths_with_description_marked_reports_fully_reviewed() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        assert!(
            state.is_marked_fully_reviewed(&cid("abc12345"), &coid("deadbeef"), &[]),
            "description-only entry with empty diff must read as fully reviewed"
        );
    }

    /// Symmetric case: empty-diff-paths WITHOUT description marked reports
    /// not-fully-reviewed (visual doesn't show ✓; resume DOES land here).
    #[test]
    fn empty_diff_paths_without_description_reports_not_fully_reviewed() {
        let state = ReviewedState::default();
        assert!(!state.is_marked_fully_reviewed(&cid("abc12345"), &coid("deadbeef"), &[]));
    }

    /// T2: partial-review pins the "covers all paths" requirement of
    /// `is_marked_fully_reviewed`. Entry exists, `commit_id` matches, but
    /// only some of the diff paths are in `reviewed_files` → must report
    /// false.
    #[test]
    fn is_marked_fully_reviewed_returns_false_when_files_partially_marked() {
        let mut state = ReviewedState::default();
        state.mark(cid("abc12345"), coid("deadbeef"), ReviewTarget::Description);
        state.mark(
            cid("abc12345"),
            coid("deadbeef"),
            ReviewTarget::File(PathBuf::from("a.rs")),
        );
        // Diff has TWO files; only `a.rs` is marked. The reviewer hasn't
        // seen `b.rs` yet, so the change is not fully reviewed.
        let diff = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        assert!(!state.is_marked_fully_reviewed(&cid("abc12345"), &coid("deadbeef"), &diff));
    }

    #[test]
    fn multiple_changes_with_mixed_shapes_roundtrip() {
        // Three changes: description-only, files-only, both. All survive
        // round-trip with the right subset of bits each.
        let dir = tmp();
        let mut state = ReviewedState::default();
        state.mark(cid("abc11111"), coid("deadbeef"), ReviewTarget::Description);
        state.mark(
            cid("abc22222"),
            coid("11223344"),
            ReviewTarget::File(PathBuf::from("only.rs")),
        );
        state.mark(cid("abc33333"), coid("55667788"), ReviewTarget::Description);
        state.mark(
            cid("abc33333"),
            coid("55667788"),
            ReviewTarget::File(PathBuf::from("both.rs")),
        );
        state.save(dir.path(), dir.path()).unwrap();
        let loaded = ReviewedState::load(dir.path(), dir.path()).unwrap();

        let e1 = loaded.entries.get(&cid("abc11111")).unwrap();
        assert!(e1.description_reviewed);
        assert!(e1.reviewed_files.is_empty());

        let e2 = loaded.entries.get(&cid("abc22222")).unwrap();
        assert!(!e2.description_reviewed);
        assert_eq!(e2.reviewed_files.len(), 1);

        let e3 = loaded.entries.get(&cid("abc33333")).unwrap();
        assert!(e3.description_reviewed);
        assert!(e3.reviewed_files.contains(&PathBuf::from("both.rs")));
    }
}
