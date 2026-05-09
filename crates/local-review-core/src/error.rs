//! Errors raised by the shared review core.
//!
//! Kept narrowly scoped — only failures that originate inside the core
//! belong here. Consumer crates (`jjr`, `ggr`) wrap or convert these
//! into their own error types as needed.

use std::path::PathBuf;

use snafu::Snafu;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("failed to parse diff for {}: {message}", file.display()))]
    DiffParse { file: PathBuf, message: String },

    #[snafu(display("invalid change id: {raw}"))]
    InvalidChangeId { raw: String },

    #[snafu(display("invalid commit id: {raw}"))]
    InvalidCommitId { raw: String },
}
