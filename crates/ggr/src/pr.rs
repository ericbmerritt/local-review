//! PR and commit data model for `ggr`.
//!
//! `PrDetails` holds the resolved commit list for a PR; `CommitEntry` is a
//! single commit reference with its short SHA and title. `ReviewThread` groups
//! the flat list of inline review comments returned by the GitHub pull request
//! comments API into per-root-comment threads.

use crate::error::GgrError;
use local_review_core::comment::Side;
use local_review_core::util::strip_controls;
use local_review_core::Severity;

/// Validate a single path/hostname segment: non-empty, no `..`, no leading or
/// trailing dot, only alphanumeric and separator chars (`-`, `_`, `.`), at
/// least one alphanumeric.
///
/// Used by both [`RepoName::try_from`] for owner/repo segments and by
/// `pr_ref::extract_host` for hostname validation.
pub(crate) fn valid_segment(seg: &str) -> bool {
    !seg.is_empty()
        && !seg.contains("..")
        && !seg.starts_with('.')
        && !seg.ends_with('.')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && seg.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Validate a forward-slash–delimited file path from a PR diff.
///
/// The path is stored as a JSON field, not used directly as a filesystem path,
/// so only path-traversal patterns need rejection: empty segments, bare `.`,
/// and `..` components. Leading dots are allowed for hidden files and
/// directories (`.gitignore`, `.github/`, etc.).
pub(crate) fn valid_file_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

// ── RepoName ──────────────────────────────────────────────────────────────────

/// A validated `owner/repo` slug.
///
/// Constructed via [`TryFrom<&str>`], which enforces the `owner/repo` format
/// before accepting the value.  Use [`RepoName::as_str`] to recover the
/// underlying string for API endpoint construction.
#[derive(Debug, Clone)]
pub(crate) struct RepoName(String);

impl TryFrom<&str> for RepoName {
    type Error = GgrError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let err = || GgrError::InvalidRepoName {
            repo_name: strip_controls(s),
        };
        let (owner, repo) = s.split_once('/').ok_or_else(err)?;
        if !valid_segment(owner) || !valid_segment(repo) {
            return Err(err());
        }
        Ok(Self(s.to_owned()))
    }
}

impl RepoName {
    /// Returns the underlying `owner/repo` string.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

// ── domain types ─────────────────────────────────────────────────────────────

/// A validated 40-character lowercase-hex SHA-1 commit identifier.
///
/// Constructed via [`TryFrom<&str>`], which enforces the 40-char lowercase-hex
/// constraint at the boundary.  Use [`CommitSha::as_str`] to recover the raw
/// string for API endpoint construction or display.
#[derive(Debug, Clone)]
pub(crate) struct CommitSha(String);

impl TryFrom<&str> for CommitSha {
    type Error = GgrError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let valid = s.len() == 40 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(GgrError::InvalidCommitSha {
                sha: strip_controls(s),
            })
        }
    }
}

impl CommitSha {
    /// Returns the underlying 40-character SHA string.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One commit in a PR, as resolved by `gh pr view --json commits`.
///
/// # Security
/// All string fields originate from GitHub users and are untrusted.
/// Strip terminal control characters before passing to any renderer to
/// prevent ANSI escape injection.
#[derive(Debug, Clone)]
pub(crate) struct CommitEntry {
    /// Full 40-char validated SHA.
    pub(crate) sha: CommitSha,
    /// First 8 characters of the SHA, for display.
    pub(crate) short_sha: String,
    /// First line of the commit message (the `messageHeadline` field from GitHub).
    pub(crate) title: String,
    /// Commit message body (the `messageBody` field from GitHub); empty when
    /// the commit has no body. Untrusted — strip controls before rendering.
    pub(crate) body: String,
}

/// A general (non-inline) PR comment.
///
/// # Security
/// All string fields originate from GitHub users and are untrusted.
/// Strip terminal control characters before passing to any renderer to
/// prevent ANSI escape injection.
#[derive(Debug, Clone)]
pub(crate) struct PrComment {
    /// Comment author login as returned by the GitHub API.
    pub(crate) author: String,
    /// Comment body as returned by the GitHub API.
    pub(crate) body: String,
}

/// One comment within an inline review thread.
///
/// `id` is GitHub's review comment ID; it is needed to post replies.
///
/// # Security
/// All string fields originate from GitHub users and are untrusted.
/// Strip terminal control characters before passing to any renderer to
/// prevent ANSI escape injection.
#[derive(Debug, Clone)]
pub(crate) struct ThreadComment {
    /// GitHub's numeric review comment ID.
    pub(crate) id: u64,
    /// Comment author login as returned by the GitHub API.
    pub(crate) author: String,
    /// ISO 8601 creation timestamp (e.g. `"2024-01-15T10:30:00Z"`).
    pub(crate) created_at: String,
    /// Comment body as returned by the GitHub API.
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
    /// From the GitHub API; sanitized at construction (control characters stripped).
    pub(crate) path: String,
    /// 1-based diff-offset position in the PR diff.
    pub(crate) position: Option<u32>,
    /// Validated 40-char lowercase-hex SHA.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) original_commit_id: CommitSha,
    /// Root (first) comment for the thread.
    pub(crate) root: ThreadComment,
    /// Replies to the root comment, in API-returned order.
    pub(crate) replies: Vec<ThreadComment>,
    /// 1-based line number in the **new** version of the file that the thread
    /// anchors to (`side: "RIGHT"` in the GitHub API).  `None` for hunk-context
    /// lines (which have no file-side line number) and for outdated threads.
    pub(crate) line: Option<u32>,
    /// 1-based line number in the **old** version of the file that the thread
    /// anchors to (`side: "LEFT"` in the GitHub API).  `None` for right-side and
    /// hunk-context threads, and for outdated threads.
    pub(crate) original_line: Option<u32>,
    /// Which side of the diff this thread is anchored to: [`Side::Old`] (GitHub
    /// `"LEFT"`) or [`Side::New`] (GitHub `"RIGHT"`).  `None` when the GitHub API
    /// returns `null` (hunk-context or outdated threads).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by thread rendering TUI (not yet built)")
    )]
    pub(crate) diff_side: Option<Side>,
    /// Review severity. GitHub threads do not carry an explicit severity field;
    /// the API always maps to [`Severity::Note`].
    pub(crate) severity: Severity,
}

impl ReviewThread {
    /// Returns `true` when the anchored diff line no longer exists in the PR head.
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
    pub(crate) repo_name: RepoName,
    /// GHE hostname for `gh api --hostname`. `None` → github.com.
    pub(crate) hostname: Option<String>,
    /// Ordered commits, oldest-first (as returned by `gh pr view --json commits`).
    pub(crate) commits: Vec<CommitEntry>,
    /// Inline review threads, grouped by root comment, oldest-first within each thread.
    pub(crate) review_threads: Vec<ReviewThread>,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{CommitSha, RepoName};

    #[test]
    fn valid_commit_sha_accepts_40_lowercase_hex() {
        assert!(CommitSha::try_from("a3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4").is_ok());
    }

    #[test]
    fn valid_commit_sha_rejects_39_chars() {
        assert!(CommitSha::try_from("a3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b").is_err());
    }

    #[test]
    fn valid_commit_sha_rejects_41_chars() {
        assert!(CommitSha::try_from("a3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b40").is_err());
    }

    #[test]
    fn valid_commit_sha_rejects_empty() {
        assert!(CommitSha::try_from("").is_err());
    }

    #[test]
    fn valid_commit_sha_rejects_non_hex_char() {
        assert!(CommitSha::try_from("g3b4c5d6e7f8a3b4c5d6e7f8a3b4c5d6e7f8a3b4").is_err());
    }

    #[test]
    fn valid_commit_sha_rejects_uppercase_hex() {
        assert!(CommitSha::try_from("A3B4C5D6E7F8A3B4C5D6E7F8A3B4C5D6E7F8A3B4").is_err());
    }

    #[test]
    fn root_with_invalid_sha_returns_err() {
        // A crafted SHA containing ANSI escape sequences must not be echoed
        // verbatim into the error display; control characters are stripped
        // before the error is constructed, so the returned Err still carries
        // a sanitised (non-40-hex) string and is not Ok.
        let crafted = "\x1b[31mevil\x1b[0m";
        let result = CommitSha::try_from(crafted);
        assert!(result.is_err());
        if let Err(crate::error::GgrError::InvalidCommitSha { sha }) = result {
            assert!(
                !sha.chars().any(char::is_control),
                "control characters must be stripped from error sha: {sha:?}"
            );
        }
    }

    #[test]
    fn validate_repo_name_accepts_valid_slug() {
        assert!(RepoName::try_from("owner/repo").is_ok());
    }

    #[test]
    fn validate_repo_name_rejects_missing_slash() {
        assert!(RepoName::try_from("ownerrepo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_extra_slash() {
        assert!(RepoName::try_from("owner/repo/extra").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_empty_owner() {
        assert!(RepoName::try_from("/repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_empty_repo() {
        assert!(RepoName::try_from("owner/").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_bare_slash() {
        assert!(RepoName::try_from("/").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_whitespace_owner() {
        assert!(RepoName::try_from(" /repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_whitespace_repo() {
        assert!(RepoName::try_from("owner/ ").is_err());
    }

    #[test]
    fn validate_repo_name_accepts_dots_hyphens_underscores() {
        assert!(RepoName::try_from("my-org/my.repo_v1").is_ok());
    }

    #[test]
    fn validate_repo_name_rejects_dotdot_owner() {
        assert!(RepoName::try_from("../repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_dotdot_repo() {
        assert!(RepoName::try_from("owner/..").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_all_underscore_owner() {
        assert!(RepoName::try_from("_/repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_all_underscore_repo() {
        assert!(RepoName::try_from("owner/_").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_single_dot_owner() {
        assert!(RepoName::try_from("./repo").is_err());
    }

    #[test]
    fn validate_repo_name_accepts_leading_hyphen() {
        // valid_segment does not restrict hyphen position; GitHub API enforces naming rules.
        assert!(RepoName::try_from("-owner/repo").is_ok());
    }

    #[test]
    fn validate_repo_name_accepts_trailing_hyphen() {
        assert!(RepoName::try_from("owner/repo-").is_ok());
    }

    #[test]
    fn validate_repo_name_rejects_leading_dot_owner() {
        assert!(RepoName::try_from(".owner/repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_trailing_dot_owner() {
        assert!(RepoName::try_from("owner./repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_leading_dot_repo() {
        assert!(RepoName::try_from("owner/.repo").is_err());
    }

    #[test]
    fn validate_repo_name_rejects_trailing_dot_repo() {
        assert!(RepoName::try_from("owner/repo.").is_err());
    }

    #[test]
    fn validate_repo_name_accepts_internal_dot() {
        assert!(RepoName::try_from("my.org/my.repo").is_ok());
    }

    #[test]
    fn validate_repo_name_ansi_escape_is_stripped_from_error() {
        // ANSI escape sequences in the repo slug must not survive into the
        // error's repo_name field; strip_controls is applied before embedding.
        let crafted = "\x1b[31mevil\x1b[0m/repo";
        let result = RepoName::try_from(crafted);
        assert!(result.is_err());
        if let Err(crate::error::GgrError::InvalidRepoName { repo_name }) = result {
            assert!(
                !repo_name.chars().any(char::is_control),
                "control characters must be stripped from error repo_name: {repo_name:?}"
            );
        }
    }

    // ── valid_file_path ────────────────────────────────────────────────────────

    #[test]
    fn valid_file_path_accepts_normal_path() {
        assert!(super::valid_file_path("src/main.rs"));
    }

    #[test]
    fn valid_file_path_accepts_hidden_file_at_root() {
        assert!(super::valid_file_path(".gitignore"));
    }

    #[test]
    fn valid_file_path_accepts_hidden_directory() {
        assert!(super::valid_file_path(".github/workflows/ci.yml"));
    }

    #[test]
    fn valid_file_path_accepts_dotfile_in_subdir() {
        assert!(super::valid_file_path("config/.env.example"));
    }

    #[test]
    fn valid_file_path_rejects_dotdot_segment() {
        assert!(!super::valid_file_path("foo/../bar"));
    }

    #[test]
    fn valid_file_path_rejects_bare_dotdot() {
        assert!(!super::valid_file_path(".."));
    }

    #[test]
    fn valid_file_path_rejects_dot_segment() {
        assert!(!super::valid_file_path("foo/./bar"));
    }

    #[test]
    fn valid_file_path_rejects_absolute_path() {
        assert!(!super::valid_file_path("/etc/passwd"));
    }

    #[test]
    fn valid_file_path_rejects_empty_string() {
        assert!(!super::valid_file_path(""));
    }

    #[test]
    fn valid_file_path_rejects_trailing_slash() {
        assert!(!super::valid_file_path("src/"));
    }

    #[test]
    fn valid_file_path_rejects_double_slash() {
        assert!(!super::valid_file_path("src//main.rs"));
    }
}
