//! Multi-line text input widget — re-exported from `local_review_core`.
//!
//! The implementation lives in `local_review_core::tui::textarea` and is
//! shared between `jjr` and `ggr`. This file exists only to satisfy the
//! `mod textarea;` declaration in `tui.rs`.

pub(crate) use local_review_core::tui::textarea::TextArea;
