//! Shared terminal UI infrastructure for local-first batched code review.
//!
//! This module owns all rendering logic that is the same regardless of whether
//! the reviewer is walking a local jj stack (`jjr`) or a GitHub pull request
//! (`ggr`). Per-tool behaviour is plugged in through the [`ReviewSurface`]
//! trait.
//!
//! ## Layout rule
//! The `tui.rs` + `tui/` layout (no `mod.rs`) is required by the workspace's
//! `mod_module_files = "deny"` / `self_named_module_files = "allow"` policy.

pub mod app;
pub mod composer;
pub mod composer_overlay;
pub mod diff_view;
pub mod file_picker;
pub mod help_screen;
pub mod textarea;

pub use app::{
    footer_text_for_width, run_app, App, AppError, BaseViews, DiffMode, Edge, EffectiveDiffMode,
    ExtraScreenContext, ReviewSurfaceExt, TransitionMode, TransitionState, MIN_COLS, MIN_ROWS,
    SIDE_BY_SIDE_GUTTER_WIDTH, SIDE_BY_SIDE_MIN_WIDTH,
};
pub use diff_view::{
    collect_context, CommentIndex, DiffView, InlineComment, PairedRow, RenderedLine,
    RenderedLineKind,
};
pub use file_picker::{FilePickerEntry, FilePickerState};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::severity::Severity;

// ---------------------------------------------------------------------------
// Scrollbar utilities — shared by file_picker, diff panes, and extra screens.
// ---------------------------------------------------------------------------

/// Width (cells) of the vertical scrollbar strip on the right edge of a
/// paginated body. One column is reserved when content overflows the viewport.
pub const SCROLLBAR_WIDTH: u16 = 1;

/// Compute the overflow parameters needed to build a `ScrollbarState` for a
/// paginated body of `total_lines` rows with topmost-visible row `scroll` in
/// a viewport `viewport_rows` rows tall.
///
/// Returns `None` when the content fits in the viewport (or the viewport is
/// zero-height); the caller should skip the scrollbar in that case.
///
/// Returns `Some((content_length, position))` where `content_length` is the
/// number of scrollable positions and `position` is the clamped scroll index.
pub fn scrollbar_overflow_for_view(
    total_lines: usize,
    scroll: u16,
    viewport_rows: u16,
) -> Option<(usize, usize)> {
    if viewport_rows == 0 {
        return None;
    }
    let viewport_usize = usize::from(viewport_rows);
    if total_lines <= viewport_usize {
        return None;
    }
    let max_scroll = total_lines - viewport_usize;
    let position = usize::from(scroll).min(max_scroll);
    let content_length = max_scroll + 1;
    Some((content_length, position))
}

/// Build the [`ScrollbarState`] for a paginated body.
///
/// Thin shell over [`scrollbar_overflow_for_view`] that lifts the tuple
/// into ratatui's `ScrollbarState`. Returns `None` when no scrollbar is
/// needed.
pub fn scrollbar_state_for_view(
    total_lines: usize,
    scroll: u16,
    viewport_rows: u16,
) -> Option<ScrollbarState> {
    let (content_length, position) =
        scrollbar_overflow_for_view(total_lines, scroll, viewport_rows)?;
    Some(ScrollbarState::new(content_length).position(position))
}

/// Split `area` into a body region and an optional [`SCROLLBAR_WIDTH`]-col
/// scrollbar strip on the right edge.
///
/// Returns `(body_area, scrollbar_slot)`. The slot is `None` when no scrollbar
/// was requested (`with_scrollbar = false`) **or** when the area is too narrow
/// to host both the body and a scrollbar column. In both cases the body keeps
/// the full original area.
pub fn split_body_for_scrollbar(area: Rect, with_scrollbar: bool) -> (Rect, Option<Rect>) {
    if !with_scrollbar || area.width <= SCROLLBAR_WIDTH {
        return (area, None);
    }
    let split =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(SCROLLBAR_WIDTH)]).split(area);
    (split[0], Some(split[1]))
}

/// Build the right-edge scrollbar widget shared by every paginated view.
/// Centralises the orientation and per-element style choices so all screens
/// render the same glyphs and colors.
pub fn view_scrollbar() -> Scrollbar<'static> {
    Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .track_style(Style::default().fg(Color::DarkGray))
        .thumb_style(Style::default().fg(Color::Gray))
        .begin_style(Style::default().fg(Color::DarkGray))
        .end_style(Style::default().fg(Color::DarkGray))
}

/// One-shot layout helper for paginated views that want a scrollbar.
///
/// Combines [`scrollbar_state_for_view`] and [`split_body_for_scrollbar`] into
/// a single call so both pieces of scroll state are computed together.
///
/// Returns `(body, sb_area, sb_state)`:
/// - `body` — area into which the view's content is rendered; shrinks by
///   [`SCROLLBAR_WIDTH`] when a scrollbar will be drawn.
/// - `sb_area` and `sb_state` — both `Some` together (overflow + room) or
///   both `None` (fits or area too narrow).
pub fn scrollbar_layout_for_view(
    area: Rect,
    total_lines: usize,
    scroll: u16,
) -> (Rect, Option<Rect>, Option<ScrollbarState>) {
    let sb_state = scrollbar_state_for_view(total_lines, scroll, area.height);
    let (body, sb_area) = split_body_for_scrollbar(area, sb_state.is_some());
    (body, sb_area, sb_state)
}

/// Render the right-edge scrollbar into `sb_area` if both the area and the
/// state were produced by [`scrollbar_layout_for_view`]. Either both are
/// `Some` together or both are `None`; in the latter case this is a no-op.
pub fn render_view_scrollbar(
    frame: &mut Frame<'_>,
    sb_state: Option<&mut ScrollbarState>,
    sb_area: Option<Rect>,
) {
    if let (Some(state), Some(area)) = (sb_state, sb_area) {
        frame.render_stateful_widget(view_scrollbar(), area, state);
    }
}

/// Maps a [`Severity`] variant to the terminal [`Color`] used across all
/// review surfaces (composer overlay, stale screen, overview, send-to-claude).
pub fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Required => Color::Red,
        Severity::Suggestion => Color::Yellow,
        Severity::Note => Color::DarkGray,
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::scrollbar_overflow_for_view;

    #[test]
    fn viewport_rows_zero_returns_none() {
        assert_eq!(scrollbar_overflow_for_view(100, 0, 0), None);
    }

    #[test]
    fn content_fits_exactly_returns_none() {
        assert_eq!(scrollbar_overflow_for_view(10, 0, 10), None);
    }

    #[test]
    fn content_shorter_than_viewport_returns_none() {
        assert_eq!(scrollbar_overflow_for_view(5, 0, 10), None);
    }

    #[test]
    fn one_line_overflow_returns_some() {
        let result = scrollbar_overflow_for_view(11, 0, 10);
        assert!(result.is_some(), "one overflow line must produce Some");
        let (content_length, position) = result.unwrap();
        assert_eq!(content_length, 2, "max_scroll=1, so content_length=2");
        assert_eq!(position, 0);
    }

    #[test]
    fn scroll_beyond_max_is_clamped() {
        // total=20, viewport=10 => max_scroll=10; passing scroll=99 should clamp to 10.
        let (content_length, position) = scrollbar_overflow_for_view(20, 99, 10).unwrap();
        assert_eq!(position, 10, "scroll must be clamped to max_scroll");
        assert_eq!(content_length, 11);
    }

    #[test]
    fn scrollbar_overflow_for_view_zero_lines_returns_none() {
        assert_eq!(scrollbar_overflow_for_view(0, 0, 10), None);
    }
}

#[cfg(test)]
pub mod scrollbar_test_helpers {
    /// Whether `col` of `buf` contains any glyph drawn by ratatui's
    /// `Scrollbar` widget (the begin/end arrows, the thumb, or the track).
    pub fn col_contains_scrollbar_glyph(buf: &ratatui::buffer::Buffer, col: u16) -> bool {
        (0..buf.area.height).any(|row| {
            matches!(
                buf[(col, row)].symbol(),
                "\u{25b2}" | "\u{25bc}" | "\u{2588}" | "\u{2551}"
            )
        })
    }

    /// Find the row of the topmost thumb (`█`) glyph in `col` of `buf`.
    pub fn scrollbar_thumb_row(buf: &ratatui::buffer::Buffer, col: u16) -> Option<u16> {
        (0..buf.area.height).find(|&row| buf[(col, row)].symbol() == "\u{2588}")
    }
}

/// The behavioural seams that distinguish review tools inside the shared TUI.
/// Each tool implements this trait once; the core `App<S>` is parameterised
/// over it. Only add methods here when the behaviour must differ across tools.
pub trait ReviewSurface: Sized {
    /// Error type returned by fallible surface operations.
    type Error: core::error::Error + Send + Sync + 'static;

    // ------------------------------------------------------------------
    // Stack / entry enumeration
    // ------------------------------------------------------------------

    /// Total number of entries in this review session (PR commits or jj
    /// changes). Must be ≥ 1; the core asserts this at construction time.
    fn entry_count(&self) -> usize;

    /// 0-based index of the currently loaded entry.
    fn current_entry_index(&self) -> usize;

    /// Short display identifier for entry `idx` — used in the stack bar.
    /// Returns an empty string for out-of-range indices.
    fn entry_id_display(&self, idx: usize) -> String;

    /// One-line description (commit message first line, PR commit subject) for
    /// entry `idx`. Returns an empty string for out-of-range indices.
    fn entry_description(&self, idx: usize) -> String;

    // ------------------------------------------------------------------
    // Diff retrieval
    // ------------------------------------------------------------------

    /// Build the rendered diff views for the entry at the given index.
    ///
    /// View 0 is always the synthetic description/cover view. Views 1..N map
    /// to the per-file diff views in their natural order.
    ///
    /// Returning an error causes the caller to surface a status message and
    /// leave the current views in place rather than replacing them.
    fn fetch_views(&mut self, idx: usize) -> Result<Vec<DiffView>, Self::Error>;

    // ------------------------------------------------------------------
    // Existing-state context (inline threads, GitHub review comments)
    // ------------------------------------------------------------------

    /// Return the inline comments to inject into view `view_idx` of the entry
    /// currently loaded in the core `App`. The core calls this after loading a
    /// new entry and after every save/delete operation.
    ///
    /// The severity filter, if any, must be applied by the surface so the core
    /// does not need to know the comment type.
    fn inline_comments_for_view(
        &self,
        now: std::time::SystemTime,
        view_idx: usize,
        severity_filter: Option<Severity>,
    ) -> Vec<InlineComment>;

    /// Return comments to append to the END of view `view_idx`, outside any
    /// hunk context. The default implementation returns an empty slice.
    ///
    /// Used by jjr for change-scope comments that appear at the bottom of the
    /// description view rather than anchored to a specific line.
    fn appended_comments_for_view(
        &self,
        _view_idx: usize,
        _severity_filter: Option<Severity>,
    ) -> Vec<InlineComment> {
        Vec::new()
    }

    // ------------------------------------------------------------------
    // Comment persistence (composer hooks)
    // ------------------------------------------------------------------

    /// Invoked when the user presses `^X` in the composer to save a new
    /// comment. The surface persists the comment and updates its internal
    /// state. The core will call [`inline_comments_for_view`] on the next
    /// redraw to re-inject the new comment.
    ///
    /// [`inline_comments_for_view`]: ReviewSurface::inline_comments_for_view
    fn save_comment(&mut self, req: SaveRequest<'_>) -> Result<SaveOutcome, Self::Error>;

    /// Invoked when the user presses `^X` in edit mode. The surface updates
    /// the existing record.
    fn update_comment(&mut self, req: UpdateRequest<'_>) -> Result<SaveOutcome, Self::Error>;

    /// Invoked when the user presses `^D` in edit mode. The surface deletes
    /// the record keyed by `identity`.
    fn delete_comment(&mut self, req: DeleteRequest) -> Result<DeleteOutcome, Self::Error>;

    // ------------------------------------------------------------------
    // Reviewed-bit tracking
    // ------------------------------------------------------------------

    /// Return `true` when view `view_idx` of the currently loaded entry has
    /// been marked reviewed.
    fn is_view_reviewed(&self, view_idx: usize) -> bool;

    /// Persist the reviewed bit for view `view_idx`. Called automatically
    /// when the user lands on a new view; also called manually via `U`.
    ///
    /// Returns the outcome so the caller can surface a status message when
    /// the entry was reset due to a commit amendment.
    fn mark_view_reviewed(&mut self, view_idx: usize) -> MarkReviewedOutcome;

    /// Toggle the reviewed bit for view `view_idx`. Used by the `U` keybind.
    ///
    /// Returns a [`ReviewedOutcome`] so the caller can surface the appropriate
    /// toast, including the case where the reviewed state was reset due to a
    /// commit mismatch before being re-marked.
    fn toggle_view_reviewed(&mut self, view_idx: usize) -> ReviewedOutcome;

    // ------------------------------------------------------------------
    // Severity histogram (used by transition modal and stack overview)
    // ------------------------------------------------------------------

    /// Count active (non-stale, non-orphaned) comments for the currently
    /// loaded entry, grouped by severity. Used by the between-change
    /// transition modal and the stack overview's right-edge dot column.
    fn severity_histogram(&self) -> SeverityHistogram;

    // ------------------------------------------------------------------
    // Extra-screen hooks
    // ------------------------------------------------------------------

    /// Handle a key not consumed by the core event loop from `Screen::Main`.
    ///
    /// Returning `ExtraKeyAction::OpenScreen` pushes an opaque extra screen
    /// managed by the surface. The core does not know the screen's type; it
    /// just stores `Box<dyn ExtraScreen>` and dispatches to the surface's
    /// `render_extra_screen` / `handle_extra_screen_key` methods.
    ///
    /// `file_index` and `line_index` are the cursor position at call time.
    /// `current_view` is the annotated `DiffView` for `file_index` (with
    /// inline comments already injected). Surfaces that open a composer from
    /// this method use these to build a `LineTarget`.
    fn handle_extra_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        file_index: usize,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> Result<ExtraKeyAction, Self::Error>;

    /// Render the extra screen. Called when `app.screen` is `Screen::Extra`.
    fn render_extra_screen(&self, frame: &mut Frame<'_>, state: &mut dyn ExtraScreen);

    /// Handle a key while the extra screen is open.
    ///
    /// Returns an [`ExtraScreenAction`] to tell the core what to do next:
    /// - `StayOpen` — keep the current screen open.
    /// - `Close` — close the screen and return to `Screen::Main`.
    /// - `OpenScreen(state)` — replace the current screen with a new one.
    ///
    /// The `ctx` bundle gives mutable access to the core `App` fields the
    /// surface may need to update (status message, last severity, etc.).
    fn handle_extra_screen_key(
        &mut self,
        state: &mut dyn ExtraScreen,
        key: crossterm::event::KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> Result<ExtraScreenAction, Self::Error>;

    /// Return the file-picker entries for the currently loaded entry.
    ///
    /// Called by the core when the user presses `f`. The surface builds the
    /// list from its loaded diff + comment state.
    fn file_picker_entries(&self) -> Vec<FilePickerEntry>;

    /// Title shown in the `?` help screen. Each surface supplies its own
    /// tool name so the generic core does not hard-code "jjr".
    fn help_screen_title(&self) -> &'static str;

    /// Body text shown in the `?` help screen. Each surface supplies its own
    /// keybinding reference so the help is accurate for the tool in use.
    fn help_screen_body(&self) -> &'static str;

    /// One-line keybinding hint shown in the main-view footer when no status
    /// message is active. Each surface supplies its own hint so the footer
    /// reflects the tool's actual keys.
    fn footer_hint(&self, width: u16, has_stack: bool, severity_filter: Option<Severity>)
        -> String;
}

/// Opaque extra-screen state owned by the core `App` but whose type is only
/// known to the surface implementation.
///
/// The `as_any` / `as_any_mut` methods allow the surface's
/// `render_extra_screen` and `handle_extra_screen_key` implementations to
/// downcast `&dyn ExtraScreen` to a concrete wrapper type.
pub trait ExtraScreen: Send + 'static {
    /// When `true` the core renders the main diff view first, then calls
    /// [`ReviewSurface::render_extra_screen`] to draw the overlay on top.
    /// When `false` (the default) the extra screen replaces the main view
    /// entirely.
    fn is_overlay(&self) -> bool {
        false
    }

    /// Upcast to `&dyn Any` for downcasting to the concrete wrapper type.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Upcast to `&mut dyn Any` for downcasting to the concrete wrapper type.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Action returned by [`ReviewSurface::handle_extra_screen_key`].
///
/// Richer than a plain `bool` so the surface can close its current screen and
/// immediately open a different one (e.g. overview → composer) in a single
/// round-trip.
pub enum ExtraScreenAction {
    /// Keep the current extra screen open.
    StayOpen,
    /// Close the extra screen and return to `Screen::Main`.
    Close,
    /// Close the current extra screen and open `state` in its place.
    OpenScreen(Box<dyn ExtraScreen>),
}

/// Action returned by [`ReviewSurface::handle_extra_key`].
pub enum ExtraKeyAction {
    /// Key was not consumed or had no effect.
    Ignored,
    /// Open an extra screen with the given state.
    OpenScreen(Box<dyn ExtraScreen>),
    /// Surface a status message on the main screen.
    StatusMessage(String),
    /// Refresh inline comment views (rebuild `annotated_per_file`) and show a
    /// status message. Used after in-place mutations like direct comment delete
    /// that update the surface's data without going through the composer flow.
    RefreshAndStatus(String),
    /// Quit the application.
    Quit,
}

/// Opaque identity key for a comment record.
///
/// Wraps the `created_at` timestamp that serves as the comment's primary key.
/// Using a newtype prevents accidental interchange with arbitrary
/// `OffsetDateTime` values and makes intent explicit at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentId(time::OffsetDateTime);

impl CommentId {
    /// Construct a `CommentId` from an `OffsetDateTime`.
    pub fn new(t: time::OffsetDateTime) -> Self {
        Self(t)
    }

    /// Extract the underlying `OffsetDateTime`.
    pub fn as_offset_date_time(self) -> time::OffsetDateTime {
        self.0
    }
}

/// Request bundle passed to [`ReviewSurface::save_comment`].
pub struct SaveRequest<'a> {
    /// Scope of the comment (line, change, stack, description).
    pub scope: &'a composer::ComposerScope,
    /// Severity chosen by the reviewer.
    pub severity: Severity,
    /// Body text (already trimmed of leading/trailing whitespace by the core).
    pub body: &'a str,
    /// Current entry index (0-based) at save time.
    pub entry_idx: usize,
}

/// Request bundle passed to [`ReviewSurface::update_comment`].
pub struct UpdateRequest<'a> {
    /// Opaque key identifying the comment record to update.
    pub identity: CommentId,
    /// New body text.
    pub body: &'a str,
    /// New severity.
    pub severity: Severity,
    /// Whether the body exceeded the size cap (so the surface can warn).
    pub oversized: bool,
}

/// Request bundle passed to [`ReviewSurface::delete_comment`].
pub struct DeleteRequest {
    /// Opaque key identifying the comment record to delete.
    pub identity: CommentId,
    /// The `comment_index` carried by the focused `RenderedLineKind::InlineCommentMeta`
    /// row, so the surface can look the record up in its loaded list. `None`
    /// when the composer was opened outside the main-view inline list (e.g.
    /// the stack overview), where there is no `InlineCommentMeta` row to carry
    /// the index.
    pub comment_index: Option<usize>,
}

impl DeleteRequest {
    #[must_use]
    pub fn new(identity: CommentId, comment_index: Option<usize>) -> Self {
        Self {
            identity,
            comment_index,
        }
    }
}

/// Outcome reported after a delete operation.
#[derive(Debug)]
pub enum DeleteOutcome {
    /// The comment was successfully deleted.
    Deleted,
    /// The surface declined to delete the comment (e.g. read-only surface).
    Refused { reason: String },
}

/// Outcome reported after a save or update operation.
#[derive(Debug)]
pub enum SaveOutcome {
    /// Comment was persisted successfully.
    Saved { status_message: String },
    /// Save was refused (e.g. empty body). Composer stays open.
    Refused { reason: String },
    /// Persistence failed. Composer stays open so the reviewer can retry.
    Errored { message: String },
}

/// Result of marking a view reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkReviewedOutcome {
    /// Normal mark — no prior reviewed entry or the commit matches.
    NoReset,
    /// The stored commit id no longer matched (change was amended/rebased).
    /// The previous reviewed bits were cleared.
    ResetDueToCommitMismatch,
    /// This surface does not implement reviewed tracking; the mark is a no-op.
    NotTracked,
}

/// Outcome returned by [`ReviewSurface::toggle_view_reviewed`].
///
/// Carries enough information for the core to surface the right status message:
/// normal mark/unmark vs. the case where the reviewed state was reset first.
/// Surfaces that do not implement reviewed tracking return [`ReviewedOutcome::NotTracked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewedOutcome {
    /// The view was not previously reviewed; it is now marked reviewed.
    Marked,
    /// The view was previously reviewed; it is now marked unreviewed.
    Unmarked,
    /// The stored commit id no longer matched (change was amended/rebased);
    /// the prior reviewed bits were cleared and the view was then marked reviewed.
    ResetAndMarked,
    /// This surface does not implement reviewed tracking; the toggle is a no-op.
    NotTracked,
}

/// Comment counts by severity for the currently loaded entry.
///
/// Stale and orphaned comments are excluded — they live in the stale view, not
/// the active-comment counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SeverityHistogram {
    pub required: usize,
    pub suggestion: usize,
    pub note: usize,
}

impl SeverityHistogram {
    #[must_use]
    pub fn total(self) -> usize {
        self.required + self.suggestion + self.note
    }
}

// ── Shared ExtraScreen helpers ────────────────────────────────────────────────

/// Downcast `s` to `&mut T` through the `as_any_mut` escape hatch.
///
/// Both `jjr` and `ggr` use identical downcast patterns for their composer and
/// other extra screens; centralising the helper removes the duplicate.
pub fn try_downcast_mut<T: 'static>(s: &mut dyn ExtraScreen) -> Option<&mut T> {
    s.as_any_mut().downcast_mut::<T>()
}

/// Wraps `Box<Composer>` so it can be stored as `Box<dyn ExtraScreen>`.
///
/// Both `jjr` and `ggr` open a `Composer` overlay for comment entry; a shared
/// wrapper avoids duplicating the struct and its `ExtraScreen` impl in both
/// crates.
pub struct ComposerScreen(pub Box<composer::Composer>);

impl ExtraScreen for ComposerScreen {
    fn is_overlay(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
