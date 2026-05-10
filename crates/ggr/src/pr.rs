//! PR and commit data model for `ggr`.
//!
//! `PrDetails` holds the resolved commit list for a PR; `CommitEntry` is a
//! single commit reference with its short SHA and title. `ReviewThread` groups
//! the flat list of inline review comments returned by the GitHub pull request
//! comments API into per-root-comment threads.

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

/// A general (non-inline) PR comment.
#[derive(Debug, Clone)]
pub(crate) struct PrComment {
    pub(crate) author: String,
    pub(crate) body: String,
}

/// One comment within an inline review thread.
///
/// `id` is GitHub's review comment ID; it is needed to post replies (P3).
#[derive(Debug, Clone)]
pub(crate) struct ThreadComment {
    /// GitHub's numeric review comment ID.
    #[expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")]
    pub(crate) id: u64,
    #[expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")]
    pub(crate) author: String,
    /// ISO 8601 timestamp as returned by GitHub (e.g. `"2024-01-15T10:30:00Z"`).
    #[expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")]
    pub(crate) created_at: String,
    /// Comment body as returned by the GitHub API.
    ///
    /// # Security
    /// Content originates from GitHub users and is untrusted.
    /// Strip terminal control characters before passing to any renderer to
    /// prevent ANSI escape injection.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) body: String,
}

/// A grouped inline review thread anchored to a file path in the diff.
///
/// GitHub's pull request comments API returns a flat list of comments.  Each
/// comment is either a root comment (no `in_reply_to_id`) or a reply to a
/// root.  `ReviewThread` presents a root comment and all of its replies as a
/// single logical unit.
///
/// `position` is a 1-based integer offset into the diff hunk as defined by the
/// GitHub API; it is `None` when the thread is outdated (the diff line the
/// thread was anchored to no longer exists in the current PR head).  Call
/// [`ReviewThread::is_outdated`] rather than checking `position` directly.
#[derive(Debug, Clone)]
pub(crate) struct ReviewThread {
    /// Repo-root-relative file path the thread is anchored to.
    ///
    /// From the GitHub API; strip control characters before rendering.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) path: String,
    /// 1-based diff-offset position in the PR diff.
    pub(crate) position: Option<u32>,
    /// Commit SHA at which the root comment was first made.
    ///
    /// From the GitHub API; strip control characters before rendering.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) original_commit_id: String,
    /// Root (first) comment for the thread.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) root: ThreadComment,
    /// Replies to the root comment, in API-returned order.
    pub(crate) replies: Vec<ThreadComment>,
}

impl ReviewThread {
    /// Returns `true` when the anchored diff line no longer exists in the PR head.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) fn is_outdated(&self) -> bool {
        self.position.is_none()
    }
}

/// A resolved pull request with its ordered commit list (oldest-first).
#[derive(Debug, Clone)]
pub(crate) struct PrDetails {
    pub(crate) number: u64,
    pub(crate) title: String,
    /// PR description body (may be empty).
    pub(crate) body: String,
    /// General (non-inline) PR comments, oldest-first.
    pub(crate) comments: Vec<PrComment>,
    /// Validated `owner/repo` slug from `headRepository.nameWithOwner`, used for diff API calls.
    pub(crate) repo_name: crate::gh::RepoName,
    /// GHE hostname for `gh api --hostname`. `None` → github.com.
    pub(crate) hostname: Option<String>,
    /// Ordered commits, oldest-first (as returned by `gh pr view --json commits`).
    pub(crate) commits: Vec<CommitEntry>,
    /// Inline review threads, grouped by root comment, oldest-first within each thread.
    ///
    /// `None` until populated by [`crate::gh::fetch_review_threads`].
    pub(crate) review_threads: Option<Vec<ReviewThread>>,
}
