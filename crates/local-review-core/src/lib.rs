//! Shared core for local-first batched code review.
//!
//! This crate is the eventual home of the diff parser, fuzzy-anchoring
//! machinery, comment storage, and TUI render layer that are currently
//! still inside `jjr`. The two consumers — `jjr` (jj stacks) and `ggr`
//! (GitHub PRs) — will plug a thin source-of-truth shell into the same
//! pure core.
//!
//! Code migration is intentionally deferred so the workspace conversion
//! lands as a reviewable structural change before any logic moves.
