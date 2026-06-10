//! Core entity types produced by semantic extraction.
//!
//! `EntityCoreData` is the cache-safe output of extraction. It carries the
//! semantic identity plus the diff classification, but not the raw source text
//! (which is large). Display names and comment counts are computed at render
//! time from this type and the live comment store.

use std::path::PathBuf;

/// Opaque identity placeholder used in Phase 1.
///
/// Encoded as `<file_path>::<entity_kind>::<scope_path>` where `<scope_path>`
/// is the `::` -separated chain of container names and the entity's own name
/// (e.g., `AuthService::authenticate`). Phase 2 replaces this newtype with
/// the structured `EntityId` without changing `EntityCoreData`'s field shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlaceholderEntityId(pub String);

/// Classification of a code entity by its syntactic kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Moved,
}

/// What specifically changed within a modified entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Cached to disk by Phase 2; `display_name` and `comment_count` are computed
/// at render time and therefore not stored here.
#[derive(Debug, Clone)]
pub struct EntityCoreData {
    /// Opaque identity (Phase 2 replaces with structured `EntityId`).
    pub id: PlaceholderEntityId,
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
    /// The entity's identity string (becomes `PlaceholderEntityId`).
    pub id_str: String,
    /// Scope portion only (everything after `<file>::<kind>::`), e.g.
    /// `Session::refresh`. Used by the Container Rule to detect parent-child
    /// relationships without brittle `id_str` prefix matching.
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
    /// Used to distinguish `SigChanged` from `SigAndBody`.
    pub body_hash: u64,
    /// File path; forwarded into `EntityCoreData.id`.
    pub file_path: String,
}
