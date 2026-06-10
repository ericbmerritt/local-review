//! The `SemanticExtractor` trait and per-extraction error type.

use std::path::PathBuf;

use snafu::Snafu;

use crate::semantic::entity::RawEntity;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum ExtractError {
    #[snafu(display("no extractor registered for {}", file_path.display()))]
    UnsupportedLanguage { file_path: PathBuf },

    #[snafu(display("tree-sitter parser could not be initialised: {detail}"))]
    ParserInit { detail: String },

    #[snafu(display("tree-sitter parse produced ERROR nodes in {}", file_path.display()))]
    ParseContainsErrors { file_path: PathBuf },
}

/// Extraction result for one file. `Ok(vec)` may be empty for valid files
/// with no extractable entities. `Err` means the file becomes a fallback row.
pub type ExtractResult = Result<Vec<RawEntity>, ExtractError>;

/// An extractor that can identify semantic entities within a source file.
///
/// All methods take `&self`; any internal parser state uses thread-local storage
/// so the registry can be shared across threads.
pub trait SemanticExtractor: Send + Sync {
    /// Short identifier for the language (e.g., `"rust"`, `"typescript"`).
    fn id(&self) -> &'static str;

    /// File extensions handled by this extractor (lowercase, no leading dot).
    fn extensions(&self) -> &[&str];

    /// Extract semantic entities from `content`.
    ///
    /// Returns `Err(ParseContainsErrors)` when tree-sitter produces ERROR nodes.
    /// Returns `Ok([])` for valid but entity-free files.
    fn extract(&self, content: &str, file_path: &str) -> ExtractResult;
}

/// Compute the first-8-bytes blake3 hash of a string, returned as u64.
pub(crate) fn content_hash(s: &str) -> u64 {
    let bytes = blake3::hash(s.as_bytes());
    let arr: [u8; 8] = bytes.as_bytes()[..8].try_into().unwrap_or([0u8; 8]);
    u64::from_le_bytes(arr)
}

/// True if `line` is a comment, decorator, or annotation — not a declaration.
fn is_preamble_line(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t.starts_with("//")
        || t.starts_with('#')
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with('@')
}

/// Hash the first declaration line of `content`, skipping doc-comments,
/// decorators, and annotations that precede it.
///
/// Using the first non-preamble line as the signature avoids classifying
/// comment-only edits as signature changes.
pub(crate) fn sig_hash(content: &str) -> u64 {
    let sig = content.lines().find(|l| !is_preamble_line(l)).unwrap_or("");
    content_hash(sig.trim())
}

/// Hash the body of `content` — everything after the first declaration line.
///
/// A separate body hash lets the differ distinguish `SigChanged` (body
/// identical, declaration changed) from `SigAndBody` (both changed).
pub(crate) fn body_hash(content: &str) -> u64 {
    let mut lines = content.lines();
    // Skip preamble lines
    for line in lines.by_ref() {
        if !is_preamble_line(line) {
            break; // consumed the declaration line
        }
    }
    // Everything remaining is the body
    let body: String = lines.collect::<Vec<_>>().join("\n");
    content_hash(&body)
}

/// Build an `EntityId` from a scope string and file path.
///
/// `scope` is the `::` -separated chain including the entity name, e.g.
/// `"Session::refresh"`. `ordinal` defaults to 0; callers that need ordinal
/// disambiguation must post-process the batch via `entity_id::assign_ordinals`.
pub(crate) fn build_entity_id(
    file_path: &str,
    scope: &str,
    ordinal: u32,
) -> crate::semantic::entity_id::EntityId {
    let chain: Vec<String> = scope
        .split("::")
        .map(crate::util::strip_controls)
        .filter(|s| !s.is_empty())
        .collect();
    crate::semantic::entity_id::EntityId::new(
        PathBuf::from(file_path),
        chain,
        None, // signature_key: computed in a future phase
        ordinal,
    )
}
