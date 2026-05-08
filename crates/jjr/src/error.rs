use std::path::PathBuf;

use snafu::Snafu;

pub type Result<T> = std::result::Result<T, JjrError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum JjrError {
    #[snafu(display("jj is not on PATH; install jujutsu (https://github.com/jj-vcs/jj)"))]
    JjMissing { source: std::io::Error },

    /// `message` is jj's stderr verbatim. In jjr's local-CLI context the
    /// audience is the same user that invoked jj. If jjr is ever embedded in a
    /// daemon, server, or log-aggregator pipeline where output crosses an
    /// audience boundary, sanitize before forwarding.
    #[snafu(display("jj failed: {message}"))]
    JjFailed {
        message: String,
        exit_code: Option<i32>,
    },

    #[snafu(display("invalid change id: {raw}"))]
    InvalidChangeId { raw: String },

    #[snafu(display("invalid commit id: {raw}"))]
    InvalidCommitId { raw: String },

    #[snafu(display("failed to parse diff for {}: {message}", file.display()))]
    DiffParse { file: PathBuf, message: String },

    #[snafu(display("file {} contains invalid UTF-8 and was excluded", file.display()))]
    InvalidUtf8 { file: PathBuf },

    #[snafu(display("jj output is not valid UTF-8: {source}"))]
    JjOutputEncoding { source: std::string::FromUtf8Error },

    /// `raw` carries jj's stdout verbatim for diagnosis. Same audience caveat
    /// as `JjFailed`: sanitize if forwarding outside a local-CLI context.
    #[snafu(display("unexpected output from jj: {raw}"))]
    JjUnexpectedOutput { raw: String },

    #[snafu(display("revset {revset} matched no changes"))]
    RevsetNoMatch { revset: String },

    /// `raw` carries jj's stdout verbatim. Same audience caveat as `JjFailed`:
    /// sanitize if forwarding outside a local-CLI context.
    #[snafu(display("revset {revset} matched multiple changes:\n{raw}"))]
    RevsetAmbiguous { revset: String, raw: String },

    #[snafu(display("io error: {source}"))]
    Io { source: std::io::Error },

    #[snafu(display(
        "not inside a jj repo: searched up from {} and found no .jj directory",
        cwd.display()
    ))]
    NotInJjRepo { cwd: PathBuf },

    #[snafu(display("terminal is too narrow: {} columns (minimum 60)", cols))]
    TerminalTooNarrow { cols: u16 },

    #[snafu(display("terminal is too short: {} rows (minimum 10)", rows))]
    TerminalTooShort { rows: u16 },

    #[snafu(display(
        "schema version mismatch: file has {found}, expected {expected}; \
         run `jjr clear <revset>` to remove and re-author incompatible records"
    ))]
    SchemaVersionMismatch { found: String, expected: String },

    #[snafu(display("two comments share created_at {timestamp}; cannot uniquely identify"))]
    DuplicateCommentTimestamp { timestamp: String },

    #[snafu(display(
        "comment with created_at {timestamp} not found in {}",
        file.display()
    ))]
    CommentNotFound { file: PathBuf, timestamp: String },

    #[snafu(display("line-scoped comment requires at least one of old_line or new_line"))]
    LineAnchorMissingLineNumber,

    #[snafu(display(
        "{} contains a non-stack-scoped record; file is meant to be stack-scope-only",
        path.display()
    ))]
    StackFileCorruption { path: PathBuf },

    #[snafu(display("no comments to send for revset {revset}"))]
    EmptyPacket { revset: String },

    #[snafu(display("no comments to export for revset {revset}"))]
    NoCommentsToExport { revset: String },

    #[snafu(display("clear aborted"))]
    ClearAborted,

    #[snafu(display("{}", agent_missing_message(tool)))]
    AgentMissing {
        tool: String,
        source: std::io::Error,
    },

    #[snafu(display(
        "{tool} exited with {}: see stderr above for details",
        exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string())
    ))]
    AgentFailed {
        tool: String,
        exit_code: Option<i32>,
    },

    #[snafu(display(
        "review packet ({size} bytes) exceeds {limit}-byte argv limit; chunk the stack or omit context"
    ))]
    PromptTooLarge { size: usize, limit: usize },
}

/// Format an `AgentMissing` display string. Appends the Claude install URL
/// only when the configured tool is the default (`claude`); for any other
/// tool, jjr has no canonical install pointer to offer.
fn agent_missing_message(tool: &str) -> String {
    if tool == crate::agent_config::DEFAULT_TOOL {
        "claude is not on PATH; install Claude CLI (https://docs.anthropic.com/en/docs/claude-code)"
            .to_owned()
    } else {
        format!("{tool} is not on PATH")
    }
}
