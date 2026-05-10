//! Generic TUI application state and event loop.
//!
//! `App<S>` is parameterised over a [`ReviewSurface`] that supplies the
//! tool-specific behaviour (diff loading, comment persistence, extra screens).
//! All rendering and input handling that is identical across `jjr` and `ggr`
//! lives here.
//!
//! ## Module layout rule
//! Declared as `tui.rs` + `tui/app.rs` (no `mod.rs`) per the workspace's
//! `mod_module_files = "deny"` / `self_named_module_files = "allow"` policy.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::severity::Severity;
use crate::tui::{
    composer_overlay, file_picker, help_screen, render_view_scrollbar, scrollbar_layout_for_view,
    severity_color, DiffView, ExtraKeyAction, ExtraScreen, ExtraScreenAction, FilePickerState,
    MarkReviewedOutcome, PairedRow, RenderedLine, RenderedLineKind, ReviewSurface, ReviewedOutcome,
    SeverityHistogram,
};

// ---------------------------------------------------------------------------
// Constants shared by all surfaces
// ---------------------------------------------------------------------------

/// Minimum terminal columns required to start the TUI. Re-exported so
/// surfaces can call `crossterm::terminal::size()` before entering.
pub const MIN_COLS: u16 = 60;

/// Minimum terminal rows required to start the TUI.
pub const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block.
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render.
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

/// Stack depth at which `transition_screen = "auto"` starts firing.
const AUTO_TRANSITION_THRESHOLD: usize = 8;

/// Width (cells) of the graphical fill in the stack progress bar.
const STACK_PROGRESS_BAR_WIDTH: u16 = 20;

/// Below this column count, the stack bar drops the graphical fill.
const STACK_BAR_MIN_COLS_FOR_FILL: u16 = 80;

/// Width (cells) of the transition modal.
const TRANSITION_MODAL_WIDTH: u16 = 42;

/// Height (rows) of the transition modal.
const TRANSITION_MODAL_HEIGHT: u16 = 18;

/// Description budget (chars) inside the transition modal.
const TRANSITION_DESC_BUDGET: usize = 36;

/// Maximum number of `●` dots rendered before truncating with `…`.
pub const DOT_BUDGET: usize = 5;

const STATUS_AT_LAST_FILE: &str = "already at the last file";
const STATUS_AT_FIRST_FILE: &str = "already at the first file";
const STATUS_ONLY_ONE_FILE: &str = "only one file";
const REVIEWED_TITLE_GLYPH: &str = "\u{2713}";
const STATUS_REVIEWED_RESET: &str = "change amended; reviewed state reset";
const STATUS_MARKED_REVIEWED: &str = "file marked as reviewed";
const STATUS_MARKED_UNREVIEWED: &str = "file marked as unreviewed";
const STATUS_RESET_AND_MARKED_REVIEWED: &str = "reviewed state reset; marked reviewed";

/// Wrapper type for the base (un-annotated) diff views held in
/// [`ExtraScreenContext::base_per_file`].
///
/// Distinct from `Vec<DiffView>` so callers cannot accidentally pass a
/// pre-annotated view list — the newtype makes the invariant structural rather
/// than doc-only.
pub struct BaseViews(pub Vec<DiffView>);

/// Footer hint for the transition modal.
const TRANSITION_FOOTER_TEXT: &str = "  Enter  p prev  Esc cancel  q quit";

// ---------------------------------------------------------------------------
// Diff layout types
// ---------------------------------------------------------------------------

/// User preference for unified vs side-by-side diff layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Auto,
    ForceUnified,
    ForceSideBySide,
}

/// Resolved layout for a single render pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveDiffMode {
    Unified,
    SideBySide,
}

/// Minimum body width at which `DiffMode::Auto` switches to side-by-side.
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;

/// Width of the divider between left and right columns in side-by-side mode.
pub const SIDE_BY_SIDE_GUTTER_WIDTH: u16 = 3;

/// Minimum cells per side cell in side-by-side mode.
pub const MIN_USEFUL_SIDE_CELL_WIDTH: u16 = 4;

/// Below this body width side-by-side falls back to unified at draw time.
pub const MIN_USEFUL_SIDE_BY_SIDE_WIDTH: u16 =
    SIDE_BY_SIDE_GUTTER_WIDTH + 2 * MIN_USEFUL_SIDE_CELL_WIDTH;

/// Resolve the user's `DiffMode` preference into the layout for this render pass.
pub fn resolve_diff_mode(pref: DiffMode, body_width: u16, file_index: usize) -> EffectiveDiffMode {
    if file_index == 0 {
        return EffectiveDiffMode::Unified;
    }
    match pref {
        DiffMode::Auto => {
            if body_width >= SIDE_BY_SIDE_MIN_WIDTH {
                EffectiveDiffMode::SideBySide
            } else {
                EffectiveDiffMode::Unified
            }
        }
        DiffMode::ForceUnified => EffectiveDiffMode::Unified,
        DiffMode::ForceSideBySide => EffectiveDiffMode::SideBySide,
    }
}

// ---------------------------------------------------------------------------
// Transition state
// ---------------------------------------------------------------------------

/// Which transition behavior is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionMode {
    Never,
    Auto,
    Always,
}

/// State for the transition screen shown between changes in stack mode.
pub struct TransitionState {
    /// Index of the change just reviewed.
    pub reviewed_index: usize,
    /// Index of the next change to open.
    pub next_index: usize,
    /// Comment count for the reviewed change.
    pub reviewed_comment_count: Option<usize>,
    /// Severity histogram for the reviewed change.
    pub severity_histogram: SeverityHistogram,
}

// ---------------------------------------------------------------------------
// Screen enum
// ---------------------------------------------------------------------------

/// All screens the generic app can be in. Tool-specific overlays live behind
/// `Extra(Box<dyn ExtraScreen>)`.
pub enum Screen {
    Main,
    Help,
    /// Between-change transition beat shown in stack mode.
    Transition(TransitionState),
    /// Tool-specific overlay (composer, stale, overview, send-to-claude, etc.).
    Extra(Box<dyn ExtraScreen>),
    /// File picker modal.
    FilePicker(FilePickerState),
}

// ---------------------------------------------------------------------------
// App struct
// ---------------------------------------------------------------------------

/// Generic TUI application state parameterised over a [`ReviewSurfaceExt`].
pub struct App<S: ReviewSurfaceExt> {
    pub surface: S,
    /// Base rendered views (no inline comments); rebuilt on entry load.
    pub(crate) rendered_per_file: Vec<DiffView>,
    /// Base views passed back by the surface via `ExtraScreenContext::base_per_file`
    /// after out-of-band reloads (e.g. post-claude). The core calls
    /// `refresh_inline_comments` immediately after and annotates these before display.
    pub(crate) annotated_per_file: BaseViews,
    pub(crate) file_index: usize,
    pub(crate) line_index: usize,
    pub(crate) scroll: u16,
    pub(crate) screen: Screen,
    pub(crate) should_quit: bool,
    /// Cached viewport height (set during `render_main`, read in key handler).
    pub(crate) viewport_rows: u16,
    /// Cached diff body width (set during `render_diff`).
    pub(crate) diff_body_width: u16,
    /// Severity chosen in the last save.
    pub(crate) last_severity: Option<Severity>,
    /// One-line status message shown at the bottom of the main view.
    pub(crate) status_message: Option<String>,
    /// Transition screen behavior.
    pub(crate) transition_mode: TransitionMode,
    /// Active severity filter for inline comment display.
    pub(crate) severity_filter: Option<Severity>,
    /// Session-scoped diff layout preference.
    pub(crate) diff_mode: DiffMode,
    /// Set when the alternate screen was re-entered out-of-band.
    pub(crate) needs_full_redraw: bool,
}

impl<S: ReviewSurfaceExt> App<S> {
    /// Read-only access to the current cursor row.
    pub fn line_index(&self) -> usize {
        self.line_index
    }

    /// Read-only access to the current file index.
    pub fn file_index(&self) -> usize {
        self.file_index
    }

    /// Read-only access to the current status message.
    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    /// Read-only access to the current screen.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }
}

impl<S: ReviewSurfaceExt> App<S> {
    /// Construct a new `App` with the given surface and initial views.
    pub fn new(
        surface: S,
        rendered_per_file: Vec<DiffView>,
        transition_mode: TransitionMode,
    ) -> Self {
        let annotated_per_file = BaseViews(rendered_per_file.clone());
        Self {
            surface,
            rendered_per_file,
            annotated_per_file,
            file_index: 0,
            line_index: 0,
            scroll: 0,
            screen: Screen::Main,
            should_quit: false,
            viewport_rows: FALLBACK_VIEWPORT_ROWS,
            diff_body_width: 0,
            last_severity: None,
            status_message: None,
            transition_mode,
            severity_filter: None,
            diff_mode: DiffMode::Auto,
            needs_full_redraw: false,
        }
    }

    pub fn current_view(&self) -> Option<&DiffView> {
        self.annotated_per_file.0.get(self.file_index)
    }

    /// Resolve the effective diff layout for the current view + cached body width.
    pub fn effective_diff_mode(&self) -> EffectiveDiffMode {
        resolve_diff_mode(self.diff_mode, self.diff_body_width, self.file_index)
    }

    /// Cycle the diff layout preference.
    pub fn cycle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Auto => DiffMode::ForceUnified,
            DiffMode::ForceUnified => DiffMode::ForceSideBySide,
            DiffMode::ForceSideBySide => DiffMode::Auto,
        };
        self.line_index = 0;
        self.scroll = 0;
        self.status_message = Some(
            match self.diff_mode {
                DiffMode::Auto => "diff layout: auto",
                DiffMode::ForceUnified => "diff layout: unified",
                DiffMode::ForceSideBySide => "diff layout: side-by-side",
            }
            .to_owned(),
        );
    }

    /// Number of rows in the active layout.
    pub fn current_row_count(&self) -> usize {
        let Some(view) = self.current_view() else {
            return 0;
        };
        match self.effective_diff_mode() {
            EffectiveDiffMode::Unified => view.lines.len(),
            EffectiveDiffMode::SideBySide => view.paired_rows.len(),
        }
    }

    /// Whether the row at `row_idx` is non-navigable.
    pub fn is_skip_row(&self, row_idx: usize) -> bool {
        let Some(view) = self.current_view() else {
            return false;
        };
        match self.effective_diff_mode() {
            EffectiveDiffMode::Unified => view.lines.get(row_idx).is_some_and(|l| {
                matches!(
                    l.kind,
                    RenderedLineKind::HunkSeparator | RenderedLineKind::InlineCommentBody,
                )
            }),
            EffectiveDiffMode::SideBySide => {
                let Some(row) = view.paired_rows.get(row_idx) else {
                    return false;
                };
                match row {
                    PairedRow::Spanning(idx) => view.lines.get(*idx).is_some_and(|l| {
                        matches!(
                            l.kind,
                            RenderedLineKind::HunkSeparator | RenderedLineKind::InlineCommentBody,
                        )
                    }),
                    PairedRow::Pair { .. } => false,
                }
            }
        }
    }

    /// Move cursor by `delta` rows, skipping non-navigable rows.
    pub fn move_line(&mut self, delta: isize) {
        let count = self.current_row_count();
        if count == 0 {
            return;
        }
        let max_index = count - 1;
        let mut next = clamp_with_delta(self.line_index, delta, max_index);
        let step: isize = if delta >= 0 { 1 } else { -1 };
        while next > 0 && next < max_index && self.is_skip_row(next) {
            next = clamp_with_delta(next, step, max_index);
        }
        // The loop above stops at a boundary (0 or max_index) regardless of
        // row kind. If the boundary row is a skip row, scan in the opposite
        // direction for the nearest navigable row. If every row is a skip row,
        // leave the cursor at its current position.
        if self.is_skip_row(next) {
            let reverse = -step;
            let mut candidate = next;
            loop {
                let prev = candidate;
                candidate = clamp_with_delta(candidate, reverse, max_index);
                if candidate == prev {
                    // Reached the opposite boundary with no navigable row.
                    return;
                }
                if !self.is_skip_row(candidate) {
                    next = candidate;
                    break;
                }
            }
        }
        self.line_index = next;
    }

    /// Move cursor by a full page.
    pub fn move_page(&mut self, delta: isize) {
        let step = page_size(self.viewport_rows);
        let signed_step: isize = isize::try_from(step).unwrap_or(isize::MAX);
        self.move_line(delta.saturating_mul(signed_step));
    }

    /// Jump to top or bottom of current view.
    ///
    /// After setting the raw index, `move_line` is called with a directional
    /// nudge (forward for Top, backward for Bottom) so the cursor skips any
    /// non-navigable row (e.g. a `HunkSeparator`) sitting at the boundary.
    pub fn jump_to(&mut self, end: Edge) {
        let count = self.current_row_count();
        if count == 0 {
            return;
        }
        self.line_index = match end {
            Edge::Top => 0,
            Edge::Bottom => count - 1,
        };
        // Nudge in the natural direction so the cursor is not left stranded on
        // a skip row at the boundary.  move_line handles the case where no
        // navigable row exists by leaving the cursor in place.
        let nudge = match end {
            Edge::Top => 1,
            Edge::Bottom => -1,
        };
        if self.is_skip_row(self.line_index) {
            self.move_line(nudge);
        }
    }

    /// Cycle to the next/previous file view.
    pub fn cycle_file(&mut self, delta: isize) {
        let count = self.rendered_per_file.len();
        if count == 0 {
            return;
        }
        if count == 1 {
            self.status_message = Some(STATUS_ONLY_ONE_FILE.to_owned());
            self.line_index = 0;
            self.scroll = 0;
            self.mark_current_file_reviewed();
            return;
        }
        let max_index = count - 1;
        let previous_index = self.file_index;
        let new_index = clamp_with_delta(previous_index, delta, max_index);
        if new_index == previous_index {
            if delta > 0 {
                self.status_message = Some(STATUS_AT_LAST_FILE.to_owned());
            } else if delta < 0 {
                self.status_message = Some(STATUS_AT_FIRST_FILE.to_owned());
            }
        }
        self.file_index = new_index;
        self.line_index = 0;
        self.scroll = 0;
        self.mark_current_file_reviewed();
    }

    /// Ensure the cursor is visible in the viewport.
    pub fn ensure_cursor_visible(&mut self, viewport_rows: u16) {
        let line_index_u16 = u16::try_from(self.line_index).unwrap_or(u16::MAX);
        if line_index_u16 < self.scroll {
            self.scroll = line_index_u16;
        }
        let last_visible = self.scroll.saturating_add(viewport_rows.saturating_sub(1));
        if line_index_u16 > last_visible {
            self.scroll = line_index_u16.saturating_sub(viewport_rows.saturating_sub(1));
        }
    }

    /// Whether a transition should fire for the given stack length.
    pub fn transition_enabled(&self, stack_len: usize) -> bool {
        match self.transition_mode {
            TransitionMode::Never => false,
            TransitionMode::Always => true,
            TransitionMode::Auto => stack_len >= AUTO_TRANSITION_THRESHOLD,
        }
    }

    /// Auto-mark the active view as reviewed.
    pub fn mark_current_file_reviewed(&mut self) {
        let outcome = self.surface.mark_view_reviewed(self.file_index);
        match outcome {
            MarkReviewedOutcome::ResetDueToCommitMismatch if self.status_message.is_none() => {
                self.status_message = Some(STATUS_REVIEWED_RESET.to_owned());
            }
            MarkReviewedOutcome::ResetDueToCommitMismatch
            | MarkReviewedOutcome::NoReset
            | MarkReviewedOutcome::NotTracked => {}
        }
    }

    /// Toggle the reviewed bit for the active view.
    pub fn toggle_current_file_reviewed(&mut self) {
        let outcome = self.surface.toggle_view_reviewed(self.file_index);
        let msg = match outcome {
            ReviewedOutcome::Marked => Some(STATUS_MARKED_REVIEWED),
            ReviewedOutcome::Unmarked => Some(STATUS_MARKED_UNREVIEWED),
            ReviewedOutcome::ResetAndMarked => Some(STATUS_RESET_AND_MARKED_REVIEWED),
            ReviewedOutcome::NotTracked => None,
        };
        if let Some(m) = msg {
            self.status_message = Some(m.to_owned());
        }
    }

    /// Whether the current view at `file_index` is reviewed.
    pub fn is_view_reviewed(&self, file_index: usize) -> bool {
        self.surface.is_view_reviewed(file_index)
    }

    /// Rebuild annotated views by re-injecting inline comments from the surface.
    pub fn refresh_inline_comments(&mut self) {
        let now = std::time::SystemTime::now();
        self.annotated_per_file = BaseViews(
            self.rendered_per_file
                .iter()
                .enumerate()
                .map(|(view_idx, base_view)| {
                    let inline =
                        self.surface
                            .inline_comments_for_view(now, view_idx, self.severity_filter);
                    let appended = self
                        .surface
                        .appended_comments_for_view(view_idx, self.severity_filter);
                    base_view
                        .clone()
                        .with_inline_comments(&inline)
                        .with_change_comments_appended(&appended)
                })
                .collect(),
        );
    }

    /// Reload views for the currently loaded entry from the surface.
    pub fn reload_current_entry(&mut self) -> Result<(), S::Error> {
        let idx = self.surface.current_entry_index();
        let views = self.surface.fetch_views(idx)?;
        self.rendered_per_file = views;
        self.annotated_per_file = BaseViews(self.rendered_per_file.clone());
        Ok(())
    }

    /// Load the entry at `idx` in the stack (surface-driven).
    pub fn load_entry(&mut self, idx: usize, record_cursor: bool) -> Result<(), S::Error> {
        let views = self.surface.fetch_views(idx)?;
        self.rendered_per_file = views;
        self.annotated_per_file = BaseViews(self.rendered_per_file.clone());
        self.file_index = 0;
        self.line_index = 0;
        self.scroll = 0;
        self.status_message = self.surface.take_pending_status_message();
        self.refresh_inline_comments();
        self.mark_current_file_reviewed();
        self.surface.on_entry_loaded(idx, record_cursor);
        Ok(())
    }

    /// Advance to the next entry in stack mode.
    pub fn advance_stack(&mut self) -> Result<(), S::Error> {
        let current = self.surface.current_entry_index();
        let count = self.surface.entry_count();
        let next_index = current + 1;
        if next_index >= count {
            self.status_message = Some("already at the last change".to_owned());
            return Ok(());
        }
        if self.transition_enabled(count) {
            let (reviewed_comment_count, severity_histogram) =
                self.surface.severity_histogram_for_transition();
            self.screen = Screen::Transition(TransitionState {
                reviewed_index: current,
                next_index,
                reviewed_comment_count,
                severity_histogram,
            });
            return Ok(());
        }
        self.load_entry(next_index, true)
    }

    /// Retreat to the previous entry in stack mode.
    pub fn retreat_stack(&mut self) -> Result<(), S::Error> {
        let current = self.surface.current_entry_index();
        let count = self.surface.entry_count();
        if count == 1 {
            self.status_message =
                Some("single-change view — run with --stack to walk a stack".to_owned());
            return Ok(());
        }
        if current == 0 {
            self.status_message = Some("already at the first change".to_owned());
            return Ok(());
        }
        self.load_entry(current - 1, false)
    }

    /// Navigate directly to entry `idx` in stack mode (no transition).
    pub fn goto_entry(&mut self, idx: usize) -> Result<(), S::Error> {
        let count = self.surface.entry_count();
        if idx >= count {
            return Ok(());
        }
        self.load_entry(idx, true)
    }

    /// Toggle severity filter for a given severity.
    pub fn toggle_severity_filter(&mut self, severity: Severity) {
        if self.severity_filter == Some(severity) {
            self.severity_filter = None;
        } else {
            self.severity_filter = Some(severity);
        }
        self.refresh_inline_comments();
    }
}

// ---------------------------------------------------------------------------
// Edge enum (used by jump_to)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Edge {
    Top,
    Bottom,
}

use crate::util::{clamp_with_delta, page_size, pluralize, truncate};

/// Render a string of `●` dots for a single count.
pub fn render_dots(count: usize) -> String {
    if count == 0 {
        String::new()
    } else if count <= DOT_BUDGET {
        "●".repeat(count)
    } else {
        format!("{}…", "●".repeat(DOT_BUDGET))
    }
}

/// Render mixed-severity dots sharing a single `DOT_BUDGET`.
pub fn render_dots_mixed(hist: SeverityHistogram) -> String {
    let mut dots = String::new();
    let mut budget = DOT_BUDGET;
    for count in [hist.required, hist.suggestion, hist.note] {
        let take = count.min(budget);
        for _ in 0..take {
            dots.push('●');
        }
        budget = budget.saturating_sub(take);
        if budget == 0 {
            break;
        }
    }
    if hist.total() > DOT_BUDGET {
        dots.push('…');
    }
    dots
}

// ---------------------------------------------------------------------------
// Extended ReviewSurface methods needed by App
// ---------------------------------------------------------------------------

/// Extension methods on `ReviewSurface` that the generic `App<S>` calls but
/// that do not need to be part of the public trait surface. Implemented as a
/// separate trait so downstream surfaces only implement the required items.
pub trait ReviewSurfaceExt: ReviewSurface {
    /// Called after `fetch_views` succeeds for entry `idx`. Surfaces can
    /// persist the cursor position or update their current-index here.
    fn on_entry_loaded(&mut self, idx: usize, record_cursor: bool);

    /// Return the `(reviewed_comment_count, severity_histogram)` pair used
    /// by the transition modal. `None` for the count means the load failed.
    fn severity_histogram_for_transition(&self) -> (Option<usize>, SeverityHistogram);

    /// Take a deferred status message produced during the last entry load
    /// (e.g. a reconcile-and-persist error). Returns `None` if no message is
    /// pending. The default implementation always returns `None`.
    fn take_pending_status_message(&mut self) -> Option<String> {
        None
    }

    /// Initial `(file_index, line_index)` to navigate to at startup, for
    /// surfaces that restore a saved cursor position. Called once after
    /// `reload_current_entry` succeeds, before the event loop starts.
    fn initial_view_position(&mut self) -> (usize, usize) {
        (0, 0)
    }
}

// ---------------------------------------------------------------------------
// Footer layout helpers
// ---------------------------------------------------------------------------

struct FooterSegment {
    text: &'static str,
    stack_only: bool,
}

const FOOTER_IRREDUCIBLE: &str = " \u{2191}\u{2193} line  Tab file  n/p revision  Enter comment";

const FOOTER_OPTIONAL: &[FooterSegment] = &[
    FooterSegment {
        text: "  ?",
        stack_only: false,
    },
    FooterSegment {
        text: "  C \u{2192} Claude",
        stack_only: false,
    },
    FooterSegment {
        text: "  S stale",
        stack_only: false,
    },
    FooterSegment {
        text: "  s stack",
        stack_only: true,
    },
];

/// Build the main-view footer text for the given terminal width.
pub fn footer_text_for_width(
    width: u16,
    has_stack: bool,
    severity_filter: Option<Severity>,
) -> String {
    let badge = match severity_filter {
        Some(Severity::Required) => "  [F:required]",
        Some(Severity::Suggestion) => "  [F:suggestion]",
        Some(Severity::Note) => "  [F:note]",
        None => "",
    };

    let base = FOOTER_IRREDUCIBLE;
    let candidates: Vec<&str> = FOOTER_OPTIONAL
        .iter()
        .filter(|seg| !seg.stack_only || has_stack)
        .map(|seg| seg.text)
        .collect();

    let target = usize::from(width);
    let badge_chars = badge.chars().count();

    for drop_count in 0..=candidates.len() {
        let kept = &candidates[drop_count..];
        let text: String = std::iter::once(base)
            .chain(kept.iter().copied().rev())
            .collect::<Vec<_>>()
            .concat();
        let total_chars = text.chars().count() + badge_chars;
        if total_chars <= target || drop_count == candidates.len() {
            if badge.is_empty() {
                return text;
            }
            return format!("{text}{badge}");
        }
    }

    unreachable!("loop returns at drop_count == candidates.len()")
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Run the TUI event loop until the user quits.
///
/// `on_exit` is called with a reference to `app` after the loop exits but
/// before the function returns, so surfaces can persist cursor state etc.
pub fn run_app<S, B, F>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App<S>,
    on_exit: F,
) -> Result<(), AppError<S>>
where
    S: ReviewSurfaceExt,
    B: ratatui::backend::Backend,
    B::Error: core::error::Error + Send + Sync + 'static,
    F: FnOnce(&mut App<S>),
{
    // Seed the first entry.
    if let Err(e) = app.reload_current_entry() {
        return Err(AppError::Surface(e));
    }
    app.refresh_inline_comments();
    app.mark_current_file_reviewed();

    {
        let (fi, li) = app.surface.initial_view_position();
        app.file_index = fi;
        let max_li = app
            .rendered_per_file
            .get(fi)
            .map(|v| v.lines.len().saturating_sub(1))
            .unwrap_or(0);
        app.line_index = li.min(max_li);
    }

    while !app.should_quit {
        if app.needs_full_redraw {
            terminal
                .clear()
                .map_err(|e| AppError::Io(std::io::Error::other(e)))?;
            app.needs_full_redraw = false;
        }
        terminal
            .draw(|frame| render(frame, app))
            .map_err(|e| AppError::Io(std::io::Error::other(e)))?;
        handle_event(app)?;
    }

    on_exit(app);
    Ok(())
}

/// Error type for `run_app`.
pub enum AppError<S: ReviewSurfaceExt> {
    Io(std::io::Error),
    Surface(S::Error),
}

impl<S: ReviewSurfaceExt> core::fmt::Debug for AppError<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "AppError::Io({e})"),
            Self::Surface(e) => write!(f, "AppError::Surface({e})"),
        }
    }
}

impl<S: ReviewSurfaceExt> core::fmt::Display for AppError<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Surface(e) => write!(f, "{e}"),
        }
    }
}

impl<S: ReviewSurfaceExt> core::error::Error for AppError<S>
where
    S::Error: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Surface(e) => {
                let e: &dyn core::error::Error = e;
                Some(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub(crate) fn render<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, app: &mut App<S>) {
    if matches!(app.screen, Screen::Extra(_)) {
        let Screen::Extra(state) = std::mem::replace(&mut app.screen, Screen::Main) else {
            unreachable!("matched above");
        };
        // Overlay screens render on top of the main view; full-screen extras
        // replace the main view entirely.
        if state.is_overlay() {
            render_main(frame, app);
        }
        let mut state = state;
        app.surface.render_extra_screen(frame, state.as_mut());
        app.screen = Screen::Extra(state);
        return;
    }

    render_main(frame, app);
    match &app.screen {
        Screen::Main => {}
        Screen::Help => help_screen::render(frame, app.surface.help_screen_title()),
        Screen::Transition(state) => {
            render_transition(frame, app, state);
        }
        Screen::FilePicker(state) => {
            file_picker::render(frame, state);
        }
        Screen::Extra(_) => unreachable!("handled above"),
    }
}

fn render_main<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, app: &mut App<S>) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    render_stack_bar(frame, layout[0], app);
    render_file_header(frame, layout[1], app);

    let diff_area = layout[2];
    let viewport_rows = diff_area.height;
    app.viewport_rows = viewport_rows;
    app.ensure_cursor_visible(viewport_rows);
    render_diff(frame, diff_area, app);

    render_footer(frame, layout[3], app);
}

fn render_stack_bar<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, area: Rect, app: &App<S>) {
    let count = app.surface.entry_count();
    let current = app.surface.current_entry_index();
    let (position, total) = if count > 1 {
        (current + 1, count)
    } else {
        (1, 1)
    };

    let interior_cols = area.width.saturating_sub(BLOCK_BORDER_COLS);
    let bar_segment = if area.width >= STACK_BAR_MIN_COLS_FOR_FILL && total > 0 {
        progress_bar_string(position, total, STACK_PROGRESS_BAR_WIDTH)
    } else {
        String::new()
    };

    let id_str = app.surface.entry_id_display(current);
    let text_segment = format!("{position}/{total}  {id_str}  ");
    let used_width = bar_segment.chars().count() + text_segment.chars().count();
    let desc_budget = usize::from(interior_cols).saturating_sub(used_width);
    let desc = app.surface.entry_description(current);
    let label = format!(
        "{}{}{}",
        bar_segment,
        text_segment,
        truncate(&desc, desc_budget)
    );
    let block = Block::default().borders(Borders::ALL).title("Stack");
    let widget = Paragraph::new(label).block(block);
    frame.render_widget(widget, area);
}

fn progress_bar_string(position: usize, total: usize, width: u16) -> String {
    let width_usize = usize::from(width);
    if total == 0 || width_usize == 0 {
        return String::new();
    }
    let position = position.min(total);
    let filled = (position * width_usize) / total;
    let empty = width_usize.saturating_sub(filled);
    let mut s = String::with_capacity(width_usize * 4 + 2);
    for _ in 0..filled {
        s.push('█');
    }
    for _ in 0..empty {
        s.push('░');
    }
    s.push_str("  ");
    s
}

fn render_file_header<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, area: Rect, app: &App<S>) {
    let total = app.rendered_per_file.len();
    let position = app.file_index.saturating_add(1);
    let path_label = app
        .current_view()
        .map_or_else(|| "(no files)".to_owned(), |v| v.title.clone());
    let label = format!("{path_label}  ·  {position} of {total}");
    let block = Block::default().borders(Borders::ALL).title("File");
    let line = if app.is_view_reviewed(app.file_index) {
        TuiLine::from(vec![
            Span::raw(label),
            Span::raw(" "),
            Span::styled(REVIEWED_TITLE_GLYPH, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        TuiLine::from(label)
    };
    let widget = Paragraph::new(line).block(block);
    frame.render_widget(widget, area);
}

fn render_diff<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, area: Rect, app: &mut App<S>) {
    let scroll = app.scroll;
    let line_index = app.line_index;
    let file_index = app.file_index;
    let diff_mode = app.diff_mode;

    let Some(view) = app.current_view() else {
        let widget = Paragraph::new("No files in this change.");
        frame.render_widget(widget, area);
        app.diff_body_width = area.width;
        return;
    };

    let probe_layout = scrollbar_layout_for_view(area, view.lines.len(), scroll);
    let probe_width = probe_layout.0.width;
    let mode = resolve_diff_mode(diff_mode, probe_width, file_index);

    let (body_area, scrollbar_area, mut sb_state) = match mode {
        EffectiveDiffMode::Unified => probe_layout,
        EffectiveDiffMode::SideBySide => {
            scrollbar_layout_for_view(area, view.paired_rows.len(), scroll)
        }
    };

    let body_width = body_area.width;

    match mode {
        EffectiveDiffMode::Unified => {
            let lines: Vec<TuiLine<'_>> = view
                .lines
                .iter()
                .enumerate()
                .map(|(idx, line)| render_rendered_line(line, idx == line_index, body_width))
                .collect();

            let widget = Paragraph::new(lines).scroll((scroll, 0));
            frame.render_widget(widget, body_area);
        }
        EffectiveDiffMode::SideBySide => {
            render_diff_side_by_side(frame, body_area, view, line_index, scroll);
        }
    }

    render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);
    app.diff_body_width = body_width;
}

fn render_diff_side_by_side(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &DiffView,
    cursor_row: usize,
    scroll: u16,
) {
    let total_width = area.width;
    if total_width < MIN_USEFUL_SIDE_BY_SIDE_WIDTH {
        let lines: Vec<TuiLine<'_>> = view
            .lines
            .iter()
            .enumerate()
            .map(|(idx, line)| render_rendered_line(line, idx == cursor_row, total_width))
            .collect();
        let widget = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(widget, area);
        return;
    }

    let geom = SideBySideGeometry {
        side_width: (total_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2,
        full_width: total_width,
    };

    let lines: Vec<TuiLine<'_>> = view
        .paired_rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            render_paired_row(
                view,
                PairedRowAt {
                    row: *row,
                    row_idx,
                    rows: &view.paired_rows,
                },
                row_idx == cursor_row,
                geom,
            )
        })
        .collect();

    let widget = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(widget, area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineCommentColumn {
    Left,
    Right,
}

fn inline_comment_column(
    rows: &[PairedRow],
    row_idx: usize,
    comment: &RenderedLine,
) -> InlineCommentColumn {
    let is_side_old = comment.source_line.is_some() && comment.target_line.is_none();
    if !is_side_old {
        return InlineCommentColumn::Right;
    }
    let mut probe = row_idx;
    while probe > 0 {
        probe -= 1;
        match rows.get(probe) {
            Some(PairedRow::Spanning(_)) => {}
            Some(PairedRow::Pair {
                left: Some(_),
                right: None,
                ..
            }) => return InlineCommentColumn::Left,
            Some(_) | None => return InlineCommentColumn::Right,
        }
    }
    InlineCommentColumn::Right
}

#[derive(Debug, Clone, Copy)]
struct SideBySideGeometry {
    side_width: u16,
    full_width: u16,
}

#[derive(Debug, Clone, Copy)]
struct PairedRowAt<'a> {
    row: PairedRow,
    row_idx: usize,
    rows: &'a [PairedRow],
}

fn render_paired_row<'a>(
    view: &'a DiffView,
    at: PairedRowAt<'_>,
    focused: bool,
    geom: SideBySideGeometry,
) -> TuiLine<'a> {
    match at.row {
        PairedRow::Spanning(idx) => {
            let Some(line) = view.lines.get(idx) else {
                return TuiLine::raw("");
            };
            if matches!(
                line.kind,
                RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody
            ) {
                let column = inline_comment_column(at.rows, at.row_idx, line);
                render_inline_comment_row(line, column, focused, geom.side_width)
            } else {
                render_full_width_row(line, focused, geom.full_width)
            }
        }
        PairedRow::Pair { left, right, .. } => {
            let left_spans = match left.and_then(|i| view.lines.get(i)) {
                Some(line) => side_cell_spans(line, geom.side_width, focused),
                None => blank_cell_spans(geom.side_width, focused),
            };
            let right_spans = match right.and_then(|i| view.lines.get(i)) {
                Some(line) => side_cell_spans(line, geom.side_width, focused),
                None => blank_cell_spans(geom.side_width, focused),
            };
            let gutter = side_by_side_gutter_spans();
            TuiLine::from([left_spans, gutter, right_spans].concat())
        }
    }
}

fn render_full_width_row(line: &RenderedLine, focused: bool, full_width: u16) -> TuiLine<'_> {
    let (body, fg_color) = prefix_truncate_pad(line, full_width);
    TuiLine::from(vec![Span::styled(body, focus_style(fg_color, focused))])
}

fn render_inline_comment_row(
    line: &RenderedLine,
    column: InlineCommentColumn,
    focused: bool,
    side_width: u16,
) -> TuiLine<'_> {
    let comment_spans = side_cell_spans(line, side_width, focused);
    let blank = blank_cell_spans(side_width, focused);
    let gutter = side_by_side_gutter_spans();
    let (left, right) = match column {
        InlineCommentColumn::Left => (comment_spans, blank),
        InlineCommentColumn::Right => (blank, comment_spans),
    };
    TuiLine::from([left, gutter, right].concat())
}

fn side_cell_spans(line: &RenderedLine, side_width: u16, focused: bool) -> Vec<Span<'_>> {
    let (body, fg_color) = prefix_truncate_pad(line, side_width);
    vec![Span::styled(body, focus_style(fg_color, focused))]
}

fn focus_style(fg_color: Color, focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if matches!(fg_color, Color::Reset) {
        Style::default()
    } else {
        Style::default().fg(fg_color)
    }
}

fn blank_cell_spans<'a>(side_width: u16, focused: bool) -> Vec<Span<'a>> {
    let body: String = " ".repeat(usize::from(side_width));
    vec![Span::styled(body, focus_style(Color::Reset, focused))]
}

fn side_by_side_gutter_spans<'a>() -> Vec<Span<'a>> {
    vec![Span::styled(
        " \u{2502} ",
        Style::default().fg(Color::DarkGray),
    )]
}

#[derive(Debug, Clone, Copy)]
struct LineVisual {
    prefix: &'static str,
    fg_color: Color,
}

fn prefix_truncate_pad(line: &RenderedLine, width: u16) -> (String, Color) {
    let attrs = line_visual_attrs(line);
    let prefix_chars = attrs.prefix.chars().count();
    let max_text_chars = usize::from(width).saturating_sub(prefix_chars);
    let text = truncate(&line.text, max_text_chars);
    let used = prefix_chars + text.chars().count();
    let pad = usize::from(width).saturating_sub(used);
    (
        format!("{}{}{}", attrs.prefix, text, " ".repeat(pad)),
        attrs.fg_color,
    )
}

fn line_visual_attrs(line: &RenderedLine) -> LineVisual {
    match line.kind {
        RenderedLineKind::Added => LineVisual {
            prefix: "+ ",
            fg_color: Color::Green,
        },
        RenderedLineKind::Removed => LineVisual {
            prefix: "- ",
            fg_color: Color::Red,
        },
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            LineVisual {
                prefix: "  ",
                fg_color: line.comment_severity.map_or(Color::Cyan, severity_color),
            }
        }
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice
        | RenderedLineKind::DescriptionLine => LineVisual {
            prefix: "  ",
            fg_color: Color::Reset,
        },
    }
}

fn render_rendered_line(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    match line.kind {
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            let attrs = line_visual_attrs(line);
            return TuiLine::from(vec![Span::styled(
                line.text.as_str(),
                focus_style(attrs.fg_color, focused),
            )]);
        }
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice
        | RenderedLineKind::Added
        | RenderedLineKind::Removed
        | RenderedLineKind::DescriptionLine => {}
    }

    if focused {
        let (body, fg_color) = prefix_truncate_pad(line, width);
        TuiLine::from(vec![Span::styled(body, focus_style(fg_color, true))])
    } else {
        let attrs = line_visual_attrs(line);
        let content_style = focus_style(attrs.fg_color, false);
        TuiLine::from(vec![
            Span::raw(attrs.prefix),
            Span::styled(line.text.as_str(), content_style),
        ])
    }
}

fn render_footer<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, area: Rect, app: &App<S>) {
    let (text, style) = if let Some(msg) = app.status_message.as_deref() {
        (msg.to_owned(), Style::default().fg(Color::Yellow))
    } else {
        let has_stack = app.surface.entry_count() > 1;
        (
            footer_text_for_width(area.width, has_stack, app.severity_filter),
            Style::default(),
        )
    };
    let widget = Paragraph::new(text).style(style);
    frame.render_widget(widget, area);
}

fn render_transition<S: ReviewSurfaceExt>(
    frame: &mut Frame<'_>,
    app: &App<S>,
    state: &TransitionState,
) {
    let area = frame.area();
    let modal_area =
        composer_overlay::centered_rect(area, TRANSITION_MODAL_WIDTH, TRANSITION_MODAL_HEIGHT);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double);
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let count = app.surface.entry_count();
    if count == 0 {
        return;
    }

    let reviewed_id = app.surface.entry_id_display(state.reviewed_index);
    let reviewed_desc = app.surface.entry_description(state.reviewed_index);
    let next_id = app.surface.entry_id_display(state.next_index);
    let next_desc = app.surface.entry_description(state.next_index);

    let reviewed_pos = state.reviewed_index + 1;
    let next_pos = state.next_index + 1;

    let reviewed_desc = truncate(&reviewed_desc, TRANSITION_DESC_BUDGET);
    let next_desc = truncate(&next_desc, TRANSITION_DESC_BUDGET);

    let body = format!(
        "\n  Reviewed\n  {reviewed_pos}/{count}  {reviewed_id}\n  {reviewed_desc}\n\n  ────────────────\n\n  Next\n  {next_pos}/{count}  {next_id}\n  {next_desc}\n",
    );

    let content_layout = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(body.as_str()), content_layout[0]);
    render_transition_comment_summary(frame, content_layout[1], state);
    frame.render_widget(Paragraph::new(TRANSITION_FOOTER_TEXT), content_layout[2]);
}

fn render_transition_comment_summary(frame: &mut Frame<'_>, area: Rect, state: &TransitionState) {
    let Some(count) = state.reviewed_comment_count else {
        let widget = Paragraph::new("  comments could not be loaded")
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(widget, area);
        return;
    };
    if count == 0 {
        return;
    }

    let h = state.severity_histogram;
    let mut spans: Vec<Span<'_>> = Vec::with_capacity(8);
    spans.push(Span::raw("  "));
    let mut needs_separator = false;
    if h.required > 0 {
        spans.push(Span::styled(
            render_dots(h.required),
            Style::default().fg(Color::Red),
        ));
        spans.push(Span::raw(format!(" {} required", h.required)));
        needs_separator = true;
    }
    if h.suggestion > 0 {
        if needs_separator {
            spans.push(Span::raw(" · "));
        }
        spans.push(Span::styled(
            render_dots(h.suggestion),
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw(format!(
            " {} {}",
            h.suggestion,
            pluralize("suggestion", h.suggestion)
        )));
        needs_separator = true;
    }
    if h.note > 0 {
        if needs_separator {
            spans.push(Span::raw(" · "));
        }
        spans.push(Span::styled(
            render_dots(h.note),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(format!(
            " {} {}",
            h.note,
            pluralize("note", h.note)
        )));
    }

    let line = TuiLine::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

fn handle_event<S: ReviewSurfaceExt>(app: &mut App<S>) -> Result<(), AppError<S>> {
    let evt = crossterm::event::read().map_err(AppError::Io)?;
    if let Event::Key(key) = evt {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match &app.screen {
            Screen::Main => handle_main_key(app, key).map_err(AppError::Surface)?,
            Screen::Help => handle_help_key(app, key),
            Screen::Transition(_) => handle_transition_key(app, key).map_err(AppError::Surface)?,
            Screen::Extra(_) => handle_extra_screen_key(app, key).map_err(AppError::Surface)?,
            Screen::FilePicker(_) => handle_file_picker_key(app, key),
        }
    }
    Ok(())
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_main_key<S: ReviewSurfaceExt>(app: &mut App<S>, key: KeyEvent) -> Result<(), S::Error> {
    // Clear status on every key (surface's handle_extra_key sets its own).
    app.status_message = None;

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.screen = Screen::Help,
        KeyCode::Up | KeyCode::Char('k') => app.move_line(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_line(1),
        KeyCode::PageUp => app.move_page(-1),
        KeyCode::PageDown => app.move_page(1),
        KeyCode::Home | KeyCode::Char('g') => app.jump_to(Edge::Top),
        KeyCode::End | KeyCode::Char('G') => app.jump_to(Edge::Bottom),
        KeyCode::Tab => app.cycle_file(1),
        KeyCode::BackTab => app.cycle_file(-1),
        KeyCode::Char('n') => app.advance_stack()?,
        KeyCode::Char('p') => app.retreat_stack()?,
        KeyCode::Char('f') => open_file_picker(app),
        KeyCode::Char('1') => app.toggle_severity_filter(Severity::Required),
        KeyCode::Char('2') => app.toggle_severity_filter(Severity::Suggestion),
        KeyCode::Char('3') => app.toggle_severity_filter(Severity::Note),
        KeyCode::Char('U') => app.toggle_current_file_reviewed(),
        KeyCode::Char('|') => app.cycle_diff_mode(),
        _ => {
            // Delegate to the surface for tool-specific keys.
            let action = app.surface.handle_extra_key(key)?;
            match action {
                ExtraKeyAction::Ignored => {}
                ExtraKeyAction::OpenScreen(state) => {
                    app.screen = Screen::Extra(state);
                }
                ExtraKeyAction::StatusMessage(msg) => {
                    app.status_message = Some(msg);
                }
                ExtraKeyAction::Quit => {
                    app.should_quit = true;
                }
            }
        }
    }
    Ok(())
}

fn handle_help_key<S: ReviewSurfaceExt>(app: &mut App<S>, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q' | '?') | KeyCode::Esc) {
        app.screen = Screen::Main;
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the transition modal"
)]
fn handle_transition_key<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<(), S::Error> {
    let Screen::Transition(ref state) = app.screen else {
        return Ok(());
    };
    let next_index = state.next_index;
    match key.code {
        KeyCode::Enter => {
            app.screen = Screen::Main;
            app.load_entry(next_index, true)?;
        }
        KeyCode::Char('p') => {
            app.screen = Screen::Main;
            app.retreat_stack()?;
        }
        KeyCode::Esc => {
            app.screen = Screen::Main;
        }
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        _ => {}
    }
    Ok(())
}

fn handle_extra_screen_key<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<(), S::Error> {
    let Screen::Extra(mut state) = std::mem::replace(&mut app.screen, Screen::Main) else {
        return Ok(());
    };
    let mut navigate_to_entry: Option<usize> = None;
    let action = app.surface.handle_extra_screen_key(
        state.as_mut(),
        key,
        &mut ExtraScreenContext {
            file_index: &mut app.file_index,
            line_index: &mut app.line_index,
            scroll: &mut app.scroll,
            rendered_per_file: &mut app.rendered_per_file,
            base_per_file: &mut app.annotated_per_file,
            should_quit: &mut app.should_quit,
            status_message: &mut app.status_message,
            last_severity: &mut app.last_severity,
            needs_full_redraw: &mut app.needs_full_redraw,
            severity_filter: app.severity_filter,
            navigate_to_entry: &mut navigate_to_entry,
        },
    )?;
    match action {
        ExtraScreenAction::StayOpen => {
            app.screen = Screen::Extra(state);
        }
        ExtraScreenAction::Close => {
            if let Some(idx) = navigate_to_entry {
                app.load_entry(idx, true)?;
            }
        }
        ExtraScreenAction::OpenScreen(new_state) => {
            app.screen = Screen::Extra(new_state);
        }
    }
    // After any extra-screen key, re-inject inline comments in case the
    // surface mutated its comment state.
    app.refresh_inline_comments();
    Ok(())
}

/// Shared mutable context passed from the core `App` to the surface's
/// `handle_extra_screen_key`. This lets the surface update core fields
/// (status message, last severity, etc.) without having a mutable reference
/// to the whole `App`.
pub struct ExtraScreenContext<'a> {
    pub file_index: &'a mut usize,
    pub line_index: &'a mut usize,
    pub scroll: &'a mut u16,
    /// Annotated `DiffView`s for rendering. Write completed annotation results
    /// here. Mutable so the surface can replace them after an out-of-band
    /// reload (e.g. after invoking Claude). Do not write pre-annotated (base)
    /// views here — use `base_per_file` for that.
    pub rendered_per_file: &'a mut Vec<DiffView>,
    /// Un-annotated base views. The core calls `refresh_inline_comments` after
    /// `handle_extra_screen_key` returns and injects inline comments into these.
    /// Writing pre-annotated views here causes double-injection. The
    /// [`BaseViews`] newtype enforces the invariant structurally.
    pub base_per_file: &'a mut BaseViews,
    pub should_quit: &'a mut bool,
    pub status_message: &'a mut Option<String>,
    pub last_severity: &'a mut Option<Severity>,
    pub needs_full_redraw: &'a mut bool,
    /// Read-only snapshot of the active severity filter. Extra-screen key
    /// handlers can read the filter to drive their display logic, but updating
    /// it is a main-view operation only — surfaces route filter changes through
    /// the core's `toggle_severity_filter` keybinding, not via this context.
    pub severity_filter: Option<Severity>,
    /// If set before returning `Close`, the core will call `load_entry` on this
    /// index after closing the screen. This lets the surface trigger a stack
    /// navigation from within `handle_extra_screen_key` without direct access
    /// to `App::goto_entry`.
    pub navigate_to_entry: &'a mut Option<usize>,
}

fn open_file_picker<S: ReviewSurfaceExt>(app: &mut App<S>) {
    let entries = app.surface.file_picker_entries();
    app.screen = Screen::FilePicker(FilePickerState {
        selected_index: 0,
        scroll_offset: 0,
        entries,
    });
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the file picker"
)]
fn handle_file_picker_key<S: ReviewSurfaceExt>(app: &mut App<S>, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.screen = Screen::Main;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::FilePicker(ref mut s) = app.screen {
                file_picker::move_cursor(s, -1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::FilePicker(ref mut s) = app.screen {
                file_picker::move_cursor(s, 1);
            }
        }
        KeyCode::Enter => {
            file_picker_enter(app);
        }
        _ => {}
    }
}

/// Minimum jump distance (in new-file coordinates) needed to emit the
/// "jumped to first commentable line" status message.
const FILE_JUMP_HINT_MIN_DELTA: usize = 3;

fn file_picker_enter<S: ReviewSurfaceExt>(app: &mut App<S>) {
    let Screen::FilePicker(ref state) = app.screen else {
        return;
    };
    let Some(entry) = state.entries.get(state.selected_index) else {
        return;
    };
    let view_index = entry.view_index;
    let is_binary = entry.is_binary;
    let first_commentable = entry.first_commentable_row;
    // Capture both before any state mutation so comparisons are in consistent spaces.
    let is_file_switch = view_index != app.file_index;
    let prior_line = app.line_index;
    app.screen = Screen::Main;
    app.file_index = view_index;
    app.scroll = 0;
    if is_binary {
        app.status_message = Some("binary file — no commentable lines".to_owned());
        app.line_index = 0;
        app.mark_current_file_reviewed();
        return;
    }
    app.line_index = first_commentable;
    // For a file switch, prior_line is in a different coordinate space, so use
    // first_commentable directly as the jump magnitude.  For same-file re-entry,
    // the delta from the prior position is the meaningful quantity.
    let jump_magnitude = if is_file_switch {
        first_commentable
    } else {
        first_commentable.saturating_sub(prior_line)
    };
    if first_commentable > 0 && jump_magnitude >= FILE_JUMP_HINT_MIN_DELTA {
        app.status_message = Some("jumped to first commentable line".to_owned());
    }
    app.mark_current_file_reviewed();
}

// ---------------------------------------------------------------------------
// Additional ReviewSurface methods needed by App (via a separate trait)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::tui::{
        DeleteOutcome, DeleteRequest, DiffView, ExtraKeyAction, ExtraScreenAction,
        ExtraScreenContext, FilePickerEntry, FilePickerState, MarkReviewedOutcome, RenderedLine,
        RenderedLineKind, ReviewSurface, ReviewedOutcome, SaveOutcome, SaveRequest,
        SeverityHistogram, UpdateRequest,
    };

    // -----------------------------------------------------------------------
    // Minimal no-op surface for App construction
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct NoopError;
    impl core::fmt::Display for NoopError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "noop")
        }
    }
    impl core::error::Error for NoopError {}

    struct NoopSurface {
        views: Vec<DiffView>,
        not_tracked: bool,
    }

    impl NoopSurface {
        fn new(views: Vec<DiffView>) -> Self {
            Self {
                views,
                not_tracked: false,
            }
        }

        fn new_not_tracked(views: Vec<DiffView>) -> Self {
            Self {
                views,
                not_tracked: true,
            }
        }
    }

    impl ReviewSurface for NoopSurface {
        type Error = NoopError;
        fn entry_count(&self) -> usize {
            1
        }
        fn current_entry_index(&self) -> usize {
            0
        }
        fn entry_id_display(&self, _: usize) -> String {
            String::new()
        }
        fn entry_description(&self, _: usize) -> String {
            String::new()
        }
        fn fetch_views(&mut self, _: usize) -> Result<Vec<DiffView>, NoopError> {
            Ok(self.views.clone())
        }
        fn inline_comments_for_view(
            &self,
            _: std::time::SystemTime,
            _: usize,
            _: Option<Severity>,
        ) -> Vec<crate::tui::InlineComment> {
            Vec::new()
        }
        fn save_comment(&mut self, _: SaveRequest<'_>) -> Result<SaveOutcome, NoopError> {
            Ok(SaveOutcome::Refused {
                reason: String::new(),
            })
        }
        fn update_comment(&mut self, _: UpdateRequest<'_>) -> Result<SaveOutcome, NoopError> {
            Ok(SaveOutcome::Refused {
                reason: String::new(),
            })
        }
        fn delete_comment(&mut self, _: DeleteRequest) -> Result<DeleteOutcome, NoopError> {
            Ok(DeleteOutcome::Deleted)
        }
        fn is_view_reviewed(&self, _: usize) -> bool {
            false
        }
        fn mark_view_reviewed(&mut self, _: usize) -> MarkReviewedOutcome {
            if self.not_tracked {
                MarkReviewedOutcome::NotTracked
            } else {
                MarkReviewedOutcome::NoReset
            }
        }
        fn toggle_view_reviewed(&mut self, _: usize) -> ReviewedOutcome {
            if self.not_tracked {
                ReviewedOutcome::NotTracked
            } else {
                ReviewedOutcome::Unmarked
            }
        }
        fn severity_histogram(&self) -> SeverityHistogram {
            SeverityHistogram::default()
        }
        fn handle_extra_key(&mut self, _: KeyEvent) -> Result<ExtraKeyAction, NoopError> {
            Ok(ExtraKeyAction::Ignored)
        }
        fn render_extra_screen(&self, _: &mut Frame<'_>, _: &mut dyn ExtraScreen) {}
        fn handle_extra_screen_key(
            &mut self,
            _: &mut dyn ExtraScreen,
            _: KeyEvent,
            _: &mut ExtraScreenContext<'_>,
        ) -> Result<ExtraScreenAction, NoopError> {
            Ok(ExtraScreenAction::StayOpen)
        }
        fn file_picker_entries(&self) -> Vec<FilePickerEntry> {
            Vec::new()
        }
        fn help_screen_title(&self) -> &'static str {
            "test"
        }
    }

    impl ReviewSurfaceExt for NoopSurface {
        fn on_entry_loaded(&mut self, _: usize, _: bool) {}
        fn severity_histogram_for_transition(&self) -> (Option<usize>, SeverityHistogram) {
            (Some(0), SeverityHistogram::default())
        }
    }

    fn make_line(kind: RenderedLineKind) -> RenderedLine {
        RenderedLine {
            kind,
            text: String::new(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        }
    }

    fn view_with_kinds(kinds: &[RenderedLineKind]) -> DiffView {
        DiffView {
            title: "test".to_owned(),
            lines: kinds.iter().copied().map(make_line).collect(),
            paired_rows: Vec::new(),
        }
    }

    fn make_app(view: DiffView) -> App<NoopSurface> {
        let surface = NoopSurface::new(vec![view.clone()]);
        let mut app = App::new(surface, vec![view], TransitionMode::Never);
        app.diff_mode = DiffMode::ForceUnified;
        app
    }

    fn make_app_with_views(views: Vec<DiffView>) -> App<NoopSurface> {
        let surface = NoopSurface::new(views.clone());
        let mut app = App::new(surface, views, TransitionMode::Never);
        app.diff_mode = DiffMode::ForceUnified;
        app
    }

    // -----------------------------------------------------------------------
    // render_dots / render_dots_mixed
    // -----------------------------------------------------------------------

    #[test]
    fn render_dots_zero_is_empty() {
        assert_eq!(render_dots(0), "");
    }

    #[test]
    fn render_dots_at_budget_no_ellipsis() {
        let s = render_dots(DOT_BUDGET);
        assert_eq!(s.chars().filter(|&c| c == '●').count(), DOT_BUDGET);
        assert!(!s.contains('…'));
    }

    #[test]
    fn render_dots_over_budget_adds_ellipsis() {
        let s = render_dots(DOT_BUDGET + 1);
        assert_eq!(s.chars().filter(|&c| c == '●').count(), DOT_BUDGET);
        assert!(s.contains('…'));
    }

    #[test]
    fn render_dots_mixed_zero_histogram_is_empty() {
        assert_eq!(render_dots_mixed(SeverityHistogram::default()), "");
    }

    #[test]
    fn render_dots_mixed_within_budget_no_ellipsis() {
        let h = SeverityHistogram {
            required: 3,
            suggestion: 0,
            note: 0,
        };
        let s = render_dots_mixed(h);
        assert_eq!(s.chars().filter(|&c| c == '●').count(), 3);
        assert!(!s.contains('…'));
    }

    #[test]
    fn render_dots_mixed_overflow_adds_ellipsis() {
        let h = SeverityHistogram {
            required: 3,
            suggestion: 3,
            note: 0,
        };
        let s = render_dots_mixed(h);
        assert!(s.contains('…'));
        assert!(s.chars().filter(|&c| c == '●').count() <= DOT_BUDGET);
    }

    // -----------------------------------------------------------------------
    // resolve_diff_mode
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_diff_mode_description_view_always_unified() {
        assert_eq!(
            resolve_diff_mode(DiffMode::ForceSideBySide, 200, 0),
            EffectiveDiffMode::Unified
        );
    }

    #[test]
    fn resolve_diff_mode_auto_narrow_is_unified() {
        assert_eq!(
            resolve_diff_mode(DiffMode::Auto, SIDE_BY_SIDE_MIN_WIDTH - 1, 1),
            EffectiveDiffMode::Unified
        );
    }

    #[test]
    fn resolve_diff_mode_auto_wide_is_side_by_side() {
        assert_eq!(
            resolve_diff_mode(DiffMode::Auto, SIDE_BY_SIDE_MIN_WIDTH, 1),
            EffectiveDiffMode::SideBySide
        );
    }

    #[test]
    fn resolve_diff_mode_force_unified_overrides_width() {
        assert_eq!(
            resolve_diff_mode(DiffMode::ForceUnified, 300, 1),
            EffectiveDiffMode::Unified
        );
    }

    #[test]
    fn resolve_diff_mode_force_side_by_side_overrides_width() {
        assert_eq!(
            resolve_diff_mode(DiffMode::ForceSideBySide, 10, 1),
            EffectiveDiffMode::SideBySide
        );
    }

    // -----------------------------------------------------------------------
    // move_line — boundary skip-row handling (p2)
    // -----------------------------------------------------------------------

    /// View with `HunkSeparator` at 0 and Added at 1. Downward navigation from
    /// index 0 must not stay on the separator — the boundary fallback scan
    /// must find row 1.
    #[test]
    fn move_line_skip_row_at_boundary_zero_scans_forward() {
        let view = view_with_kinds(&[RenderedLineKind::HunkSeparator, RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.line_index = 0;
        app.move_line(1);
        assert_eq!(
            app.line_index, 1,
            "downward from separator-at-0 must land on Added at 1"
        );
    }

    /// View with all separators — cursor must not move.
    #[test]
    fn move_line_all_skip_rows_cursor_unmoved() {
        let view = view_with_kinds(&[
            RenderedLineKind::HunkSeparator,
            RenderedLineKind::HunkSeparator,
            RenderedLineKind::HunkSeparator,
        ]);
        let mut app = make_app(view);
        app.line_index = 1;
        app.move_line(1);
        assert_eq!(
            app.line_index, 1,
            "all-separator view must leave cursor unmoved"
        );
    }

    /// View with Added at 0 and `HunkSeparator` at `max_index`. Upward navigation
    /// from `max_index` must not stay on the separator — the boundary fallback scan
    /// must find row 0 in the reverse direction.
    #[test]
    fn move_line_skip_row_at_boundary_max_scans_backward() {
        let view = view_with_kinds(&[RenderedLineKind::Added, RenderedLineKind::HunkSeparator]);
        let mut app = make_app(view);
        app.line_index = 1; // start on the separator at max_index
        app.move_line(-1);
        assert_eq!(
            app.line_index, 0,
            "upward from separator-at-max must land on Added at 0"
        );
    }

    /// Normal navigable view — cursor advances one step.
    #[test]
    fn move_line_normal_advances_cursor() {
        let view = view_with_kinds(&[RenderedLineKind::Added, RenderedLineKind::Context]);
        let mut app = make_app(view);
        app.line_index = 0;
        app.move_line(1);
        assert_eq!(app.line_index, 1);
    }

    // -----------------------------------------------------------------------
    // cycle_file
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_file_single_view_sets_status() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.cycle_file(1);
        assert!(
            app.status_message.is_some(),
            "single-view cycle must surface a status message"
        );
    }

    // -----------------------------------------------------------------------
    // advance_stack / retreat_stack — single-entry stack
    // -----------------------------------------------------------------------

    #[test]
    fn advance_stack_single_entry_does_not_move() {
        // Surface returns entry_count=1; advance should set a status message
        // and NOT attempt to load a new entry.
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        let initial_idx = app.surface.current_entry_index();
        app.advance_stack().expect("advance_stack must not error");
        assert_eq!(
            app.surface.current_entry_index(),
            initial_idx,
            "advance_stack on a single-entry stack must not change current_entry_index"
        );
        assert!(
            app.status_message.is_some(),
            "advance_stack on a single-entry stack must surface a status message"
        );
    }

    #[test]
    fn retreat_stack_single_entry_does_not_move() {
        // Surface returns entry_count=1; retreat should set a status message
        // and NOT attempt to load a previous entry.
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        let initial_idx = app.surface.current_entry_index();
        app.retreat_stack().expect("retreat_stack must not error");
        assert_eq!(
            app.surface.current_entry_index(),
            initial_idx,
            "retreat_stack on a single-entry stack must not change current_entry_index"
        );
        assert!(
            app.status_message.is_some(),
            "retreat_stack on a single-entry stack must surface a status message"
        );
    }

    // -----------------------------------------------------------------------
    // jump_to — skip-row normalization after landing (magnus.m1)
    // -----------------------------------------------------------------------

    /// When the first row is a `HunkSeparator`, jumping to Top must skip it and
    /// land on the nearest navigable row.
    #[test]
    fn jump_to_top_skips_hunk_separator_at_row_zero() {
        let view = view_with_kinds(&[RenderedLineKind::HunkSeparator, RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.line_index = 1; // start elsewhere so the jump is meaningful
        app.jump_to(Edge::Top);
        assert_ne!(
            app.line_index, 0,
            "jump_to(Top) must not strand cursor on HunkSeparator at row 0"
        );
        assert_eq!(
            app.line_index, 1,
            "jump_to(Top) must advance to the first navigable row"
        );
    }

    // -----------------------------------------------------------------------
    // file_picker_enter — "jumped" status message gating (priya.p1+saskia.s2)
    // -----------------------------------------------------------------------

    fn make_file_picker_entry(view_index: usize, first_commentable_row: usize) -> FilePickerEntry {
        FilePickerEntry {
            display_path: std::path::PathBuf::from("src/foo.rs"),
            view_index,
            comment_count: 0,
            reviewed: false,
            is_binary: false,
            first_commentable_row,
        }
    }

    /// Entering the picker when `first_commentable_row > 0` and the prior
    /// `line_index` differs must set the "jumped" status message.
    #[test]
    fn file_picker_enter_jumps_and_sets_message() {
        let view = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Context,
            RenderedLineKind::Context,
            RenderedLineKind::Added,
        ]);
        let mut app = make_app(view);
        app.line_index = 0; // prior position differs from first_commentable=3
        let entry = make_file_picker_entry(0, 3);
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: vec![entry],
        });
        // Simulate pressing Enter in the file picker.
        file_picker_enter(&mut app);
        assert_eq!(
            app.line_index, 3,
            "cursor must move to first commentable row"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("jumped to first commentable line"),
            "status message must fire when cursor actually moves to first commentable"
        );
    }

    /// A trivial jump of 1 or 2 rows must not emit the "jumped" message; the
    /// threshold is ≥ 3.
    #[test]
    fn file_picker_enter_no_message_for_small_delta() {
        let view = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Added,
            RenderedLineKind::Context,
        ]);
        for (prior, first_commentable) in [(0usize, 1usize), (0, 2), (1, 2)] {
            let mut app = make_app(view.clone());
            app.line_index = prior;
            let entry = make_file_picker_entry(0, first_commentable);
            app.screen = Screen::FilePicker(FilePickerState {
                selected_index: 0,
                scroll_offset: 0,
                entries: vec![entry],
            });
            file_picker_enter(&mut app);
            assert!(
                app.status_message.is_none(),
                "delta {} must not emit 'jumped' message",
                first_commentable - prior,
            );
        }
    }

    /// When `line_index` is already at `first_commentable_row`, no "jumped"
    /// message should fire because the cursor did not actually move.
    #[test]
    fn file_picker_enter_no_message_when_already_at_first_commentable() {
        let view = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Context,
            RenderedLineKind::Context,
            RenderedLineKind::Added,
        ]);
        let mut app = make_app(view);
        app.line_index = 3; // already at first_commentable
        let entry = make_file_picker_entry(0, 3);
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: vec![entry],
        });
        file_picker_enter(&mut app);
        assert_eq!(
            app.line_index, 3,
            "cursor must stay at first commentable row"
        );
        assert!(
            app.status_message.is_none(),
            "no status message when cursor was already at first commentable row"
        );
    }

    // -----------------------------------------------------------------------
    // move_line — delta=0 interior skip-row (priya.p1)
    // -----------------------------------------------------------------------

    /// When the cursor sits on an interior `HunkSeparator` and `move_line(0)`
    /// is called, the cursor must move off the skip row to a navigable row.
    /// The view is `[Added, HunkSeparator, Context]` so the separator is at
    /// index 1 — neither 0 nor `max_index`.
    #[test]
    fn move_line_delta_zero_interior_skip_lands_on_navigable_row() {
        let view = view_with_kinds(&[
            RenderedLineKind::Added,
            RenderedLineKind::HunkSeparator,
            RenderedLineKind::Context,
        ]);
        let mut app = make_app(view);
        app.line_index = 1; // place cursor on the interior HunkSeparator
        app.move_line(0);
        assert_ne!(
            app.line_index, 1,
            "move_line(0) must move cursor off interior HunkSeparator"
        );
    }

    // -----------------------------------------------------------------------
    // file_picker_enter — first_commentable=0 with prior_line != 0 (priya.p2)
    // -----------------------------------------------------------------------

    /// When `first_commentable_row` is 0 the condition
    /// `first_commentable > 0 && ...` is false, so no "jumped" status message
    /// should be emitted even when the prior `line_index` differs.
    #[test]
    fn file_picker_enter_no_message_when_first_commentable_is_zero() {
        let view = view_with_kinds(&[
            RenderedLineKind::Added,
            RenderedLineKind::Context,
            RenderedLineKind::Context,
        ]);
        let mut app = make_app(view);
        app.line_index = 5; // non-zero prior position
        let entry = make_file_picker_entry(0, 0); // first_commentable_row = 0
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: vec![entry],
        });
        file_picker_enter(&mut app);
        assert_eq!(
            app.line_index, 0,
            "cursor must be set to first_commentable_row (0)"
        );
        assert!(
            app.status_message.is_none(),
            "no status message when first_commentable_row is 0"
        );
    }

    // -----------------------------------------------------------------------
    // file_picker_enter — cross-file coordinate space (magnus.m2+priya.p1)
    // -----------------------------------------------------------------------

    /// When the user switches to a different file the jump magnitude is
    /// `first_commentable` in the new file, not the delta from `prior_line`
    /// in the old file.  Here `prior_line=10` > `first_commentable=3`, so the
    /// delta-based formula would yield 0 (`saturating_sub`) and suppress the
    /// message — the fix ensures it fires.
    #[test]
    fn file_picker_enter_cross_file_uses_new_file_coordinates() {
        let view0 = view_with_kinds(&[RenderedLineKind::Added, RenderedLineKind::Context]);
        let view1 = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Context,
            RenderedLineKind::Context,
            RenderedLineKind::Added,
        ]);
        let mut app = make_app_with_views(vec![view0, view1]);
        app.file_index = 0;
        app.line_index = 10; // prior position in old file — different coordinate space
        let entry = make_file_picker_entry(1, 3); // switching to file index 1
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: vec![entry],
        });
        file_picker_enter(&mut app);
        assert_eq!(
            app.line_index, 3,
            "cursor must move to first_commentable_row in new file"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("jumped to first commentable line"),
            "status message must fire based on new-file coordinates, not cross-file delta"
        );
    }

    /// When switching files, the jump magnitude is `first_commentable` in the
    /// new file — not a cross-file delta.  `first_commentable ∈ {1, 2}` is
    /// below `FILE_JUMP_HINT_MIN_DELTA` (3), so no "jumped" message fires even
    /// though this is a file switch.
    #[test]
    fn file_picker_enter_cross_file_small_first_commentable_no_message() {
        let view0 = view_with_kinds(&[RenderedLineKind::Added]);
        let view1 = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Added,
            RenderedLineKind::Context,
        ]);
        for first_commentable in [1usize, 2usize] {
            let mut app = make_app_with_views(vec![view0.clone(), view1.clone()]);
            app.file_index = 0;
            app.line_index = 10; // prior position in old file (different coordinate space)
            let entry = make_file_picker_entry(1, first_commentable);
            app.screen = Screen::FilePicker(FilePickerState {
                selected_index: 0,
                scroll_offset: 0,
                entries: vec![entry],
            });
            file_picker_enter(&mut app);
            assert_eq!(
                app.line_index, first_commentable,
                "cursor must move to first_commentable_row in new file"
            );
            assert!(
                app.status_message.is_none(),
                "file switch with first_commentable={first_commentable} (< FILE_JUMP_HINT_MIN_DELTA) must not emit 'jumped' message",
            );
        }
    }

    /// Same-file re-entry with a small delta must not emit the "jumped"
    /// message even when `first_commentable` >= `FILE_JUMP_HINT_MIN_DELTA`, because
    /// the delta from the prior position is what matters on same-file navigation.
    #[test]
    fn file_picker_enter_same_file_small_delta_no_message() {
        let view = view_with_kinds(&[
            RenderedLineKind::HunkHeader,
            RenderedLineKind::Context,
            RenderedLineKind::Added,
        ]);
        let mut app = make_app(view);
        app.file_index = 0;
        app.line_index = 0;
        let entry = make_file_picker_entry(0, 2); // same file, delta = 2 < 3
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: vec![entry],
        });
        file_picker_enter(&mut app);
        assert_eq!(
            app.line_index, 2,
            "cursor must move to first_commentable_row"
        );
        assert!(
            app.status_message.is_none(),
            "same-file delta of 2 must not emit 'jumped' message"
        );
    }

    // -----------------------------------------------------------------------
    // toggle_current_file_reviewed — NotTracked arm
    // -----------------------------------------------------------------------

    #[test]
    fn toggle_current_file_reviewed_not_tracked_leaves_status_unchanged() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let surface = NoopSurface::new_not_tracked(vec![view.clone()]);
        let mut app = App::new(surface, vec![view], TransitionMode::Never);
        app.diff_mode = DiffMode::ForceUnified;
        assert!(
            app.status_message.is_none(),
            "precondition: status_message must start as None"
        );
        app.toggle_current_file_reviewed();
        assert!(
            app.status_message.is_none(),
            "NotTracked must leave status_message unchanged (None)"
        );
    }

    #[test]
    fn toggle_current_file_reviewed_not_tracked_does_not_clobber_existing_status() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let surface = NoopSurface::new_not_tracked(vec![view.clone()]);
        let mut app = App::new(surface, vec![view], TransitionMode::Never);
        app.diff_mode = DiffMode::ForceUnified;
        app.status_message = Some("prior".to_owned());
        app.toggle_current_file_reviewed();
        assert_eq!(
            app.status_message.as_deref(),
            Some("prior"),
            "NotTracked must not clobber a pre-existing status_message"
        );
    }
}
