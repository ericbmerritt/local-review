//! Core entity types produced by semantic extraction.
//!
//! `EntityCoreData` is the cache-safe output of extraction. It carries the
//! semantic identity plus the diff classification, but not the raw source text
//! (which is large). Display names and comment counts are computed at render
//! time from this type and the live comment store.

use std::path::PathBuf;

pub use super::entity_id::EntityId;
use crate::diff::DiffFile;

/// Classification of a code entity by its syntactic kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntityKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Module,
    Type,
    Constant,
    Table,
    View,
    Index,
    Trigger,
    Policy,
    Schema,
    Extension,
    ConfigProperty,
    AnonymousBlock,
    /// Markdown document section (bounded by an ATX heading).
    Section,
    /// Top-level test suite block (`describe` / `suite`).
    TestSuite,
    /// Individual test case (`it` / `test` / `specify`).
    TestCase,
    Other,
}

/// How the entity changed between before and after states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Moved,
}

/// What specifically changed within a modified entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangeAnnotation {
    /// Declaration (signature, generics, visibility, base class) changed.
    SigChanged,
    /// Only the body changed; declaration is identical.
    BodyOnly,
    /// Both declaration and body changed.
    SigAndBody,
    /// No annotation available (added / deleted / moved entities).
    None,
}

/// Behavior-preserving refactor classification, detected by the differ.
///
/// A parser heuristic, not truth (spec: "the classification is a hint, not a
/// verdict"): tags demote rows visually but never hide them by default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RefactorKind {
    /// Same file, matched by content, scope-chain tail differs. `from` is the
    /// old name (before-state scope-chain tail). `pure` is `true` only when
    /// substituting the new name for the old in the entire before-state
    /// content reproduces the after-state exactly — any other edit (params,
    /// return type, visibility, body) fails the equality and stays
    /// undemoted. Substitution artifacts fail conservatively (not pure).
    Renamed { from: String, pure: bool },
    /// Cross-file move (existing Jaccard match). `identical` is `true` when
    /// the content hash survived the move unchanged — the behavior-preserving
    /// case. Origin file lives in `EntityCoreData::source_file`.
    Moved { identical: bool },
    /// Added entity whose tokens are largely contained in the removed span of
    /// a shrunken sibling from the same before-file. `from` is that sibling.
    Extracted { from: EntityId },
}

/// Extraction output and diff classification for one entity.
///
/// Cached to disk; `display_name` and `comment_count` are computed at render
/// time and not stored here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityCoreData {
    pub id: EntityId,
    pub kind: EntityKind,
    pub change: ChangeType,
    pub annotation: ChangeAnnotation,
    /// Refactor classification; `None` for ordinary changes.
    pub refactor: Option<RefactorKind>,
    /// `(start_line, end_line)` in the after-state file (1-indexed).
    pub line_range: (u32, u32),
    /// Populated only for `ChangeType::Moved`; the file the entity moved from.
    pub source_file: Option<PathBuf>,
    /// The line the entity anchors to for inline-comment placement.
    pub target_line: Option<u32>,
    /// `false` when the only change was formatting or comments (parser heuristic).
    pub structural_change: bool,
    /// First 8 bytes of the blake3 hash of the entity's after-state source text.
    pub content_hash: u64,
}

impl EntityCoreData {
    /// `true` when this change is a behavior-preserving refactor: the row is
    /// visually demoted and hidden by the `;` filter.
    ///
    /// - `Renamed` preserves behavior only when `pure` — whole-content name
    ///   substitution reproduced the after-state exactly. A rename that also
    ///   touches params, return type, visibility, or body is not pure and
    ///   stays undemoted (rendered as `renamed +body`).
    /// - `Moved` preserves behavior only when content survived identically.
    /// - `Extracted` passed the containment threshold by construction.
    pub fn is_behavior_preserving(&self) -> bool {
        behavior_preserving(self.refactor.as_ref())
    }
}

/// Internal representation produced by extractors before diff computation.
///
/// Not cached; only lives during the extract → diff pipeline. Carries the raw
/// source text needed for Jaccard similarity matching.
#[derive(Debug, Clone)]
pub struct RawEntity {
    /// Structured identity.
    pub id: EntityId,
    /// Scope portion as a `::` -separated string (e.g., `Session::refresh`).
    /// Used by the Container Rule to detect parent-child relationships without
    /// re-parsing `id.scope_chain`.
    pub scope: String,
    pub kind: EntityKind,
    pub start_line: u32,
    pub end_line: u32,
    /// Full source text of this entity (needed for Jaccard matching).
    pub content: String,
    /// Hash of `content` (first 8 bytes of blake3).
    pub content_hash: u64,
    /// Hash of the first non-comment, non-decorator declaration line.
    pub sig_hash: u64,
    /// Hash of the body (everything after the first declaration line).
    pub body_hash: u64,
    /// File path as a `String` (duplicate of `id.file_path`) for perf-sensitive
    /// comparisons in the differ.
    pub file_path: String,
}

// ── Render-time types ─────────────────────────────────────────────────────────

/// `(start_line, end_line)` inclusive, 1-indexed.
pub type LineRange = (u32, u32);

/// Content for the pinned description row at the top of the entity list.
#[derive(Debug, Clone)]
pub struct DescriptionSummary {
    /// Commit subject (ggr) or change description first line (jjr).
    pub subject: String,
    /// Number of change-scoped comments on this entry.
    pub comment_count: usize,
    /// First non-empty body line after the subject, for the orientation
    /// header's peek row. `None` when the description has no body.
    pub body_peek: Option<String>,
}

/// Extract the orientation-header peek line from a full description
/// (subject on line 1): the first non-empty line *after* the subject.
/// Callers strip controls.
pub fn body_peek_from(description: &str) -> Option<String> {
    peek_line(description.lines().skip(1))
}

/// Extract the orientation-header peek line from body-only text (no
/// subject line, e.g. GitHub's `messageBody`). Callers strip controls.
pub fn body_peek_from_body(body: &str) -> Option<String> {
    peek_line(body.lines())
}

fn peek_line<'a>(lines: impl Iterator<Item = &'a str> + 'a) -> Option<String> {
    lines
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// Build a synthetic "whole-file" `EntitySummary` for a file with no
/// extracted entities — either because extraction failed (parse errors,
/// IO failure, unsupported language) or because the file is a valid
/// source with no entity-kind matches (e.g., a plain text file in a diff
/// of markdown + plaintext). The user still needs to see and comment on
/// the change, so the entity list surfaces one row per file representing
/// the whole file. Navigating to it opens the file diff.
pub fn fallback_summary_for_file(file: &DiffFile) -> EntitySummary {
    let (path, source_file, change) = match file {
        DiffFile::Added { path, .. } => (path.clone(), None, ChangeType::Added),
        DiffFile::Removed { path, .. } => (path.clone(), None, ChangeType::Deleted),
        DiffFile::Renamed { from, to, .. } => (to.clone(), Some(from.clone()), ChangeType::Moved),
        DiffFile::Modified { path, .. } | DiffFile::Binary { path } => {
            (path.clone(), None, ChangeType::Modified)
        }
    };
    let display_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let id = EntityId::new(path.clone(), vec![display_name.clone()], None, 0);
    EntitySummary {
        id,
        display_name,
        kind: EntityKind::Other,
        change,
        annotation: ChangeAnnotation::None,
        file_path: path,
        source_file,
        target_line: None,
        // Whole-file range: a fallback row represents the entire file (no
        // entity-level granularity), so when the user navigates into it
        // they should see the full diff. Using `(0, 0)` here would cause
        // entity-clip mode to render an empty view because
        // `clip_diff_view_to_range` filters lines whose source/target line
        // numbers fall inside the range, and real diff lines start at 1.
        // `(1, u32::MAX)` captures every line on either side of the diff.
        line_range: (1, u32::MAX),
        structural_change: true,
        content_hash: 0,
        refactor: None,
        comment_count: 0,
        reviewed: false,
        risk: None,
        fallback: true,
    }
}

/// Render-time view of one entity; not stored in the cache.
///
/// Constructed from `EntityCoreData` + comment store lookups at render time.
#[derive(Debug, Clone)]
pub struct EntitySummary {
    pub id: EntityId,
    /// Language-native scope path computed at render time.
    pub display_name: String,
    pub kind: EntityKind,
    pub change: ChangeType,
    pub annotation: ChangeAnnotation,
    pub file_path: PathBuf,
    /// Source file for `ChangeType::Moved`.
    pub source_file: Option<PathBuf>,
    pub target_line: Option<u32>,
    pub line_range: LineRange,
    /// `false` when the only change was formatting or comments.
    pub structural_change: bool,
    pub content_hash: u64,
    /// Refactor classification copied from `EntityCoreData`; `None` for
    /// ordinary changes.
    pub refactor: Option<RefactorKind>,
    /// Number of inline comments anchored inside this entity's line range.
    pub comment_count: usize,
    /// `true` when the reviewer has visited and auto-marked this entity.
    pub reviewed: bool,
    /// Risk-tier assessment; `None` until tiers are computed after
    /// extraction completes (see `semantic::risk::compute_risk_tiers`).
    pub risk: Option<crate::semantic::risk::RiskAssessment>,
    /// `true` for synthetic whole-file rows built by
    /// [`fallback_summary_for_file`]. Risk classification needs this
    /// explicitly: a fallback row's `ChangeType` mirrors the file-level
    /// change, so it is otherwise indistinguishable from a real entity.
    pub fallback: bool,
}

impl EntitySummary {
    /// Build the render-time summary from cached core data. `comment_count`
    /// and `reviewed` start at their defaults; callers overlay live state.
    pub fn from_core(e: &EntityCoreData) -> Self {
        Self {
            id: e.id.clone(),
            display_name: e.id.display_name(),
            kind: e.kind,
            change: e.change,
            annotation: e.annotation,
            file_path: e.id.file_path.clone(),
            source_file: e.source_file.clone(),
            target_line: e.target_line,
            line_range: e.line_range,
            structural_change: e.structural_change,
            content_hash: e.content_hash,
            refactor: e.refactor.clone(),
            comment_count: 0,
            reviewed: false,
            risk: None,
            fallback: false,
        }
    }

    /// Mirror of [`EntityCoreData::is_behavior_preserving`] for render-time
    /// dimming and the `;` filter.
    pub fn is_behavior_preserving(&self) -> bool {
        behavior_preserving(self.refactor.as_ref())
    }
}

/// Shared behavior-preserving rule (see [`EntityCoreData::is_behavior_preserving`]).
fn behavior_preserving(refactor: Option<&RefactorKind>) -> bool {
    match refactor {
        None => false,
        Some(RefactorKind::Renamed { pure, .. }) => *pure,
        Some(RefactorKind::Moved { identical }) => *identical,
        Some(RefactorKind::Extracted { .. }) => true,
    }
}

#[cfg(test)]
mod peek_tests {
    use super::{body_peek_from, body_peek_from_body};

    #[test]
    fn body_peek_skips_subject_and_blank_lines() {
        assert_eq!(
            body_peek_from("subject\n\n  first body line\nsecond"),
            Some("first body line".to_owned())
        );
    }

    #[test]
    fn body_peek_none_for_subject_only() {
        assert_eq!(body_peek_from("subject"), None);
        assert_eq!(body_peek_from("subject\n\n  \n"), None);
        assert_eq!(body_peek_from(""), None);
    }

    #[test]
    fn body_peek_from_body_takes_first_nonempty_line() {
        assert_eq!(
            body_peek_from_body("\n\nwraps refresh() in backoff\nmore"),
            Some("wraps refresh() in backoff".to_owned())
        );
        assert_eq!(body_peek_from_body(""), None);
    }
}
