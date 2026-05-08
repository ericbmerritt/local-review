//! Shared core for local-first batched code review.
//!
//! Owns the pure data layers — diff parsing, fuzzy-anchoring, comment
//! storage, TUI render — that both `jjr` (jj stacks) and `ggr` (GitHub
//! PRs) plug a thin source-of-truth shell into. No IO, no clock, no
//! subprocess.
//!
//! Code migration from `jjr` happens in stages; today this crate owns
//! diff parsing only.

pub mod diff;
pub mod error;

pub use diff::{Diff, DiffFile, Hunk, Line, LineKind};
pub use error::{Error, Result};
