//! Shared core for local-first batched code review.
//!
//! Owns the pure data layers — diff parsing, fuzzy-anchoring, anchor types —
//! that both `jjr` (jj stacks) and `ggr` (GitHub PRs) build their own
//! comment models on top of. No IO, no clock, no subprocess.

pub mod anchoring;
pub mod change_id;
pub mod comment;
pub mod diff;
pub mod error;
pub mod revset_hash;

pub use anchoring::{match_anchor, match_description_anchor, AnchorOutcome};
pub use change_id::{ChangeId, CommitId};
pub use comment::{
    DescriptionAnchor, LineAnchor, MismatchReason, Side, CONTEXT_MAX, TARGET_TEXT_MAX,
    TRUNCATION_SUFFIX,
};
pub use diff::{Diff, DiffFile, Hunk, Line, LineKind};
pub use error::{Error, Result};
pub use revset_hash::RevsetHash;
