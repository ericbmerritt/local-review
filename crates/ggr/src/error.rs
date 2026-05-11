//! Error types for `ggr`.
use std::path::PathBuf;

use snafu::Snafu;

pub(crate) type Result<T> = std::result::Result<T, GgrError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub(crate) enum GgrError {
    #[snafu(display("gh is not on PATH; install the GitHub CLI (https://cli.github.com)"))]
    GhMissing { source: std::io::Error },

    #[snafu(display("gh failed: {message}"))]
    GhFailed {
        message: String,
        exit_code: Option<i32>,
    },

    #[snafu(display("PR #{pr} not found — check the number and that you are in the right repo"))]
    PrNotFound { pr: u64 },

    #[snafu(display(
        "repository '{repo}' not found on github.com — if this is a GitHub Enterprise repo, \
         use: ggr --url <host> {repo}#<pr>"
    ))]
    RepoNotFound { repo: String },

    #[snafu(display("gh output is not valid UTF-8: {source}"))]
    GhOutputEncoding { source: std::string::FromUtf8Error },

    #[snafu(display("failed to parse PR metadata: {source}"))]
    GhJsonParse { source: serde_json::Error },

    #[snafu(display("failed to parse review comment: {source}"))]
    ReviewCommentParse { source: serde_json::Error },

    #[snafu(display("failed to parse diff for {}: {message}", file.display()))]
    DiffParse { file: PathBuf, message: String },

    #[snafu(display("invalid PR reference: {raw}"))]
    InvalidPrRef { raw: String },

    #[snafu(display("invalid repository name '{repo_name}': expected 'owner/repo' format"))]
    InvalidRepoName { repo_name: String },

    #[snafu(display("invalid commit SHA '{sha}': expected 40 lowercase hex characters"))]
    InvalidCommitSha { sha: String },

    #[snafu(display("io error: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display("invalid draft: {reason}"))]
    InvalidDraft { reason: String },

    #[snafu(display("draft I/O error: {source}"))]
    DraftIo { source: std::io::Error },

    #[snafu(display("terminal is too narrow: {} columns (minimum 60)", cols))]
    TerminalTooNarrow { cols: u16 },

    #[snafu(display("terminal is too short: {} rows (minimum 10)", rows))]
    TerminalTooShort { rows: u16 },
}

impl From<local_review_core::Error> for GgrError {
    fn from(error: local_review_core::Error) -> Self {
        match error {
            local_review_core::Error::DiffParse { file, message } => {
                Self::DiffParse { file, message }
            }
            local_review_core::Error::InvalidChangeId { raw } => Self::Io {
                source: std::io::Error::other(format!(
                    "unexpected core error: invalid change id {raw}"
                )),
            },
            local_review_core::Error::InvalidCommitId { raw } => Self::Io {
                source: std::io::Error::other(format!(
                    "unexpected core error: invalid commit id {raw}"
                )),
            },
        }
    }
}
