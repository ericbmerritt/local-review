//! Core entity types produced by semantic extraction.
//!
//! `EntityCoreData` is the cache-safe output of extraction. It carries the
//! semantic identity plus the diff classification, but not the raw source text
//! (which is large). Display names and comment counts are computed at render
//! time from this type and the live comment store.

use std::path::PathBuf;

pub use super::entity_id::EntityId;

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
    /// Number of inline comments anchored inside this entity's line range.
    pub comment_count: usize,
}
