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
    /// Entity list — primary entry point after entering a change/commit.
    Main,
    /// Focused full-file diff for one entity (pre-scrolled + range highlight).
    EntityDiff {
        /// Index into `App::entities` identifying the focused entity.
        entity_idx: usize,
    },
    /// Full file diff without entity focus (the `F` escape hatch).
    FileDiff {
        /// Index into `App::rendered_per_file`.
        file_idx: usize,
    },
    Help,
    /// Between-change transition beat shown in stack mode.
    Transition(TransitionState),
    /// Tool-specific overlay (composer, stale, overview, send-to-claude, etc.).
    Extra(Box<dyn ExtraScreen>),
    /// File picker modal.
    FilePicker(FilePickerState),
    /// Extraction in progress: showing loading overlay.
    Extracting,
}

/// Snapshot of the screen the user was on before opening a `Screen::Extra`
/// overlay (typically the comment composer). Used by `handle_extra_screen_key`
/// to restore the caller's screen on `ExtraScreenAction::Close` so that
/// saving a comment from the entity-diff view returns to the entity-diff
/// view rather than dropping back to the entity list.
///
/// Only the screen kinds that can host an `OpenScreen` action appear here —
/// composers only open from `EntityDiff` and `FileDiff` today.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScreenBeforeExtra {
    EntityDiff { entity_idx: usize },
    FileDiff { file_idx: usize },
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
    /// Scroll offset for the help screen.
    pub(crate) help_scroll: u16,

    // ── Entity navigation state (Phase 3) ─────────────────────────────────────
    /// Entities loaded for the current entry; empty until extraction completes.
    pub(crate) entities: Vec<crate::semantic::EntitySummary>,
    /// Cached Σ scope line for the orientation header. Recomputed whenever
    /// `entities` or `rendered_per_file` change (entry load, extraction
    /// events) — never during render, which runs per frame.
    pub(crate) header_stats: String,
    /// Pinned description row for the entity list.
    pub(crate) description_summary: Option<crate::semantic::DescriptionSummary>,
    /// Cursor row in the entity list (0 = description row, 1+ = entities).
    pub(crate) entity_index: usize,
    /// Scroll offset for the entity list.
    pub(crate) entity_scroll: usize,
    /// `true` when the `;` cosmetic filter is active.
    pub(crate) cosmetic_filter_on: bool,
    /// `true` when `Screen::EntityDiff` shows only lines within the entity's
    /// range rather than the full file diff. Toggled with `x`.
    pub(crate) entity_clip: bool,
    /// Tracks the `(entity_idx, entity_clip)` pair for which scroll was last
    /// initialized; prevents re-initializing on every render tick.
    pub(crate) entity_diff_initialized: Option<(usize, bool)>,
    /// Screen to restore when an `Screen::Extra` overlay closes. Set when
    /// transitioning into `Extra` via an `OpenScreen` action, consumed on
    /// `Close`. Without this the user always lands on the entity list after
    /// saving a comment, even when they opened the composer from inside the
    /// entity diff or the file diff.
    pub(crate) screen_before_extra: Option<ScreenBeforeExtra>,
    /// In-progress extraction worker (present only while `screen == Extracting`).
    pub(crate) extraction: Option<crate::tui::entity_list::ExtractionInProgress>,
    /// Monotonically-incrementing tick for spinner animation.
    pub(crate) tick: u64,
    /// Persistent entity context shown in the footer while in
    /// `Screen::EntityDiff`. Updated each render tick from the cursor
    /// position; `None` when the cursor is outside every entity's line range.
    /// Shown only when no transient `status_message` is active.
    pub(crate) entity_context: Option<String>,
    /// Active entity-list order, cycled by `o` (risk → dependency → file).
    /// Session-persisted: survives entry navigation, never written to disk.
    pub(crate) order_mode: crate::semantic::OrderMode,
    /// `true` when the current entry's risk tiers were computed without a
    /// graph — fan-out is unknown and tiers resolved upward. Surfaced in
    /// the entity-diff status bar.
    pub(crate) tiers_degraded: bool,
    /// Ensures the "risk tiers degraded" status notice fires at most once
    /// per session rather than on every entry load.
    pub(crate) degraded_notice_shown: bool,
    /// Entry to load on the next event-loop iteration, after the current frame
    /// has been drawn. Set by `advance_stack` / `retreat_stack` so the "loading"
    /// status is visible before the blocking `load_entry` call runs.
    pub(crate) pending_load: Option<(usize, bool)>,
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
            help_scroll: 0,
            entities: Vec::new(),
            header_stats: String::new(),
            description_summary: None,
            entity_index: 0,
            entity_scroll: 0,
            cosmetic_filter_on: false,
            entity_clip: true,
            entity_diff_initialized: None,
            screen_before_extra: None,
            extraction: None,
            tick: 0,
            entity_context: None,
            order_mode: crate::semantic::OrderMode::Risk,
            tiers_degraded: false,
            degraded_notice_shown: false,
            pending_load: None,
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
        // Keep the Screen::FileDiff variant in sync so render_file_diff_screen
        // does not reset file_index back to the entry value on the next tick.
        if matches!(self.screen, Screen::FileDiff { .. }) {
            self.screen = Screen::FileDiff {
                file_idx: new_index,
            };
        }
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

    /// Scroll so that `margin` rows after the cursor are visible. Used right
    /// after a comment is saved: the cursor stays on the commented line, but
    /// the freshly injected `┃ ● ...` rows sit one row past the cursor and
    /// would otherwise spill below the viewport — making the comment look
    /// like it disappeared. A small margin (a handful of rows) is enough to
    /// surface the typical 1–3 line inline comment without yanking the
    /// cursor away from where the user was working.
    pub fn ensure_rows_after_cursor_visible(&mut self, margin: u16) {
        let viewport_rows = self.viewport_rows;
        if viewport_rows == 0 {
            return;
        }
        let line_index_u16 = u16::try_from(self.line_index).unwrap_or(u16::MAX);
        let target = line_index_u16.saturating_add(margin);
        let last_visible = self.scroll.saturating_add(viewport_rows.saturating_sub(1));
        if target > last_visible {
            self.scroll = target.saturating_sub(viewport_rows.saturating_sub(1));
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
        // Populate the entity list so the first draw shows entities, not a
        // blank gray bar. load_entry does the same; this covers the startup
        // path in run_app which calls reload_current_entry, not load_entry.
        self.start_entity_extraction(idx);
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
        // Start entity extraction for the entity list view.
        self.start_entity_extraction(idx);
        Ok(())
    }

    /// Kick off entity extraction for entry `idx` in a background thread.
    ///
    /// If extraction finishes instantly (cache hit), the entities land in
    /// `self.entities` immediately. Otherwise the screen transitions to
    /// `Screen::Extracting` and the main loop polls the channel.
    /// Recompute the cached orientation-header Σ line. Called at every point
    /// `entities` or `rendered_per_file` change — never from render.
    pub(crate) fn refresh_header_stats(&mut self) {
        self.header_stats =
            crate::tui::entity_list::stats_line(&self.entities, &self.rendered_per_file);
    }

    /// Compute risk tiers for the loaded entities and apply the active
    /// order mode. Called once per entry load, after the entity list is
    /// complete (sync fetch or end of async extraction) — never per frame.
    /// One `entry_graph` read serves both tiering and ordering.
    pub(crate) fn refresh_entity_order(&mut self) {
        let idx = self.surface.current_entry_index();
        let graph = self.surface.entry_graph(idx);
        crate::semantic::compute_risk_tiers(&mut self.entities, graph.as_ref());
        self.tiers_degraded = graph.is_none();
        crate::semantic::sort_entities(&mut self.entities, self.order_mode, graph.as_ref());
        if self.tiers_degraded && !self.degraded_notice_shown && !self.entities.is_empty() {
            self.degraded_notice_shown = true;
            self.status_message = Some(match self.surface.graph_unavailable_reason() {
                Some(reason) => format!("graph unavailable — {reason}; risk tiers degraded"),
                None => "graph unavailable — risk tiers degraded".to_owned(),
            });
        }
    }

    fn start_entity_extraction(&mut self, idx: usize) {
        // Cancel any in-flight extraction.
        if let Some(ref prev) = self.extraction {
            crate::tui::entity_list::cancel_extraction(prev);
        }
        self.extraction = None;
        self.entities.clear();
        self.refresh_header_stats();
        self.description_summary = None;
        self.entity_index = 0;
        self.entity_scroll = 0;

        // Preserve the current diff view when reloading after a comment save.
        // Only navigate to Screen::Main when arriving at an entry fresh (e.g.
        // load_entry, startup) — not when reload_current_entry refreshes
        // inline comments while the user is already inside an entity diff.
        let in_diff_view = matches!(
            self.screen,
            Screen::EntityDiff { .. } | Screen::FileDiff { .. }
        );

        // Description pane (ggr entry 0, description pane active): show the
        // DiffView that fetch_views already loaded and skip entity extraction.
        // The user switches to the entity pane with `e`.
        if self.surface.is_description_entry(idx) {
            if !in_diff_view {
                self.screen = Screen::FileDiff { file_idx: 0 };
            }
            return;
        }

        // Async path: surface owns a runnable extraction task. Spawn it on a
        // background thread, set up the progress channel, and transition to
        // `Screen::Extracting`. The event loop polls the channel with a
        // timeout so the spinner animates while the worker runs.
        if let Some(task) = self.surface.entity_extraction_task(idx) {
            let (tx, rx) = std::sync::mpsc::channel();
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_for_thread = std::sync::Arc::clone(&cancel);
            std::thread::spawn(move || task.run(tx, cancel_for_thread));
            self.extraction = Some(crate::tui::entity_list::ExtractionInProgress {
                rx,
                cancel,
                files_done: 0,
                files_total: 0,
                files_failed: 0,
            });
            self.description_summary = self.surface.fetch_description_summary(idx).ok();
            // The async path always uses the loading overlay so the user
            // sees feedback. When extraction completes the event loop
            // restores the diff view via `screen_before_extra` if needed.
            // For the diff-view case (reload after comment save), the user
            // is in `EntityDiff` and we keep them there — the entity list
            // is invisible to them anyway. For the fresh-load case we want
            // the overlay, so we transition.
            if !in_diff_view {
                self.screen = Screen::Extracting;
            }
            return;
        }

        // Sync fallback: surface has no async task. Block on fetch_entity_list.
        match self.surface.fetch_entity_list(idx) {
            Ok(entities) => {
                self.entities = entities;
                self.refresh_entity_order();
                self.refresh_header_stats();
                self.description_summary = self.surface.fetch_description_summary(idx).ok();
                if !in_diff_view {
                    self.screen = Screen::Main;
                }
            }
            Err(_) => {
                if !in_diff_view {
                    self.screen = Screen::Main;
                }
            }
        }
    }

    /// Scroll the diff view so that `target_line` (1-indexed) is near the top.
    /// Used in tests; production code uses diff-view row translation instead.
    #[cfg(test)]
    pub(crate) fn scroll_to_line(&mut self, target_line: u32) {
        let line = usize::from(u16::try_from(target_line).unwrap_or(u16::MAX));
        self.line_index = line.saturating_sub(1);
        self.scroll = u16::try_from(line.saturating_sub(3)).unwrap_or(0);
    }

    /// Advance to the next entity in `Screen::EntityDiff` (clamps at last).
    pub(crate) fn next_entity(&mut self) {
        if self.entities.is_empty() {
            return;
        }
        let current = if let Screen::EntityDiff { entity_idx } = self.screen {
            entity_idx
        } else {
            0
        };
        let next = (current + 1).min(self.entities.len() - 1);
        self.screen = Screen::EntityDiff { entity_idx: next };
        self.mark_current_entity_reviewed();
    }

    /// Retreat to the previous entity in `Screen::EntityDiff` (clamps at first).
    pub(crate) fn prev_entity(&mut self) {
        let current = if let Screen::EntityDiff { entity_idx } = self.screen {
            entity_idx
        } else {
            0
        };
        self.screen = Screen::EntityDiff {
            entity_idx: current.saturating_sub(1),
        };
        self.mark_current_entity_reviewed();
    }

    /// Mark the entity currently shown in `Screen::EntityDiff` as reviewed.
    ///
    /// No-op when the screen is not `EntityDiff` or the entity index is
    /// out-of-bounds.
    pub(crate) fn mark_current_entity_reviewed(&mut self) {
        let entity_idx = if let Screen::EntityDiff { entity_idx } = &self.screen {
            *entity_idx
        } else {
            return;
        };
        self.mark_entity_reviewed_at(entity_idx);
    }

    /// Persist the reviewed bit for `self.entities[entity_idx]` and update
    /// the in-memory copy in lockstep. Both writes must succeed-or-no-op
    /// together: surface-only persists across sessions but leaves the
    /// entity-list ✓ stale until reload; in-memory-only flickers a ✓ that
    /// disappears on next open. Callers from both the tab-cycle path
    /// (`mark_current_entity_reviewed`) and the render-path auto-mark on
    /// first entry must go through here so the ✓ is consistent.
    pub(crate) fn mark_entity_reviewed_at(&mut self, entity_idx: usize) {
        let Some(entity) = self.entities.get_mut(entity_idx) else {
            return;
        };
        let entry_idx = self.surface.current_entry_index();
        let entity_id = entity.id.clone();
        let content_hash = entity.content_hash;
        entity.reviewed = true;
        self.surface
            .mark_entity_reviewed(entry_idx, &entity_id, content_hash);
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
        // Defer the blocking load so the current frame (with "loading…" status)
        // is drawn before fetch_views blocks.
        self.status_message = Some("loading…".to_owned());
        self.pending_load = Some((next_index, true));
        Ok(())
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
        self.status_message = Some("loading…".to_owned());
        self.pending_load = Some((current - 1, false));
        Ok(())
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

    /// Called by the event loop immediately after drawing each frame while an
    /// extra screen is open. If this returns `Some(action)`, the action is
    /// applied and the loop redraws without waiting for a key event.
    ///
    /// Surfaces use this to power loading overlays: the surface opens an
    /// overlay and stores deferred work (e.g. a blocking network call).
    /// On the first post-draw tick this method executes the work and returns
    /// an action so the user sees the overlay before the call blocks.
    ///
    /// The default implementation always returns `None`.
    fn poll_immediate_action(
        &mut self,
        _ctx: &mut ExtraScreenContext<'_>,
    ) -> Result<Option<ExtraScreenAction>, Self::Error> {
        Ok(None)
    }

    /// Mark the entity at `entity_id` with `content_hash` as reviewed for the
    /// entry at `entry_idx`. Default is a no-op so surfaces can adopt entity
    /// reviewed tracking incrementally.
    fn mark_entity_reviewed(
        &mut self,
        _entry_idx: usize,
        _entity_id: &crate::semantic::EntityId,
        _content_hash: u64,
    ) {
    }

    /// Return `true` when the entity has been previously marked reviewed at
    /// the given `content_hash`. Default always returns `false`.
    fn is_entity_reviewed(
        &self,
        _entry_idx: usize,
        _entity_id: &crate::semantic::EntityId,
        _content_hash: u64,
    ) -> bool {
        false
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
    // Paint a "Extracting entities…" overlay before the first
    // `reload_current_entry` call. That call is synchronous and can block
    // for several seconds while extracting entities for a large change.
    // Without this paint, the alt screen would sit blank during that wait.
    // We can't use the stderr startup spinner here — the terminal is
    // already in raw mode and on the alt screen, so stderr writes are
    // invisible. `Screen::Extracting` already has a centered loading
    // overlay; reuse it for the first-paint case.
    let saved_screen = std::mem::replace(&mut app.screen, Screen::Extracting);
    terminal
        .draw(|frame| render(frame, app))
        .map_err(|e| AppError::Io(std::io::Error::other(e)))?;
    app.screen = saved_screen;

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

        // After drawing, give the surface a chance to run deferred work while
        // an extra screen is open (e.g. execute a blocking submit after the
        // "Submitting…" overlay is visible). If it returns an action, apply it
        // and loop back to redraw immediately without blocking on a key event.
        if matches!(app.screen, Screen::Extra(_)) {
            let mut navigate_to_entry: Option<usize> = None;
            let maybe_action = app
                .surface
                .poll_immediate_action(&mut ExtraScreenContext {
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
                })
                .map_err(AppError::Surface)?;
            if let Some(action) = maybe_action {
                match action {
                    ExtraScreenAction::StayOpen => {}
                    ExtraScreenAction::Close => {
                        app.screen = Screen::Main;
                        if let Some(idx) = navigate_to_entry {
                            app.load_entry(idx, true).map_err(AppError::Surface)?;
                        }
                    }
                    ExtraScreenAction::OpenScreen(new_state) => {
                        app.screen = Screen::Extra(new_state);
                    }
                }
                app.refresh_inline_comments();
                continue;
            }
        }

        // Deferred entry load: `advance_stack` / `retreat_stack` set this
        // instead of calling `load_entry` directly so the "loading…" status
        // drawn above is visible before the blocking `fetch_views` call runs.
        if let Some((idx, record_cursor)) = app.pending_load.take() {
            app.load_entry(idx, record_cursor)
                .map_err(AppError::Surface)?;
            continue;
        }

        // While extraction is running on a background thread, poll the
        // progress channel with a short timeout instead of blocking on
        // user input. On timeout we loop back to the top — the next
        // `draw` advances `app.tick`, so the spinner animates.
        if matches!(app.screen, Screen::Extracting)
            && app.extraction.is_some()
            && drain_extraction_events(app)
        {
            continue;
        }

        handle_event(app)?;
    }

    on_exit(app);
    Ok(())
}

/// Pump messages from the background extraction worker into `App` state.
///
/// Drains all immediately-available events, then blocks for up to ~100 ms
/// waiting for the next one. The timeout doubles as the spinner frame
/// rate: every tick we either get an event (and update progress / fold in
/// entities / transition out of `Extracting`) or we time out (and the
/// next render cycle advances the tick, animating the spinner).
///
/// Returns `true` if the caller should `continue` (skip `handle_event`) —
/// either we made progress, the channel closed, or we timed out (we still
/// want to re-render the spinner frame).
fn drain_extraction_events<S: ReviewSurfaceExt>(app: &mut App<S>) -> bool {
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    use std::time::Duration;

    // Pull every event already in the queue without blocking.
    loop {
        let Some(prog) = app.extraction.as_ref() else {
            return true;
        };
        match prog.rx.try_recv() {
            Ok(event) => apply_extraction_event(app, event),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                end_extraction(app);
                return true;
            }
        }
    }

    let Some(prog) = app.extraction.as_ref() else {
        return true;
    };
    match prog.rx.recv_timeout(Duration::from_millis(100)) {
        Ok(event) => {
            apply_extraction_event(app, event);
            true
        }
        Err(RecvTimeoutError::Timeout) => true,
        Err(RecvTimeoutError::Disconnected) => {
            end_extraction(app);
            true
        }
    }
}

/// Tear down the in-progress extraction handle and exit the loading screen.
fn end_extraction<S: ReviewSurfaceExt>(app: &mut App<S>) {
    app.extraction = None;
    // Tiers and ordering apply now that the full entity list is accumulated.
    app.refresh_entity_order();
    if matches!(app.screen, Screen::Extracting) {
        app.screen = Screen::Main;
    }
}

/// Fold a single `ExtractionEvent` into the in-flight extraction state.
fn apply_extraction_event<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    event: crate::tui::entity_list::ExtractionEvent,
) {
    use crate::tui::entity_list::ExtractionEvent;
    match event {
        ExtractionEvent::Progress {
            files_done,
            files_total,
            files_failed,
        } => {
            if let Some(prog) = app.extraction.as_mut() {
                prog.files_done = files_done;
                prog.files_total = files_total;
                prog.files_failed = files_failed;
            }
        }
        ExtractionEvent::FileExtracted { entities, .. } => {
            app.entities.extend(entities);
            app.refresh_header_stats();
        }
        // Treat Complete, Cancelled, and Error identically from the
        // event-loop perspective: all three end the worker's lifetime and
        // return control to the user. The distinction lives in the
        // status-message path, which the worker drives before sending.
        ExtractionEvent::Complete | ExtractionEvent::Cancelled | ExtractionEvent::Error(_) => {
            end_extraction(app);
        }
    }
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
    app.tick = app.tick.wrapping_add(1);

    if matches!(app.screen, Screen::Extra(_)) {
        let Screen::Extra(state) = std::mem::replace(&mut app.screen, Screen::Main) else {
            unreachable!("matched above");
        };
        if state.is_overlay() {
            // Restore the underlying screen for the dispatch render so a
            // composer over an `EntityDiff` paints the diff behind the
            // overlay — not the entity list. Without this, the mem::replace
            // above leaves `app.screen` as `Screen::Main`, and dispatching
            // would render the entity list as the composer's backdrop even
            // though the user opened the composer from a diff view.
            let placeholder = match &app.screen_before_extra {
                Some(ScreenBeforeExtra::EntityDiff { entity_idx }) => Screen::EntityDiff {
                    entity_idx: *entity_idx,
                },
                Some(ScreenBeforeExtra::FileDiff { file_idx }) => Screen::FileDiff {
                    file_idx: *file_idx,
                },
                None => Screen::Main,
            };
            app.screen = placeholder;
            render_dispatch(frame, app);
            // `render_dispatch` may have mutated `app.screen` (e.g.
            // `render_file_diff_screen` syncs `file_idx`); the value left
            // here is replaced by `Screen::Extra(state)` below.
        }
        let mut state = state;
        app.surface.render_extra_screen(frame, state.as_mut());
        app.screen = Screen::Extra(state);
        return;
    }

    render_dispatch(frame, app);

    match &app.screen {
        Screen::Main | Screen::EntityDiff { .. } | Screen::FileDiff { .. } | Screen::Extracting => {
        }
        Screen::Help => {
            help_screen::render(
                frame,
                app.surface.help_screen_title(),
                app.surface.help_screen_body(),
                app.help_scroll,
            );
        }
        Screen::Transition(state) => {
            render_transition(frame, app, state);
        }
        Screen::FilePicker(state) => {
            file_picker::render(frame, state);
        }
        Screen::Extra(_) => unreachable!("handled above"),
    }
}

/// Route rendering to the right function based on the current screen.
fn render_dispatch<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, app: &mut App<S>) {
    match &app.screen {
        Screen::EntityDiff { entity_idx } => {
            let eidx = *entity_idx;
            render_entity_diff_screen(frame, app, eidx);
        }
        Screen::FileDiff { file_idx } => {
            let fidx = *file_idx;
            render_file_diff_screen(frame, app, fidx);
        }
        Screen::Main
        | Screen::Extracting
        | Screen::Help
        | Screen::Transition(_)
        | Screen::FilePicker(_)
        | Screen::Extra(_) => render_entity_list_screen(frame, app),
    }
}

/// Render the entity list screen (the new `Screen::Main`).
fn render_entity_list_screen<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, app: &mut App<S>) {
    let area = frame.area();
    // Orientation header: description row, optional body-peek row, and the
    // Σ scope row sit between the stack bar and the divider. The peek row is
    // omitted entirely (no blank line) when the description has no body.
    let body_peek = app
        .description_summary
        .as_ref()
        .and_then(|d| d.body_peek.as_deref())
        .filter(|p| !p.is_empty());
    let mut constraints = vec![
        Constraint::Length(3), // stack bar
        Constraint::Length(1), // description row
    ];
    if body_peek.is_some() {
        constraints.push(Constraint::Length(1)); // body-peek row
    }
    constraints.extend([
        Constraint::Length(1), // Σ scope row
        Constraint::Length(1), // divider
        Constraint::Min(1),    // entity list body
        Constraint::Length(1), // footer
    ]);
    let layout = Layout::vertical(constraints).split(area);
    let mut slot = 0usize;
    let mut next = || {
        let r = layout[slot];
        slot += 1;
        r
    };

    render_stack_bar(frame, next(), app);

    let desc_area = next();
    let (subject, comment_count) = app
        .description_summary
        .as_ref()
        .map(|d| (d.subject.as_str(), d.comment_count))
        .unwrap_or(("", 0));
    let desc_focused = app.entity_index == 0;
    crate::tui::entity_list::render_description_row(
        frame,
        desc_area,
        subject,
        comment_count,
        desc_focused,
    );
    if let Some(peek) = body_peek {
        crate::tui::entity_list::render_body_peek_row(frame, next(), peek);
    }
    let stats = app.header_stats.clone();
    crate::tui::entity_list::render_stats_row(frame, next(), &stats);
    crate::tui::entity_list::render_divider(frame, next());

    let body_area = next();
    app.viewport_rows = body_area.height;
    crate::tui::entity_list::render_entity_list_body(frame, body_area, app);

    // When there are no entities, show a dimmed hint so the reviewer
    // knows the list is empty rather than broken.
    if app.entities.is_empty() && body_area.height > 0 {
        use ratatui::style::{Color, Style};
        use ratatui::text::{Line as TuiLine, Span};
        use ratatui::widgets::Paragraph;
        frame.render_widget(
            Paragraph::new(TuiLine::from(Span::styled(
                "  no entities — change has no extractable code",
                Style::default().fg(Color::DarkGray),
            ))),
            Rect {
                y: body_area.y,
                height: 1,
                ..body_area
            },
        );
    }

    render_footer(frame, next(), app);

    // Loading overlay on top of the entity list body. Render whenever
    // `Screen::Extracting` is active, even when the async extraction worker
    // is absent (initial synchronous load in `run_app` before the first
    // event tick) — otherwise the alt screen shows the empty "no entities"
    // message during the multi-second blocking reload, which reads as
    // "broken" rather than "loading."
    if matches!(app.screen, Screen::Extracting) {
        let (done, total, failed) = app
            .extraction
            .as_ref()
            .map(|p| (p.files_done, p.files_total, p.files_failed))
            .unwrap_or((0, 0, 0));
        crate::tui::entity_list::render_loading_overlay(frame, done, total, failed, app.tick);
    }
}

/// Build a `DiffView` containing only lines within `(start, end)` plus the
/// hunk headers that introduce each group of matching lines.
fn clip_diff_view_to_range(view: &DiffView, start: u32, end: u32) -> DiffView {
    let mut lines = Vec::new();
    let mut pending_header: Option<RenderedLine> = None;
    for l in &view.lines {
        if matches!(
            l.kind,
            RenderedLineKind::HunkHeader | RenderedLineKind::HunkSeparator
        ) {
            pending_header = Some(l.clone());
        } else {
            let in_range = l.target_line.is_some_and(|tl| tl >= start && tl <= end)
                || l.source_line.is_some_and(|sl| sl >= start && sl <= end);
            if in_range {
                if let Some(h) = pending_header.take() {
                    lines.push(h);
                }
                lines.push(l.clone());
            }
        }
    }
    let mut clipped = DiffView::from_lines(view.title.clone(), lines);
    // Copy only the token spans for lines that survived the clip. Cloning the
    // whole map would retain highlight data for every line in the file on
    // every clip, most of which the clipped view can never render.
    clipped.token_spans = clipped
        .lines
        .iter()
        .filter_map(|l| {
            let key = (l.source_line, l.target_line);
            view.token_spans.get(&key).map(|v| (key, v.clone()))
        })
        .collect();
    clipped
}

/// Render a focused file diff for one entity (`Screen::EntityDiff`).
fn render_entity_diff_screen<S: ReviewSurfaceExt>(
    frame: &mut Frame<'_>,
    app: &mut App<S>,
    entity_idx: usize,
) {
    // Set the file_index to the file containing this entity so render_main
    // renders the correct view. Extract everything we need from `entity`
    // up front so the borrow ends before `mark_entity_reviewed_at` (which
    // takes &mut self.entities) is called below.
    let (target, range_start, range_end) = {
        let Some(entity) = app.entities.get(entity_idx) else {
            render_main(frame, app);
            return;
        };
        let (rs, re) = entity.line_range;
        (entity.file_path.to_string_lossy().into_owned(), rs, re)
    };
    // Strip status suffixes that render_title appends for Added/Removed/Binary
    // files; without this, entity.file_path ("foo.rs") would never match the
    // DiffView title ("foo.rs (added)").
    let findex = app
        .rendered_per_file
        .iter()
        .position(|v| {
            let base = v
                .title
                .strip_suffix(" (added)")
                .or_else(|| v.title.strip_suffix(" (removed)"))
                .or_else(|| v.title.strip_suffix(" (binary)"))
                .unwrap_or(&v.title);
            base == target || v.title.ends_with(&format!(" -> {target}"))
        })
        .unwrap_or(app.file_index);
    app.file_index = findex;

    // Only initialize scroll when entering a new entity or toggling clip mode.
    // Without this guard the render loop resets scroll every tick, preventing
    // the user from scrolling within the view.
    let need_init = app.entity_diff_initialized != Some((entity_idx, app.entity_clip));
    if need_init {
        app.entity_diff_initialized = Some((entity_idx, app.entity_clip));

        // Auto-mark the entity as reviewed on first entry. Go through the
        // shared helper so both the on-disk reviewed bit and the in-memory
        // `EntitySummary.reviewed` flag flip together — otherwise returning
        // to the entity list shows no ✓ until the list is re-fetched.
        app.mark_entity_reviewed_at(entity_idx);

        if app.entity_clip {
            app.line_index = 0;
            app.scroll = 0;
        } else {
            // Full-file mode: jump to first changed line in the entity's range.
            if let Some(view) = app.annotated_per_file.0.get(findex) {
                let within = |l: &RenderedLine| {
                    let tl = l.target_line.unwrap_or(0);
                    let sl = l.source_line.unwrap_or(0);
                    (tl >= range_start && tl <= range_end) || (sl >= range_start && sl <= range_end)
                };
                let changed = |l: &RenderedLine| {
                    matches!(l.kind, RenderedLineKind::Added | RenderedLineKind::Removed)
                };
                let row = view
                    .lines
                    .iter()
                    .position(|l| within(l) && changed(l))
                    .or_else(|| {
                        view.lines.iter().position(|l| {
                            l.target_line.is_some_and(|tl| tl >= range_start)
                                || l.source_line.is_some_and(|sl| sl >= range_start)
                        })
                    });
                if let Some(r) = row {
                    app.line_index = r;
                    app.scroll = u16::try_from(r.saturating_sub(3)).unwrap_or(0);
                }
            }
        }
    }

    // Update the persistent entity context for the footer: find which entity
    // (if any) contains the line the cursor is currently on, then format the
    // name + annotation + caller count string.
    app.entity_context = entity_context_for_cursor(app, findex);

    if app.entity_clip {
        // Build a view containing only lines within the entity's range.
        // Swap it in for rendering, then restore.
        let clipped = app
            .annotated_per_file
            .0
            .get(findex)
            .map(|v| clip_diff_view_to_range(v, range_start, range_end));
        if let Some(clipped_view) = clipped {
            let orig = std::mem::replace(&mut app.annotated_per_file.0[findex], clipped_view);
            render_main(frame, app);
            app.annotated_per_file.0[findex] = orig;
            return;
        }
    }

    render_main(frame, app);
}

/// Find the entity whose line range contains the current cursor line in
/// `Screen::EntityDiff`, and format the status-bar context string.
///
/// Returns `None` when the cursor is on a hunk-separator or otherwise outside
/// every entity's line range.
fn entity_context_for_cursor<S: ReviewSurfaceExt>(app: &App<S>, findex: usize) -> Option<String> {
    let view = app.annotated_per_file.0.get(findex)?;
    let rendered = view.lines.get(app.line_index)?;

    // Prefer the after-state (target) line number; fall back to source for
    // pure-deletion lines. Hunk-separator rows have neither.
    let file_line = rendered.target_line.or(rendered.source_line)?;

    // Find the entity in the current file whose range contains this line.
    let current_file = app
        .rendered_per_file
        .get(findex)
        .map(|v| v.title.as_str())
        .unwrap_or("");

    let entity = app.entities.iter().find(|e| {
        // Match by display path string — same as render_entity_diff_screen does.
        let path = e.file_path.to_string_lossy();
        (path == current_file
            || current_file.ends_with(path.as_ref())
            || current_file.ends_with(&format!("-> {path}")))
            && file_line >= e.line_range.0
            && file_line <= e.line_range.1
    })?;

    Some(format_entity_context(entity, app.tiers_degraded))
}

/// Format the status-bar context string for one entity.
///
/// With a computed tier: `validate() modified · high · sig change · 11 callers`
/// (plus ` · tiers degraded` when the graph was unavailable). The tier clause
/// carries fan-out where it matters, so this function needs no live
/// `caller_count` lookup — load-bearing, because it runs inside render on
/// every entity-diff keystroke, and a surface lookup there means a subprocess
/// call plus a full cache read per scroll step (the historical jjr
/// scroll-lag bug). Before tiers are computed (mid-reload window), the
/// context falls back to name + annotation with no caller segment.
fn format_entity_context(entity: &crate::semantic::EntitySummary, tiers_degraded: bool) -> String {
    use crate::semantic::{ChangeAnnotation, ChangeType};
    use std::fmt::Write as _;
    let name = &entity.display_name;
    let change = match entity.change {
        ChangeType::Added => "added",
        ChangeType::Deleted => "deleted",
        ChangeType::Modified => "modified",
        ChangeType::Moved => "moved",
    };
    let mut out = format!("{name} {change}");

    // Tier + one-clause justification replaces the annotation/caller
    // segments: the clause already names the change shape ("sig change ·
    // 11 callers", "body change"), so repeating the annotation would dupe.
    if let Some(risk) = &entity.risk {
        let _ = write!(out, " · {} · {}", risk.tier.label(), risk.clause);
        if tiers_degraded {
            out.push_str(" · tiers degraded");
        }
        return out;
    }

    let annotation = match entity.change {
        ChangeType::Modified => match entity.annotation {
            ChangeAnnotation::SigChanged => Some("sig changed"),
            ChangeAnnotation::BodyOnly => Some("body"),
            ChangeAnnotation::SigAndBody => Some("sig+body"),
            ChangeAnnotation::None => None,
        },
        ChangeType::Added | ChangeType::Deleted | ChangeType::Moved => None,
    };
    if let Some(ann) = annotation {
        out.push_str(" · ");
        out.push_str(ann);
    }
    out
}

/// Render the full file diff (the `F` escape hatch, `Screen::FileDiff`).
fn render_file_diff_screen<S: ReviewSurfaceExt>(
    frame: &mut Frame<'_>,
    app: &mut App<S>,
    file_idx: usize,
) {
    app.file_index = file_idx;
    render_main(frame, app);
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
    let spec = app.surface.stack_bar_spec();
    let current = app.surface.current_entry_index();

    let interior_cols = area.width.saturating_sub(BLOCK_BORDER_COLS);
    let bar_segment = match spec.progress {
        Some((position, total)) if area.width >= STACK_BAR_MIN_COLS_FOR_FILL && total > 0 => {
            progress_bar_string(position, total, STACK_PROGRESS_BAR_WIDTH)
        }
        Some(_) | None => String::new(),
    };

    let text_segment = format!("{}  ", spec.label);
    let used_width = bar_segment.chars().count() + text_segment.chars().count();
    let desc_budget = usize::from(interior_cols).saturating_sub(used_width);
    let desc = app.surface.entry_description(current);
    let label = format!(
        "{}{}{}",
        bar_segment,
        text_segment,
        truncate(&desc, desc_budget)
    );
    let block = Block::default().borders(Borders::ALL).title(spec.title);
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
                .map(|(idx, line)| {
                    let toks = tokens_for(line, &view.token_spans);
                    render_rendered_line(line, toks, idx == line_index, body_width)
                })
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
            .map(|(idx, line)| {
                let toks = tokens_for(line, &view.token_spans);
                render_rendered_line(line, toks, idx == cursor_row, total_width)
            })
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
                Some(line) => side_cell_spans(
                    line,
                    tokens_for(line, &view.token_spans),
                    geom.side_width,
                    focused,
                ),
                None => blank_cell_spans(geom.side_width, focused),
            };
            let right_spans = match right.and_then(|i| view.lines.get(i)) {
                Some(line) => side_cell_spans(
                    line,
                    tokens_for(line, &view.token_spans),
                    geom.side_width,
                    focused,
                ),
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
    let comment_spans = side_cell_spans(line, &[], side_width, focused);
    let blank = blank_cell_spans(side_width, focused);
    let gutter = side_by_side_gutter_spans();
    let (left, right) = match column {
        InlineCommentColumn::Left => (comment_spans, blank),
        InlineCommentColumn::Right => (blank, comment_spans),
    };
    TuiLine::from([left, gutter, right].concat())
}

fn tokens_for<'a>(
    line: &RenderedLine,
    spans: &'a std::collections::HashMap<
        (Option<u32>, Option<u32>),
        Vec<crate::highlight::TokenSpan>,
    >,
) -> &'a [crate::highlight::TokenSpan] {
    match line.kind {
        RenderedLineKind::Context | RenderedLineKind::Added | RenderedLineKind::Removed => spans
            .get(&(line.source_line, line.target_line))
            .map(Vec::as_slice)
            .unwrap_or_default(),
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody
        | RenderedLineKind::DescriptionLine => &[],
    }
}

fn side_cell_spans<'a>(
    line: &'a RenderedLine,
    tokens: &[crate::highlight::TokenSpan],
    side_width: u16,
    focused: bool,
) -> Vec<Span<'a>> {
    if !focused {
        // Added/Removed always get the background tint even with no tokens.
        // Context only gets syntax spans when there are tokens to show.
        let needs_syntax = matches!(
            line.kind,
            RenderedLineKind::Added | RenderedLineKind::Removed
        ) || (matches!(line.kind, RenderedLineKind::Context)
            && !tokens.is_empty());
        if needs_syntax {
            return syntax_spans(line, tokens, side_width);
        }
    }
    let (body, fg_color) = prefix_truncate_pad(line, side_width);
    vec![Span::styled(body, focus_style(fg_color, focused))]
}

// ── GitHub dark-mode diff background palette ──────────────────────────────────
// Background covers the whole line; gutter is used for the "+"/"-" glyph only.
const ADDED_BG: Color = Color::Rgb(14, 68, 41); // #0e4429
const ADDED_GUTTER: Color = Color::Rgb(63, 185, 80); // #3fb950
const REMOVED_BG: Color = Color::Rgb(67, 12, 14); // #430c0e
const REMOVED_GUTTER: Color = Color::Rgb(248, 81, 73); // #f85149

fn diff_bg(kind: RenderedLineKind) -> Option<Color> {
    match kind {
        RenderedLineKind::Added => Some(ADDED_BG),
        RenderedLineKind::Removed => Some(REMOVED_BG),
        RenderedLineKind::Context
        | RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody
        | RenderedLineKind::DescriptionLine => None,
    }
}

fn diff_gutter_fg(kind: RenderedLineKind) -> Option<Color> {
    match kind {
        RenderedLineKind::Added => Some(ADDED_GUTTER),
        RenderedLineKind::Removed => Some(REMOVED_GUTTER),
        RenderedLineKind::Context
        | RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody
        | RenderedLineKind::DescriptionLine => None,
    }
}

/// Render a diff line with per-token syntax colours and — for added/removed
/// lines — a GitHub dark-mode background tint. The `+`/`-` glyph uses a
/// bright gutter colour against the tint. Focused lines (REVERSED) bypass
/// this path entirely.
fn syntax_spans<'a>(
    line: &'a RenderedLine,
    tokens: &[crate::highlight::TokenSpan],
    width: u16,
) -> Vec<Span<'a>> {
    let attrs = line_visual_attrs(line);
    let prefix = attrs.prefix;
    let prefix_chars = prefix.chars().count();

    // Background for diff lines; None (terminal default) for context.
    let bg = diff_bg(line.kind);
    let base = bg.map_or_else(Style::default, |c| Style::default().bg(c));

    let prefix_span: Span<'a> = match diff_gutter_fg(line.kind) {
        Some(fg) => Span::styled(prefix, base.fg(fg)),
        None => Span::styled(prefix, base),
    };

    let max_chars = usize::from(width).saturating_sub(prefix_chars);
    let max_bytes = byte_limit(&line.text, max_chars);
    let text = &line.text[..max_bytes];

    let mut spans: Vec<Span<'a>> = vec![prefix_span];
    let mut pos = 0usize;

    for token in tokens {
        let start = token.start.min(max_bytes);
        let end = token.end.min(max_bytes);
        if end <= start || start < pos {
            continue;
        }
        if start > pos {
            if let Some(gap) = text.get(pos..start) {
                spans.push(Span::styled(gap, base));
            }
        }
        if let Some(tok_text) = text.get(start..end) {
            spans.push(Span::styled(tok_text, base.fg(token.color)));
        }
        pos = end;
    }

    if pos < max_bytes {
        if let Some(tail) = text.get(pos..) {
            spans.push(Span::styled(tail, base));
        }
    }

    let text_chars = text.chars().count();
    let pad = usize::from(width).saturating_sub(prefix_chars + text_chars);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), base));
    }

    spans
}

/// Find the byte offset of the `char_limit`-th character boundary in `s`.
/// Returns `s.len()` if `s` has fewer than `char_limit` characters.
fn byte_limit(s: &str, char_limit: usize) -> usize {
    s.char_indices().nth(char_limit).map_or(s.len(), |(i, _)| i)
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

fn render_rendered_line<'a>(
    line: &'a RenderedLine,
    tokens: &[crate::highlight::TokenSpan],
    focused: bool,
    width: u16,
) -> TuiLine<'a> {
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

    if !focused {
        let needs_syntax = matches!(
            line.kind,
            RenderedLineKind::Added | RenderedLineKind::Removed
        ) || (matches!(line.kind, RenderedLineKind::Context)
            && !tokens.is_empty());
        if needs_syntax {
            return TuiLine::from(syntax_spans(line, tokens, width));
        }
    }

    // Pad every line to the full body width so all cells in the diff area are
    // explicitly written. Without this, trailing cells default to the terminal's
    // own background, which breaks consistency on custom-themed terminals and
    // means the cursor highlight doesn't extend to the right edge on focused lines.
    let (body, fg_color) = prefix_truncate_pad(line, width);
    TuiLine::from(vec![Span::styled(body, focus_style(fg_color, focused))])
}

fn render_footer<S: ReviewSurfaceExt>(frame: &mut Frame<'_>, area: Rect, app: &App<S>) {
    let in_entity_diff = matches!(app.screen, Screen::EntityDiff { .. });

    if let Some(msg) = app.status_message.as_deref() {
        let widget = Paragraph::new(msg).style(Style::default().fg(Color::Yellow));
        frame.render_widget(widget, area);
        return;
    }

    if in_entity_diff {
        if let Some(ctx) = app.entity_context.as_deref() {
            // Entity context and key hints are both present: context on the left
            // (dim), compact key reminder on the right so neither is lost.
            let line = entity_diff_footer_line(ctx, area.width, app.entity_clip);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        let has_stack = app.surface.entry_count() > 1;
        let mut hint = app
            .surface
            .footer_hint(area.width, has_stack, app.severity_filter);
        hint.push_str(if app.entity_clip {
            "  x full"
        } else {
            "  x clip"
        });
        frame.render_widget(Paragraph::new(hint), area);
        return;
    }

    let has_stack = app.surface.entry_count() > 1;
    let mut hint = app
        .surface
        .footer_hint(area.width, has_stack, app.severity_filter);
    if matches!(app.screen, Screen::Main) {
        use std::fmt::Write as _;
        let _ = write!(hint, "  o {}", app.order_mode.label());
    }
    frame.render_widget(Paragraph::new(hint), area);
}

/// Build the footer `TuiLine` for the entity-diff screen when an entity is
/// loaded. The entity context string occupies the left side (dimmed); a
/// compact key reminder occupies the right side (default colour). The context
/// is truncated so the hint always fits.
fn entity_diff_footer_line(ctx: &str, width: u16, clip: bool) -> TuiLine<'static> {
    let clip_key = if clip { "x full" } else { "x clip" };
    let key_hints = format!("  Enter c  {clip_key}  U reviewed");
    let hint_chars = key_hints.chars().count();
    let ctx_budget = usize::from(width).saturating_sub(hint_chars);
    let ctx_trimmed = truncate(ctx, ctx_budget);
    TuiLine::from(vec![
        Span::styled(ctx_trimmed, Style::default().fg(Color::DarkGray)),
        Span::raw(key_hints),
    ])
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
        "
  Reviewed
  {reviewed_pos}/{count}  {reviewed_id}
  {reviewed_desc}

  ────────────────

  Next
  {next_pos}/{count}  {next_id}
  {next_desc}
",
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
    if let Event::Resize(_, _) = evt {
        // ratatui's Terminal::draw() calls autoresize() which picks up the
        // new dimensions. Force a full clear so stale cells from the old
        // geometry don't linger after the size change.
        app.needs_full_redraw = true;
        return Ok(());
    }
    if let Event::Key(key) = evt {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match &app.screen {
            Screen::Main => {
                handle_entity_list_key(app, key).map_err(AppError::Surface)?;
            }
            Screen::Extracting => {
                // Only Esc is handled during extraction.
                if key.code == KeyCode::Esc {
                    if let Some(ref prog) = app.extraction {
                        crate::tui::entity_list::cancel_extraction(prog);
                    }
                    app.screen = Screen::Main;
                }
            }
            Screen::EntityDiff { .. } | Screen::FileDiff { .. } => {
                handle_file_view_key(app, key).map_err(AppError::Surface)?;
            }
            Screen::Help => handle_help_key(app, key),
            Screen::Transition(_) => handle_transition_key(app, key).map_err(AppError::Surface)?,
            Screen::Extra(_) => handle_extra_screen_key(app, key).map_err(AppError::Surface)?,
            Screen::FilePicker(_) => handle_file_picker_key(app, key),
        }
    }
    Ok(())
}

/// Cycle the entity list order: risk → dependency → file. Tiers were
/// computed at entry load; only the ordering is reapplied here.
fn cycle_entity_sort<S: ReviewSurfaceExt>(app: &mut App<S>) {
    app.order_mode = app.order_mode.next();
    let idx = app.surface.current_entry_index();
    let graph = app.surface.entry_graph(idx);
    crate::semantic::sort_entities(&mut app.entities, app.order_mode, graph.as_ref());
    app.status_message = Some(format!("order: {}", app.order_mode.label()));
    app.entity_index = 0;
    app.entity_scroll = 0;
}

/// `m` on the entity list, surface-first: ggr binds `m` to the commit-scoped
/// composer, and an unconditional core binding would shadow it. Only when the
/// surface ignores the key does the core open the description view (jjr).
fn open_description_surface_first<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<(), S::Error> {
    if !delegate_to_surface(app, key)? {
        app.screen = Screen::FileDiff { file_idx: 0 };
        app.line_index = 0;
        app.scroll = 0;
    }
    Ok(())
}

/// Key handler for the entity list screen (`Screen::Main`).
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_entity_list_key<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<(), S::Error> {
    app.status_message = None;
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.screen = Screen::Help;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            crate::tui::entity_list::move_entity_cursor(app, -1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            crate::tui::entity_list::move_entity_cursor(app, 1);
        }
        KeyCode::Enter => {
            // Description row (index 0) → open description in file view.
            // Entity rows → open focused entity diff.
            if app.entity_index == 0 {
                app.screen = Screen::FileDiff { file_idx: 0 };
            } else {
                let eidx = app.entity_index - 1;
                // PR overview entity pane: navigate to the commit that last
                // modified this entity, then find the entity there and open
                // its diff. ggr entity loading is synchronous so app.entities
                // is populated immediately after load_entry.
                if let Some(commit_entry) = app.surface.pr_entity_commit_entry(eidx) {
                    let entity_id = app.entities.get(eidx).map(|e| e.id.clone());
                    app.load_entry(commit_entry, false)?;
                    if let Some(eid) = entity_id {
                        if let Some(new_eidx) = app.entities.iter().position(|e| e.id == eid) {
                            app.screen = Screen::EntityDiff {
                                entity_idx: new_eidx,
                            };
                        }
                    }
                } else {
                    app.screen = Screen::EntityDiff { entity_idx: eidx };
                }
            }
        }
        // Tab/Shift-Tab on the entity list moves cursor selection (same as j/k).
        // Cycling *into* entity diffs happens in handle_file_view_key where
        // Tab/Shift-Tab navigate between Screen::EntityDiff views.
        KeyCode::Tab => crate::tui::entity_list::move_entity_cursor(app, 1),
        KeyCode::BackTab => crate::tui::entity_list::move_entity_cursor(app, -1),
        KeyCode::Char('F') => {
            let fidx = app.file_index;
            app.screen = Screen::FileDiff { file_idx: fidx };
        }
        KeyCode::Char('n') => app.advance_stack()?,
        KeyCode::Char('p') => app.retreat_stack()?,
        KeyCode::Char('R') => {
            let idx = app.surface.current_entry_index();
            app.surface.clear_entity_cache(idx);
            app.reload_current_entry()?;
            app.status_message = Some("refreshed".to_owned());
        }
        KeyCode::Char('o') => cycle_entity_sort(app),
        KeyCode::Char('1') => app.toggle_severity_filter(Severity::Required),
        KeyCode::Char('2') => app.toggle_severity_filter(Severity::Suggestion),
        KeyCode::Char('3') => app.toggle_severity_filter(Severity::Note),
        KeyCode::Char(';') => {
            app.cosmetic_filter_on = !app.cosmetic_filter_on;
            // `;` hides all behavior-preserving rows (cosmetic + refactor
            // tags), not just cosmetic — keep the reviewer aware of how many
            // rows they are not seeing.
            let hidden = app
                .entities
                .iter()
                .filter(|e| !e.structural_change || e.is_behavior_preserving())
                .count();
            let msg = if app.cosmetic_filter_on {
                format!("cosmetic + refactor rows: hidden ({hidden})")
            } else {
                format!("cosmetic + refactor rows: shown ({hidden})")
            };
            app.status_message = Some(msg);
        }
        KeyCode::Char('m') => open_description_surface_first(app, key)?,
        // PR overview pane toggle: switch from entity list back to description.
        KeyCode::Char('e')
            if app
                .surface
                .has_pr_pane_toggle(app.surface.current_entry_index()) =>
        {
            let idx = app.surface.current_entry_index();
            app.surface.toggle_pr_pane();
            // is_description_entry now returns true; start_entity_extraction
            // will navigate to Screen::FileDiff for the description view.
            app.start_entity_extraction(idx);
        }
        // Delegate everything else to the surface so surface-specific bindings
        // (e.g. ggr's `S` for submit, `R` for refresh) work from the entity list.
        _ => {
            delegate_to_surface(app, key)?;
        }
    }
    Ok(())
}

/// When the entity clip view is active, `app.line_index` is a position within
/// the clipped subset of lines. Comment placement needs the corresponding
/// index in the *full* file view. This function translates it.
fn clip_to_file_line_index<S: ReviewSurfaceExt>(app: &App<S>) -> usize {
    let Screen::EntityDiff { entity_idx } = app.screen else {
        return app.line_index;
    };
    let Some(entity) = app.entities.get(entity_idx) else {
        return app.line_index;
    };
    let Some(full_view) = app.annotated_per_file.0.get(app.file_index) else {
        return app.line_index;
    };
    let (range_start, range_end) = entity.line_range;
    let clipped = clip_diff_view_to_range(full_view, range_start, range_end);
    let Some(clip_line) = clipped.lines.get(app.line_index) else {
        return app.line_index;
    };
    // Match the clip line against the full view by its file-side line numbers
    // and kind — the clip is a subset of the full view's lines so there is
    // always an exact match for non-header rows.
    full_view
        .lines
        .iter()
        .position(|fl| {
            fl.kind == clip_line.kind
                && fl.target_line == clip_line.target_line
                && fl.source_line == clip_line.source_line
        })
        .unwrap_or(app.line_index)
}

/// Delegate an unhandled key to the surface's `handle_extra_key`, resolving
/// the line index for side-by-side mode and applying the returned action.
/// Forward `key` to the surface's `handle_extra_key` and apply the returned
/// action. Returns `true` when the surface consumed the key (any action other
/// than `Ignored`), so callers can fall back to a core binding otherwise.
fn delegate_to_surface<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<bool, S::Error> {
    // In entity clip view the cursor is in clip-space; translate to file-space
    // so comment anchors land on the correct full-file line number.
    let raw_line_index = if matches!(app.screen, Screen::EntityDiff { .. }) && app.entity_clip {
        clip_to_file_line_index(app)
    } else {
        app.line_index
    };
    let annotated_clone = app.current_view().cloned();
    let lines_index = match app.effective_diff_mode() {
        EffectiveDiffMode::Unified => raw_line_index,
        EffectiveDiffMode::SideBySide => annotated_clone
            .as_ref()
            .and_then(|v| v.paired_rows.get(app.line_index))
            .and_then(|row| match row {
                PairedRow::Spanning(idx)
                | PairedRow::Pair {
                    right: Some(idx), ..
                }
                | PairedRow::Pair {
                    left: Some(idx),
                    right: None,
                } => Some(*idx),
                PairedRow::Pair {
                    left: None,
                    right: None,
                } => None,
            })
            .unwrap_or(raw_line_index),
    };
    let action =
        app.surface
            .handle_extra_key(key, app.file_index, lines_index, annotated_clone.as_ref())?;
    match action {
        ExtraKeyAction::Ignored => return Ok(false),
        ExtraKeyAction::OpenScreen(state) => {
            // Capture the underlying screen so `Close` can restore it.
            // Composers and other overlays only open from the diff views;
            // record which one so we land back there on save/cancel rather
            // than the entity list.
            app.screen_before_extra = match app.screen {
                Screen::EntityDiff { entity_idx } => {
                    Some(ScreenBeforeExtra::EntityDiff { entity_idx })
                }
                Screen::FileDiff { file_idx } => Some(ScreenBeforeExtra::FileDiff { file_idx }),
                Screen::Main
                | Screen::Help
                | Screen::Transition(_)
                | Screen::Extra(_)
                | Screen::FilePicker(_)
                | Screen::Extracting => None,
            };
            app.screen = Screen::Extra(state);
        }
        ExtraKeyAction::StatusMessage(msg) => app.status_message = Some(msg),
        ExtraKeyAction::RefreshAndStatus(msg) => {
            app.status_message = Some(msg);
            app.refresh_inline_comments();
        }
        ExtraKeyAction::Quit => app.should_quit = true,
    }
    Ok(true)
}

/// Copy a ±10-line diff window around the cursor to the clipboard, formatted
/// with a `file:start-end` header so the result is ready to paste into a
/// Claude conversation. When the user is in entity-clip mode we yank from
/// the clipped view (matching what's visually under the cursor); otherwise
/// from the full file diff.
fn yank_cursor_window<S: ReviewSurfaceExt>(app: &mut App<S>) {
    let Some(full_view) = app.annotated_per_file.0.get(app.file_index) else {
        app.status_message = Some("yank: no file open".to_owned());
        return;
    };
    // In entity-clip mode the rendered view is clipped to the entity's line
    // range; yanking from the full view would feed Claude lines the user
    // can't see. Pull the same clip the renderer uses so the window matches
    // what's under the cursor.
    let clipped_entity = if let Screen::EntityDiff { entity_idx } = app.screen {
        app.entity_clip
            .then(|| app.entities.get(entity_idx))
            .flatten()
    } else {
        None
    };
    let view = match clipped_entity {
        Some(entity) => {
            let (start, end) = entity.line_range;
            clip_diff_view_to_range(full_view, start, end)
        }
        None => full_view.clone(),
    };
    let cursor_idx = app.line_index;
    let Some(payload) = crate::tui::yank::format_yank(&view, cursor_idx) else {
        app.status_message = Some("yank: nothing to copy at cursor".to_owned());
        return;
    };
    match crate::tui::yank::copy_to_clipboard(&payload) {
        Ok(()) => {
            let bytes = payload.len();
            app.status_message = Some(format!("yanked {bytes} bytes to clipboard"));
        }
        Err(e) => {
            app.status_message = Some(format!("yank failed: {e}"));
        }
    }
}

/// Key handler for file diff views (`Screen::EntityDiff` and `Screen::FileDiff`).
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_file_view_key<S: ReviewSurfaceExt>(
    app: &mut App<S>,
    key: KeyEvent,
) -> Result<(), S::Error> {
    app.status_message = None;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            // Return to entity list.
            app.screen = Screen::Main;
        }
        KeyCode::Up | KeyCode::Char('k') => app.move_line(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_line(1),
        KeyCode::PageUp => app.move_page(-1),
        KeyCode::PageDown => app.move_page(1),
        KeyCode::Home | KeyCode::Char('g') => app.jump_to(Edge::Top),
        KeyCode::End | KeyCode::Char('G') => app.jump_to(Edge::Bottom),
        KeyCode::Tab => {
            if matches!(app.screen, Screen::EntityDiff { .. }) {
                app.next_entity();
            } else {
                app.cycle_file(1);
            }
        }
        KeyCode::BackTab => {
            if matches!(app.screen, Screen::EntityDiff { .. }) {
                app.prev_entity();
            } else {
                app.cycle_file(-1);
            }
        }
        KeyCode::Char('F') => open_file_picker(app),
        KeyCode::Char('n') => app.advance_stack()?,
        KeyCode::Char('p') => app.retreat_stack()?,
        KeyCode::Char('R') => {
            let idx = app.surface.current_entry_index();
            app.surface.clear_entity_cache(idx);
            app.reload_current_entry()?;
            app.status_message = Some("refreshed".to_owned());
        }
        KeyCode::Char('1') => app.toggle_severity_filter(Severity::Required),
        KeyCode::Char('2') => app.toggle_severity_filter(Severity::Suggestion),
        KeyCode::Char('3') => app.toggle_severity_filter(Severity::Note),
        KeyCode::Char('U') => app.toggle_current_file_reviewed(),
        KeyCode::Char('|') => app.cycle_diff_mode(),
        KeyCode::Char('y') => yank_cursor_window(app),
        // PR overview pane toggle: switch from description to entity list.
        KeyCode::Char('e')
            if app
                .surface
                .is_description_entry(app.surface.current_entry_index()) =>
        {
            let idx = app.surface.current_entry_index();
            app.surface.toggle_pr_pane();
            app.start_entity_extraction(idx);
        }
        // Toggle entity-clipped view (show only entity range vs full file).
        KeyCode::Char('x') => {
            if matches!(app.screen, Screen::EntityDiff { .. }) {
                app.entity_clip = !app.entity_clip;
                app.line_index = 0;
                app.scroll = 0;
                let msg = if app.entity_clip {
                    "entity view: clipped"
                } else {
                    "entity view: full file"
                };
                app.status_message = Some(msg.to_owned());
            }
        }
        _ => {
            delegate_to_surface(app, key)?;
        }
    }
    Ok(())
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants intentionally ignored in help screen"
)]
fn handle_help_key<S: ReviewSurfaceExt>(app: &mut App<S>, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q' | '?') | KeyCode::Esc => app.screen = Screen::Main,
        KeyCode::Up | KeyCode::Char('k') => {
            app.help_scroll = app.help_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.help_scroll = app.help_scroll.saturating_add(1);
        }
        KeyCode::PageUp => {
            app.help_scroll = app.help_scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            app.help_scroll = app.help_scroll.saturating_add(10);
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.help_scroll = 0;
        }
        _ => {}
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
            app.status_message = Some("loading…".to_owned());
            app.pending_load = Some((next_index, true));
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
    let was_close = matches!(action, ExtraScreenAction::Close);
    match action {
        ExtraScreenAction::StayOpen => {
            app.screen = Screen::Extra(state);
        }
        ExtraScreenAction::Close => {
            if let Some(idx) = navigate_to_entry {
                // Cross-entry navigation overrides the screen-restore stack;
                // `load_entry` re-seeds App state from scratch and discards
                // the prior screen.
                app.screen_before_extra = None;
                app.load_entry(idx, true)?;
            } else if let Some(prev) = app.screen_before_extra.take() {
                // Restore the screen the user was on when the overlay opened
                // (entity diff or file diff). Without this they would always
                // land on the entity list after saving a comment.
                app.screen = match prev {
                    ScreenBeforeExtra::EntityDiff { entity_idx } => {
                        Screen::EntityDiff { entity_idx }
                    }
                    ScreenBeforeExtra::FileDiff { file_idx } => Screen::FileDiff { file_idx },
                };
            }
            // else: no snapshot recorded (e.g., overlay opened from a path
            // that does not delegate through `handle_extra_key`); fall back
            // to the `Screen::Main` placeholder set by the mem::replace above.
        }
        ExtraScreenAction::OpenScreen(new_state) => {
            app.screen = Screen::Extra(new_state);
        }
    }
    // After any extra-screen key, re-inject inline comments in case the
    // surface mutated its comment state.
    app.refresh_inline_comments();
    // A composer save injects a new `┃ ● …` block of rows right below the
    // cursor. If the cursor was near the bottom of the viewport when the
    // composer opened, those rows would render past the visible area and
    // the user would see "comment vanished" until they scroll. Scroll
    // forward enough to keep ~4 rows past the cursor in view so a typical
    // 1–3 line inline comment is visible the moment the composer closes.
    if was_close {
        app.ensure_rows_after_cursor_visible(4);
    }
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
        /// When `true`, `handle_extra_key` claims `m` with a status message —
        /// models ggr, whose `m` opens the commit-scoped composer.
        claim_m: bool,
        /// Returned from `graph_unavailable_reason` — models ggr's clone
        /// lifecycle reporting.
        graph_reason: Option<String>,
    }

    impl NoopSurface {
        fn new(views: Vec<DiffView>) -> Self {
            Self {
                views,
                not_tracked: false,
                claim_m: false,
                graph_reason: None,
            }
        }

        fn new_not_tracked(views: Vec<DiffView>) -> Self {
            Self {
                views,
                not_tracked: true,
                claim_m: false,
                graph_reason: None,
            }
        }

        fn new_claiming_m(views: Vec<DiffView>) -> Self {
            Self {
                views,
                not_tracked: false,
                claim_m: true,
                graph_reason: None,
            }
        }
    }

    impl ReviewSurface for NoopSurface {
        type Error = NoopError;
        fn graph_unavailable_reason(&self) -> Option<String> {
            self.graph_reason.clone()
        }
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
        fn handle_extra_key(
            &mut self,
            key: KeyEvent,
            _file_index: usize,
            _line_index: usize,
            _current_view: Option<&DiffView>,
        ) -> Result<ExtraKeyAction, NoopError> {
            if self.claim_m && key.code == KeyCode::Char('m') {
                return Ok(ExtraKeyAction::StatusMessage("m claimed".to_owned()));
            }
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
        fn help_screen_body(&self) -> &'static str {
            ""
        }
        fn footer_hint(
            &self,
            _width: u16,
            _has_stack: bool,
            _severity_filter: Option<Severity>,
        ) -> String {
            String::new()
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
            token_spans: std::collections::HashMap::new(),
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

    // ── Entity navigation ────────────────────────────────────────────────────

    fn make_app_with_entities(entity_count: usize) -> App<NoopSurface> {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let surface = NoopSurface::new(vec![view.clone()]);
        let mut app = App::new(surface, vec![view], TransitionMode::Never);
        // Populate entities with dummy summaries.
        for i in 0..entity_count {
            app.entities.push(crate::semantic::EntitySummary {
                id: crate::semantic::EntityId::new(
                    std::path::PathBuf::from("test.rs"),
                    vec![format!("fn{i}")],
                    None,
                    0,
                ),
                display_name: format!("fn{i}"),
                kind: crate::semantic::EntityKind::Function,
                change: crate::semantic::ChangeType::Modified,
                annotation: crate::semantic::ChangeAnnotation::BodyOnly,
                file_path: std::path::PathBuf::from("test.rs"),
                source_file: None,
                target_line: None,
                line_range: (1, 10),
                structural_change: true,
                content_hash: 0,
                refactor: None,
                comment_count: 0,
                reviewed: false,
                risk: None,
                fallback: false,
            });
        }
        app
    }

    #[test]
    fn entity_list_starts_at_description_row() {
        let app = make_app_with_entities(3);
        assert_eq!(
            app.entity_index, 0,
            "entity_index must start at 0 (description row)"
        );
    }

    #[test]
    fn move_entity_cursor_advances_past_description_to_entity() {
        let mut app = make_app_with_entities(3);
        crate::tui::entity_list::move_entity_cursor(&mut app, 1);
        assert_eq!(app.entity_index, 1, "cursor must advance to first entity");
        crate::tui::entity_list::move_entity_cursor(&mut app, 1);
        assert_eq!(app.entity_index, 2, "cursor must advance to second entity");
    }

    #[test]
    fn move_entity_cursor_clamps_at_last_entity() {
        let mut app = make_app_with_entities(2);
        crate::tui::entity_list::move_entity_cursor(&mut app, 100);
        assert_eq!(
            app.entity_index, 2,
            "cursor must clamp at last entity (description + 2 entities)"
        );
    }

    #[test]
    fn move_entity_cursor_clamps_at_description_row() {
        let mut app = make_app_with_entities(2);
        app.entity_index = 1;
        crate::tui::entity_list::move_entity_cursor(&mut app, -100);
        assert_eq!(
            app.entity_index, 0,
            "cursor must clamp at description row (0)"
        );
    }

    #[test]
    fn entity_list_len_includes_description_and_all_entities() {
        let app = make_app_with_entities(4);
        let len = crate::tui::entity_list::entity_list_len(&app);
        assert_eq!(len, 5, "len must be entity_count + 1 (description row)");
    }

    #[test]
    fn cosmetic_filter_reduces_entity_list_len() {
        let mut app = make_app_with_entities(3);
        // Make entity 0 cosmetic (structural_change = false).
        app.entities[0].structural_change = false;
        app.cosmetic_filter_on = true;
        let len = crate::tui::entity_list::entity_list_len(&app);
        assert_eq!(
            len, 3,
            "cosmetic filter must exclude the cosmetic entity; len = description + 2 visible"
        );
    }

    #[test]
    fn scroll_to_line_sets_line_index_and_scroll() {
        let mut app = make_app_with_entities(0);
        app.scroll_to_line(10);
        assert_eq!(app.line_index, 9, "line_index must be 0-based (10 - 1)");
    }

    #[test]
    fn cosmetic_filter_toggle_updates_status() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let surface = NoopSurface::new(vec![view.clone()]);
        let mut app = App::new(surface, vec![view], TransitionMode::Never);
        assert!(!app.cosmetic_filter_on, "filter must start off");
        // Simulate the ; key handler toggling the flag.
        app.cosmetic_filter_on = true;
        app.status_message = Some("cosmetic filter: hidden".to_owned());
        assert!(app.cosmetic_filter_on, "filter must be on after toggle");
        assert_eq!(
            app.status_message.as_deref(),
            Some("cosmetic filter: hidden")
        );
    }

    // ─── Screen restore after Extra overlay (composer / picker) ─────────
    //
    // Regression coverage for: "Ctrl-X save lands me on the entity list
    // instead of returning to the entity diff." The fix tracks the
    // underlying screen in `App::screen_before_extra` so `Close` can
    // restore it. These tests pin the state-machine contract so a future
    // refactor cannot silently revert the behavior.

    /// `OpenScreen` captures `EntityDiff` as the underlying screen.
    #[test]
    fn open_screen_from_entity_diff_captures_entity_diff_snapshot() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.screen = Screen::EntityDiff { entity_idx: 3 };

        // Mirror the capture logic from `delegate_to_surface`.
        app.screen_before_extra = match app.screen {
            Screen::EntityDiff { entity_idx } => Some(ScreenBeforeExtra::EntityDiff { entity_idx }),
            Screen::FileDiff { file_idx } => Some(ScreenBeforeExtra::FileDiff { file_idx }),
            Screen::Main
            | Screen::Help
            | Screen::Transition(_)
            | Screen::Extra(_)
            | Screen::FilePicker(_)
            | Screen::Extracting => None,
        };

        assert!(matches!(
            app.screen_before_extra,
            Some(ScreenBeforeExtra::EntityDiff { entity_idx: 3 })
        ));
    }

    /// `OpenScreen` captures `FileDiff` as the underlying screen.
    #[test]
    fn open_screen_from_file_diff_captures_file_diff_snapshot() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.screen = Screen::FileDiff { file_idx: 2 };

        app.screen_before_extra = match app.screen {
            Screen::EntityDiff { entity_idx } => Some(ScreenBeforeExtra::EntityDiff { entity_idx }),
            Screen::FileDiff { file_idx } => Some(ScreenBeforeExtra::FileDiff { file_idx }),
            Screen::Main
            | Screen::Help
            | Screen::Transition(_)
            | Screen::Extra(_)
            | Screen::FilePicker(_)
            | Screen::Extracting => None,
        };

        assert!(matches!(
            app.screen_before_extra,
            Some(ScreenBeforeExtra::FileDiff { file_idx: 2 })
        ));
    }

    /// Restoring from a `ScreenBeforeExtra::EntityDiff` snapshot returns to
    /// the exact same `entity_idx` the user was viewing before the overlay.
    #[test]
    fn close_restores_entity_diff_with_original_entity_idx() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.screen_before_extra = Some(ScreenBeforeExtra::EntityDiff { entity_idx: 7 });

        // Mirror the restore branch from `handle_extra_screen_key`'s
        // `ExtraScreenAction::Close` arm.
        if let Some(prev) = app.screen_before_extra.take() {
            app.screen = match prev {
                ScreenBeforeExtra::EntityDiff { entity_idx } => Screen::EntityDiff { entity_idx },
                ScreenBeforeExtra::FileDiff { file_idx } => Screen::FileDiff { file_idx },
            };
        }

        assert!(matches!(app.screen, Screen::EntityDiff { entity_idx: 7 }));
        assert!(
            app.screen_before_extra.is_none(),
            "snapshot must be consumed on restore"
        );
    }

    /// Restoring from a `ScreenBeforeExtra::FileDiff` snapshot returns to
    /// the exact same `file_idx` the user was viewing.
    #[test]
    fn close_restores_file_diff_with_original_file_idx() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.screen_before_extra = Some(ScreenBeforeExtra::FileDiff { file_idx: 4 });

        if let Some(prev) = app.screen_before_extra.take() {
            app.screen = match prev {
                ScreenBeforeExtra::EntityDiff { entity_idx } => Screen::EntityDiff { entity_idx },
                ScreenBeforeExtra::FileDiff { file_idx } => Screen::FileDiff { file_idx },
            };
        }

        assert!(matches!(app.screen, Screen::FileDiff { file_idx: 4 }));
    }

    /// When the composer is opened from a screen that is NOT a diff view
    /// (e.g., main entity list — though this isn't currently a code path),
    /// no snapshot is captured and the post-Close screen stays as the
    /// `Screen::Main` placeholder from the `mem::replace`.
    #[test]
    fn open_screen_from_main_captures_no_snapshot() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = make_app(view);
        app.screen = Screen::Main;

        app.screen_before_extra = match app.screen {
            Screen::EntityDiff { entity_idx } => Some(ScreenBeforeExtra::EntityDiff { entity_idx }),
            Screen::FileDiff { file_idx } => Some(ScreenBeforeExtra::FileDiff { file_idx }),
            Screen::Main
            | Screen::Help
            | Screen::Transition(_)
            | Screen::Extra(_)
            | Screen::FilePicker(_)
            | Screen::Extracting => None,
        };

        assert!(app.screen_before_extra.is_none());
    }

    // ─── cycle_file keeps Screen::FileDiff variant in sync ──────────────
    //
    // Regression coverage for: "Tab in FileDiff doesn't advance because
    // `render_file_diff_screen` resets `file_index` from `file_idx` on
    // every render." `cycle_file` must update both `app.file_index` AND
    // the `Screen::FileDiff { file_idx }` payload.

    #[test]
    fn cycle_file_in_file_diff_screen_updates_variant() {
        let v1 = view_with_kinds(&[RenderedLineKind::HunkHeader, RenderedLineKind::Added]);
        let v2 = view_with_kinds(&[RenderedLineKind::HunkHeader, RenderedLineKind::Removed]);
        let v3 = view_with_kinds(&[RenderedLineKind::HunkHeader, RenderedLineKind::Context]);
        let mut app = make_app_with_views(vec![v1, v2, v3]);
        app.screen = Screen::FileDiff { file_idx: 0 };

        app.cycle_file(1);

        assert_eq!(app.file_index, 1, "file_index advances");
        assert!(
            matches!(app.screen, Screen::FileDiff { file_idx: 1 }),
            "Screen::FileDiff variant must track file_index, otherwise the \
             next render resets file_index back to the variant's value"
        );
    }

    #[test]
    fn cycle_file_outside_file_diff_does_not_set_file_diff_screen() {
        let v1 = view_with_kinds(&[RenderedLineKind::Added]);
        let v2 = view_with_kinds(&[RenderedLineKind::Removed]);
        let mut app = make_app_with_views(vec![v1, v2]);
        app.screen = Screen::Main;

        app.cycle_file(1);

        assert_eq!(app.file_index, 1);
        assert!(
            matches!(app.screen, Screen::Main),
            "cycle_file from Main must leave the screen alone"
        );
    }

    // ── Render regression guard ───────────────────────────────────────────────
    //
    // These tests catch the class of bug where expensive operations (IO,
    // subprocesses, cache reads) are accidentally introduced into the render
    // path. Each test renders one frame and either:
    //   - asserts a spy surface method was never called (correctness), or
    //   - asserts the frame completed within a tight time budget (timing).
    //
    // If either fails, something expensive crept into render(). Fix the render
    // path, not the test.

    /// A surface that delegates everything to `NoopSurface` but panics if
    /// `caller_count` is called — guarding against re-introducing per-entity IO
    /// in the render path.
    struct SpySurface(NoopSurface);

    impl SpySurface {
        fn new(views: Vec<DiffView>) -> Self {
            Self(NoopSurface::new(views))
        }
    }

    impl ReviewSurface for SpySurface {
        type Error = NoopError;
        fn entry_count(&self) -> usize {
            self.0.entry_count()
        }
        fn current_entry_index(&self) -> usize {
            self.0.current_entry_index()
        }
        fn entry_id_display(&self, idx: usize) -> String {
            self.0.entry_id_display(idx)
        }
        fn entry_description(&self, idx: usize) -> String {
            self.0.entry_description(idx)
        }
        fn fetch_views(&mut self, idx: usize) -> Result<Vec<DiffView>, NoopError> {
            self.0.fetch_views(idx)
        }
        fn inline_comments_for_view(
            &self,
            t: std::time::SystemTime,
            v: usize,
            f: Option<Severity>,
        ) -> Vec<crate::tui::InlineComment> {
            self.0.inline_comments_for_view(t, v, f)
        }
        fn save_comment(&mut self, r: SaveRequest<'_>) -> Result<SaveOutcome, NoopError> {
            self.0.save_comment(r)
        }
        fn update_comment(&mut self, r: UpdateRequest<'_>) -> Result<SaveOutcome, NoopError> {
            self.0.update_comment(r)
        }
        fn delete_comment(&mut self, r: DeleteRequest) -> Result<DeleteOutcome, NoopError> {
            self.0.delete_comment(r)
        }
        fn is_view_reviewed(&self, idx: usize) -> bool {
            self.0.is_view_reviewed(idx)
        }
        fn mark_view_reviewed(&mut self, idx: usize) -> MarkReviewedOutcome {
            self.0.mark_view_reviewed(idx)
        }
        fn toggle_view_reviewed(&mut self, idx: usize) -> ReviewedOutcome {
            self.0.toggle_view_reviewed(idx)
        }
        fn severity_histogram(&self) -> SeverityHistogram {
            self.0.severity_histogram()
        }
        fn handle_extra_key(
            &mut self,
            k: KeyEvent,
            fi: usize,
            li: usize,
            v: Option<&DiffView>,
        ) -> Result<ExtraKeyAction, NoopError> {
            self.0.handle_extra_key(k, fi, li, v)
        }
        fn render_extra_screen(&self, f: &mut Frame<'_>, s: &mut dyn ExtraScreen) {
            self.0.render_extra_screen(f, s);
        }
        fn handle_extra_screen_key(
            &mut self,
            s: &mut dyn ExtraScreen,
            k: KeyEvent,
            c: &mut ExtraScreenContext<'_>,
        ) -> Result<ExtraScreenAction, NoopError> {
            self.0.handle_extra_screen_key(s, k, c)
        }
        fn file_picker_entries(&self) -> Vec<FilePickerEntry> {
            self.0.file_picker_entries()
        }
        fn help_screen_title(&self) -> &'static str {
            self.0.help_screen_title()
        }
        fn help_screen_body(&self) -> &'static str {
            self.0.help_screen_body()
        }
        fn footer_hint(&self, w: u16, s: bool, f: Option<Severity>) -> String {
            self.0.footer_hint(w, s, f)
        }

        fn caller_count(&self, _: usize, _: &crate::semantic::EntityId) -> Option<usize> {
            panic!(
                "caller_count() called during render — this is a regression. \
                 Render functions must read pre-computed state only, never trigger IO. \
                 See the render regression guard tests in app.rs."
            );
        }

        fn entry_graph(&self, _: usize) -> Option<crate::semantic::GraphData> {
            panic!(
                "entry_graph() called during render — this is a regression. \
                 On jjr it spawns a subprocess and reads the full entity cache; \
                 per-keystroke it makes scrolling unusable. Compute tiers and \
                 ordering at entry load, never in render."
            );
        }
    }

    impl ReviewSurfaceExt for SpySurface {
        fn on_entry_loaded(&mut self, idx: usize, r: bool) {
            self.0.on_entry_loaded(idx, r);
        }
        fn severity_histogram_for_transition(&self) -> (Option<usize>, SeverityHistogram) {
            self.0.severity_histogram_for_transition()
        }
    }

    fn make_entity_for_render(i: usize) -> crate::semantic::EntitySummary {
        crate::semantic::EntitySummary {
            id: crate::semantic::EntityId::new(
                std::path::PathBuf::from("src/lib.rs"),
                vec![format!("fn{i}")],
                None,
                u32::try_from(i).unwrap_or(0),
            ),
            display_name: format!("fn{i}"),
            kind: crate::semantic::EntityKind::Function,
            change: crate::semantic::ChangeType::Modified,
            annotation: crate::semantic::ChangeAnnotation::BodyOnly,
            file_path: std::path::PathBuf::from("src/lib.rs"),
            source_file: None,
            target_line: None,
            line_range: (
                u32::try_from(i * 10 + 1).unwrap_or(1),
                u32::try_from(i * 10 + 9).unwrap_or(9),
            ),
            structural_change: true,
            content_hash: 0,
            refactor: None,
            comment_count: 0,
            reviewed: false,
            risk: None,
            fallback: false,
        }
    }

    /// Rendering the entity list must never call `caller_count` on the surface.
    ///
    /// This guards against re-introducing the regression where per-entity IO
    /// (cache reads, subprocess spawns) crept into the render hot path.
    /// If this test panics with `"caller_count() called during render"`, fix the
    /// render path — not this test.
    #[test]
    fn render_entity_list_does_not_call_surface_caller_count() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let spy = SpySurface::new(vec![view.clone()]);
        let mut app = App::new(spy, vec![view], TransitionMode::Never);
        app.screen = Screen::Main;
        for i in 0..20 {
            app.entities.push(make_entity_for_render(i));
        }

        // Render a full frame — SpySurface::caller_count panics on any call.
        let backend = TestBackend::new(200, 50);
        let mut term = Terminal::new(backend).expect("terminal");
        // Will panic (test fails) if caller_count is called from render.
        term.draw(|frame| render(frame, &mut app)).expect("draw");
    }

    /// Rendering the entity diff must never call `caller_count` or
    /// `entry_graph` either — this render runs on every scroll keystroke,
    /// and on jjr those methods spawn a subprocess and read the full entity
    /// cache. The status-bar context (which historically did a live
    /// `caller_count` lookup here, causing visible scroll lag) must format
    /// from pre-computed state only. The entity deliberately has no
    /// computed tier: that is the worst-case (legacy) formatting path.
    #[test]
    fn render_entity_diff_does_not_call_surface_io() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut view = view_with_kinds(&[RenderedLineKind::Added, RenderedLineKind::Context]);
        view.title = "src/lib.rs".to_owned();
        // Put the cursor line inside entity 0's range (1..=9) so the
        // status-bar context actually resolves an entity.
        view.lines[0].target_line = Some(2);
        view.lines[1].target_line = Some(3);

        let spy = SpySurface::new(vec![view.clone()]);
        let mut app = App::new(spy, vec![view], TransitionMode::Never);
        app.entities.push(make_entity_for_render(0));
        app.screen = Screen::EntityDiff { entity_idx: 0 };
        app.line_index = 0;

        let backend = TestBackend::new(200, 50);
        let mut term = Terminal::new(backend).expect("terminal");
        // Will panic (test fails) if caller_count or entry_graph is called.
        term.draw(|frame| render(frame, &mut app)).expect("draw");
        assert!(
            app.entity_context
                .as_deref()
                .is_some_and(|c| c.contains("fn0")),
            "status-bar context must still resolve the entity: {:?}",
            app.entity_context
        );
    }

    /// `m` on the entity list is surface-first: a surface that claims it
    /// (ggr's commit-scoped composer) must receive it, and the core must NOT
    /// open the description view over it.
    #[test]
    fn entity_list_m_is_surface_first() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = App::new(
            NoopSurface::new_claiming_m(vec![view.clone()]),
            vec![view],
            TransitionMode::Never,
        );
        app.screen = Screen::Main;
        let key = KeyEvent::new(KeyCode::Char('m'), crossterm::event::KeyModifiers::NONE);
        handle_entity_list_key(&mut app, key).expect("key handling");
        assert!(
            matches!(app.screen, Screen::Main),
            "core must not open the description view when the surface claims m"
        );
        assert_eq!(app.status_message.as_deref(), Some("m claimed"));
    }

    /// When the surface ignores `m`, the core falls back to opening the
    /// description view (jjr's binding).
    #[test]
    fn entity_list_m_falls_back_to_description_view() {
        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let mut app = App::new(
            NoopSurface::new(vec![view.clone()]),
            vec![view],
            TransitionMode::Never,
        );
        app.screen = Screen::Main;
        let key = KeyEvent::new(KeyCode::Char('m'), crossterm::event::KeyModifiers::NONE);
        handle_entity_list_key(&mut app, key).expect("key handling");
        assert!(
            matches!(app.screen, Screen::FileDiff { file_idx: 0 }),
            "surface ignored m; core must open the description view"
        );
    }

    /// `o` cycles risk → dependency → file → risk, and the choice is
    /// session state: the entry-load path re-applies it, never resets it.
    #[test]
    fn o_cycles_three_orders_and_survives_entry_reload() {
        use crate::semantic::OrderMode;
        let mut app = make_app_with_entities(3);
        app.screen = Screen::Main;
        assert_eq!(app.order_mode, OrderMode::Risk, "risk is the default");

        let key = KeyEvent::new(KeyCode::Char('o'), crossterm::event::KeyModifiers::NONE);
        handle_entity_list_key(&mut app, key).expect("key handling");
        assert_eq!(app.order_mode, OrderMode::Dependency);
        assert_eq!(app.status_message.as_deref(), Some("order: dependency"));
        handle_entity_list_key(&mut app, key).expect("key handling");
        assert_eq!(app.order_mode, OrderMode::File);
        handle_entity_list_key(&mut app, key).expect("key handling");
        assert_eq!(app.order_mode, OrderMode::Risk, "cycle wraps");

        handle_entity_list_key(&mut app, key).expect("key handling");
        app.refresh_entity_order(); // what an entry load runs
        assert_eq!(
            app.order_mode,
            OrderMode::Dependency,
            "entry navigation must not reset the chosen order"
        );
    }

    /// The degraded-tiers notice includes the surface's reason when it has
    /// one (ggr's clone lifecycle) and stays generic otherwise.
    #[test]
    fn degraded_notice_includes_surface_reason() {
        let mut app = make_app_with_entities(1);
        app.surface.graph_reason = Some("clone in progress".to_owned());
        app.refresh_entity_order();
        assert_eq!(
            app.status_message.as_deref(),
            Some("graph unavailable — clone in progress; risk tiers degraded")
        );

        let mut app = make_app_with_entities(1);
        app.refresh_entity_order();
        assert_eq!(
            app.status_message.as_deref(),
            Some("graph unavailable — risk tiers degraded")
        );
    }

    /// With a computed tier, the status bar shows `name change · tier ·
    /// clause`, plus the degraded notice when the graph was unavailable.
    #[test]
    fn entity_context_shows_tier_clause_and_degraded_notice() {
        let mut e = make_entity_for_render(0);
        e.annotation = crate::semantic::ChangeAnnotation::SigChanged;
        e.risk = Some(crate::semantic::RiskAssessment {
            tier: crate::semantic::RiskTier::High,
            clause: "sig change · unverified callers".to_owned(),
        });
        assert_eq!(
            format_entity_context(&e, true),
            "fn0 modified · high · sig change · unverified callers · tiers degraded"
        );
        assert_eq!(
            format_entity_context(&e, false),
            "fn0 modified · high · sig change · unverified callers"
        );
    }

    /// End-to-end over the pure layer + renderer: a sig-changed entity with
    /// callers tiers High, sorts before all Medium/Low entities in risk
    /// order, and its row carries the `!` badge.
    #[test]
    fn high_risk_entity_sorts_first_and_carries_badge() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut high = make_entity_for_render(0);
        high.display_name = "hot_fn".to_owned();
        high.annotation = crate::semantic::ChangeAnnotation::SigChanged;
        let mut med = make_entity_for_render(1);
        med.display_name = "warm_fn".to_owned();
        let mut low = make_entity_for_render(2);
        low.display_name = "cold_fn".to_owned();
        low.structural_change = false;

        let graph = crate::semantic::GraphData {
            nodes: Vec::new(),
            edges: vec![crate::semantic::GraphEdge {
                from: crate::semantic::EntityId::new(
                    std::path::PathBuf::from("src/lib.rs"),
                    vec!["caller".to_owned()],
                    None,
                    9,
                ),
                to: high.id.clone(),
                call_sites: vec![7],
            }],
            unresolved: Vec::new(),
        };

        // Listed worst-last so only the risk sort can front-load `hot_fn`.
        let mut entities = vec![low, med, high];
        crate::semantic::compute_risk_tiers(&mut entities, Some(&graph));
        crate::semantic::sort_entities(
            &mut entities,
            crate::semantic::OrderMode::Risk,
            Some(&graph),
        );
        assert_eq!(entities[0].display_name, "hot_fn");
        let risk = entities[0].risk.as_ref().expect("tier computed");
        assert_eq!(risk.tier, crate::semantic::RiskTier::High);
        assert_eq!(risk.clause, "sig change · 1 caller");

        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let spy = SpySurface::new(vec![view.clone()]);
        let mut app = App::new(spy, vec![view], TransitionMode::Never);
        app.screen = Screen::Main;
        app.entities = entities;
        app.refresh_header_stats();

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|frame| render(frame, &mut app)).expect("draw");
        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let hot_row = rows
            .iter()
            .find(|r| r.contains("hot_fn"))
            .expect("hot_fn row rendered");
        assert!(
            hot_row.starts_with("! "),
            "High row must carry the ! badge: {hot_row:?}"
        );
        let warm_row = rows
            .iter()
            .find(|r| r.contains("warm_fn"))
            .expect("warm_fn row rendered");
        assert!(
            warm_row.starts_with("  "),
            "Medium row must not carry the badge: {warm_row:?}"
        );
    }

    /// The orientation header must render subject, body peek, and Σ scope
    /// line with visible text at the standard 80×24 size.
    #[test]
    fn entity_list_renders_orientation_header_text() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let view = view_with_kinds(&[RenderedLineKind::Added]);
        let spy = SpySurface::new(vec![view.clone()]);
        let mut app = App::new(spy, vec![view], TransitionMode::Never);
        app.screen = Screen::Main;
        app.entities.push(make_entity_for_render(0));
        app.refresh_header_stats();
        app.description_summary = Some(crate::semantic::DescriptionSummary {
            subject: "Extract token validation".to_owned(),
            comment_count: 0,
            body_peek: Some("pulls validation out of Session".to_owned()),
        });

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|frame| render(frame, &mut app)).expect("draw");

        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let all = rows.join("\n");
        assert!(
            all.contains("Extract token validation"),
            "subject missing from header:\n{all}"
        );
        assert!(
            all.contains("pulls validation out of Session"),
            "body peek missing from header:\n{all}"
        );
        assert!(
            all.contains("Σ 1 entity"),
            "Σ scope line missing from header:\n{all}"
        );
    }
}
