//! Shared core for local-first batched code review.
//!
//! # Layers
//!
//! ## Pure data layer
//!
//! The following modules are **pure**: no IO, no clock, no subprocess.
//! They take data and return data; callers (and tests) can drive them without
//! any side-effects or runtime setup.
//!
//! - [`anchoring`] — fuzzy comment re-anchoring after diffs change
//! - [`diff`] — unified-diff parser
//! - [`revset_hash`] — stable hash of a jj revset expression
//! - [`severity`] — comment severity enum
//! - [`comment`], [`change_id`], [`error`] — shared data types
//!
//! ## Semantic extraction layer
//!
//! The [`semantic`] module provides tree-sitter-based entity extraction and
//! diff computation. It is feature-gated: each language grammar is an optional
//! dependency; the `default` feature enables all 13 supported languages.
//!
//! ## Terminal rendering layer
//!
//! The [`tui`] module adds terminal-rendering and clock dependencies
//! (`ratatui`, `crossterm`, `time`).  This is intentional: the shared TUI
//! infrastructure lives here so that both `jjr` and `ggr` can use it without
//! duplicating code.  The module is parameterised by the [`tui::ReviewSurface`]
//! trait so each binary supplies its own surface implementation.

pub mod anchoring;
pub mod change_id;
pub mod comment;
pub mod diff;
pub mod error;
pub mod highlight;
pub mod revset_hash;
pub mod semantic;
pub mod severity;
pub mod startup_spinner;
pub mod tui;
pub mod util;

pub use anchoring::{
    match_anchor, match_anchor_with_entity, match_description_anchor, AnchorOutcome,
};
pub use change_id::{ChangeId, CommitId};
pub use comment::{
    AnchorFingerprint, DescriptionAnchor, LineAnchor, MismatchReason, Side, CONTEXT_MAX,
    TARGET_TEXT_MAX, TRUNCATION_SUFFIX,
};
pub use diff::{Diff, DiffFile, Hunk, Line, LineKind};
pub use error::{Error, Result};
pub use revset_hash::RevsetHash;
pub use severity::Severity;
