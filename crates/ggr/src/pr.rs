//! PR and commit data model for `ggr`.
//!
//! `PrDetails` holds the resolved commit list for a PR; `CommitEntry` is a
//! single commit reference with its short SHA and title.

/// One commit in a PR, as resolved by `gh pr view --json commits`.
#[derive(Debug, Clone)]
pub(crate) struct CommitEntry {
    /// Full 40-char SHA.
    pub(crate) sha: String,
    /// First 8 characters of the SHA, for display.
    pub(crate) short_sha: String,
    /// First line of the commit message.
    pub(crate) title: String,
}

/// A resolved pull request with its ordered commit list (oldest-first).
#[derive(Debug, Clone)]
pub(crate) struct PrDetails {
    pub(crate) number: u64,
    pub(crate) title: String,
    /// `owner/repo` slug from `headRepository.nameWithOwner`, used for diff API calls.
    pub(crate) repo_name: String,
    /// GHE hostname for `gh api --hostname`. `None` → github.com.
    pub(crate) hostname: Option<String>,
    /// Ordered commits, oldest-first (as returned by `gh pr view --json commits`).
    pub(crate) commits: Vec<CommitEntry>,
}
