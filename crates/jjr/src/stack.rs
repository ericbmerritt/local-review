pub use local_review_core::revset_hash::RevsetHash;

use crate::change_id::{ChangeId, CommitId};

/// A resolved stack of jj changes for the given revset.
#[derive(Debug, Clone)]
pub struct ResolvedStack {
    /// The revset string used for the resolution. After a fallback (e.g. when
    /// `trunk()` is unresolvable) this holds the *fallback* revset, not the
    /// original — keeping `revset_hash` and `entries` in agreement.
    pub revset: String,
    /// BLAKE3 hash of the canonicalized revset. Stable key for cursor storage.
    pub revset_hash: RevsetHash,
    /// Ordered entries, oldest-first (as `jj log` returns them for typical
    /// ancestry revsets).
    pub entries: Vec<StackEntry>,
}

/// One change in a resolved stack.
#[derive(Debug, Clone)]
pub struct StackEntry {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    /// First line of the change description.
    pub description: String,
}
