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

    #[snafu(display("terminal is too narrow: {} columns (minimum 60)", cols))]
    TerminalTooNarrow { cols: u16 },

    #[snafu(display("terminal is too short: {} rows (minimum 10)", rows))]
    TerminalTooShort { rows: u16 },
}
