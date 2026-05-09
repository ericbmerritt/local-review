//! Help screen overlay — re-exported from `local_review_core`.
//!
//! The implementation and keybinding reference live in
//! `local_review_core::tui::help_screen`. The help body is currently
//! jjr-specific; the `title` parameter allows ggr to reuse the render path
//! when its keybindings are defined. This file exists only to satisfy the
//! `mod help_screen;` declaration in `tui.rs`.

pub(super) use local_review_core::tui::help_screen::render;
