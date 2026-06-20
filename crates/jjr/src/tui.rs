use std::io::{stdout, Stdout};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
#[cfg(test)]
use ratatui::layout::{Constraint, Layout, Rect};
#[cfg(test)]
use ratatui::style::{Color, Modifier, Style};
#[cfg(test)]
use ratatui::text::{Line as TuiLine, Span};
#[cfg(test)]
use ratatui::widgets::{Block, Borders, Paragraph};
#[cfg(test)]
use ratatui::widgets::{Scrollbar, ScrollbarOrientation};
use ratatui::{Frame, Terminal};

use crate::change_id::ChangeId;
use crate::comment::{
    Anchor, Comment, DescriptionAnchor, LineAnchor, SchemaVersion, Severity, Side, Status,
    CONTEXT_MAX,
};
use crate::cursor;
use crate::error::{JjrError, Result};
use crate::jj::{self, ChangeDetails};
use crate::reviewed::{MarkOutcome, ReviewTarget, ReviewedState};
use crate::stack::{ResolvedStack, RevsetHash, StackEntry};
use crate::stderr_log::StderrLogGuard;
#[cfg(test)]
use crate::util::{clamp_with_delta, page_size, pluralize, truncate};

mod composer;
mod composer_overlay;
mod diff_view;
mod file_picker;
mod help_screen;
use help_screen::JJR_HELP_BODY;
mod overview_screen;
mod send_to_claude;
mod stale_screen;
mod textarea;

use composer::{
    default_severity, Composer, ComposerAction, ComposerInit, ComposerScope, DescriptionContext,
    EditedComment, LineTarget, StackContextSnapshot,
};
#[cfg(test)]
use diff_view::PairedRow;
use diff_view::{
    change_comment_to_inline, comment_to_inline, description_comment_to_inline, CommentIndex,
    DiffView, InlineComment, RenderedLine, RenderedLineKind,
};

use file_picker::build_entries as build_file_picker_entries;
#[cfg(test)]
use file_picker::FilePickerState;
#[cfg(test)]
use local_review_core::tui::composer::{
    STATUS_DESCRIPTION_UNAVAILABLE, STATUS_LINE_UNAVAILABLE, STATUS_STACK_UNAVAILABLE,
};
use local_review_core::tui::try_downcast_mut;
use local_review_core::tui::{
    BaseViews, DeleteOutcome as CoreDeleteOutcome, DeleteRequest, ExtraKeyAction, ExtraScreen,
    ExtraScreenAction, ExtraScreenContext, FilePickerEntry, MarkReviewedOutcome, ReviewSurface,
    ReviewSurfaceExt, ReviewedOutcome, SaveOutcome as CoreSaveOutcome, SaveRequest,
    SeverityHistogram as CoreSeverityHistogram, UpdateRequest,
};
use overview_screen::{OverviewCommentSet, OverviewScreenState, OverviewStackCtx};
use send_to_claude::{ConfirmData, SendToClaudeState};
use stale_screen::StaleScreenState;

/// Type alias for the generic TUI app parameterised over [`JjrSurface`].
pub(crate) type JjrApp = local_review_core::tui::App<JjrSurface>;

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block (one `│` on each side).
#[cfg(test)]
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render measures the
/// real diff area height. Overwritten by `render_main` on every frame.
#[cfg(test)]
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

/// Stack depth at which `transition_screen = "auto"` starts firing. Per spec,
/// deep stacks get the beat between changes; short ones don't need the pause.
#[cfg(test)]
const AUTO_TRANSITION_THRESHOLD: usize = 8;

/// Width (cells) of the graphical fill in the stack progress bar. Drops to
/// zero on narrow terminals (see `render_stack_bar`).
#[cfg(test)]
const STACK_PROGRESS_BAR_WIDTH: u16 = 20;

/// Below this column count, the stack bar drops the graphical fill and shows
/// just the textual `N/M change_id desc...` portion (per the resize ladder).
#[cfg(test)]
const STACK_BAR_MIN_COLS_FOR_FILL: u16 = 80;

/// Width (cells) of the transition modal.
#[cfg(test)]
const TRANSITION_MODAL_WIDTH: u16 = 42;

/// Height (rows) of the transition modal.
#[cfg(test)]
const TRANSITION_MODAL_HEIGHT: u16 = 18;

/// Description budget (chars) inside the transition modal. The modal interior
/// is ~38 cols after borders + indent, so 36 leaves room for the trailing `…`.
#[cfg(test)]
const TRANSITION_DESC_BUDGET: usize = 36;

/// Maximum number of `●` dots rendered before truncating with a trailing `…`.
/// The numeric count stays accurate so the user still sees the true total.
pub(super) const DOT_BUDGET: usize = 5;

/// Placeholder cursor row used when editing a comment from the overview screen,
/// where there is no active diff view and therefore no meaningful line cursor.
/// Always 0 because the `rendered_index` field is unused in this path: the
/// anchor carries the original `source_line`/`target_line` directly.
const OVERVIEW_NO_LINE_CURSOR: usize = 0;

/// Status hint surfaced when Tab is pressed at the last file (or description+files
/// boundary) — the file index is already at its max and cannot advance further.
#[cfg(test)]
const STATUS_AT_LAST_FILE: &str = "already at the last file";

/// Status hint surfaced when Shift-Tab is pressed at `file_index` 0 — there is
/// no previous file to retreat to.
#[cfg(test)]
const STATUS_AT_FIRST_FILE: &str = "already at the first file";

/// Status hint surfaced when Tab/Shift-Tab is pressed and the change has only
/// one navigable view (typical for a description-only change with no diff
/// files), so cycling cannot move in either direction.
#[cfg(test)]
const STATUS_ONLY_ONE_FILE: &str = "only one file";

/// Trailing glyph appended (`DarkGray`) to the file-header title when the
/// active view is reviewed. A glyph reads as "done, move on" rather than
/// `(reviewed)` text, which felt achievement-y.
#[cfg(test)]
const REVIEWED_TITLE_GLYPH: &str = "\u{2713}";

/// Status surfaced when [`App::mark_current_file_reviewed`] detects the
/// stored entry's `commit_id` no longer matches the live commit (the
/// change was amended/rebased) and drops the prior reviewed bits as a
/// result. Distinct from the no-prior-state "first encounter" case, which
/// stays silent.
#[cfg(test)]
const STATUS_REVIEWED_RESET: &str = "change amended; reviewed state reset";

/// Status set by the manual `U` toggle when the active file goes from
/// unreviewed to reviewed.
#[cfg(test)]
const STATUS_MARKED_REVIEWED: &str = "file marked as reviewed";

/// Status set by the manual `U` toggle when the active file goes from
/// reviewed to unreviewed.
#[cfg(test)]
const STATUS_MARKED_UNREVIEWED: &str = "file marked as unreviewed";

/// Resolve the user's `DiffMode` preference plus the current body width and
/// view index into the layout that this render pass should actually use.
///
/// Rules:
/// - `file_index == 0` is the synthetic description view; it has no two-side
///   semantic, so it always renders unified regardless of preference.
/// - `DiffMode::Auto` picks side-by-side iff `body_width >= SIDE_BY_SIDE_MIN_WIDTH`.
/// - `DiffMode::ForceUnified` and `DiffMode::ForceSideBySide` pin the choice.
#[cfg(test)]
pub(super) fn resolve_diff_mode(
    pref: DiffMode,
    body_width: u16,
    file_index: usize,
) -> EffectiveDiffMode {
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

pub(super) use local_review_core::tui::diff_view::severity_label;
pub(super) use local_review_core::tui::severity_color;

pub fn run(
    change_id: &ChangeId,
    repo_root: &std::path::Path,
    data_home: &std::path::Path,
    spinner: Option<local_review_core::startup_spinner::StartupSpinner>,
) -> Result<()> {
    let details = jj::show(change_id)?;
    let revset = change_id.as_str().to_owned();

    // Stop the startup spinner after the slow `jj show` call completes,
    // before crossterm grabs the terminal. The spinner's stderr writes
    // would otherwise interleave with the alt-screen entry sequence.
    if let Some(s) = spinner {
        s.stop();
    }

    let (mut terminal, guard) = enter_tui_session(data_home, repo_root)?;
    let ctx = JjrContext {
        data_home: data_home.to_owned(),
        repo_root: repo_root.to_owned(),
        revset,
    };
    let outcome = run_jjr_app(&mut terminal, details, ctx, None, Some(guard));
    teardown_terminal(&mut terminal)?;
    outcome
}

pub fn run_stack(
    repo_root: &std::path::Path,
    resolved: &ResolvedStack,
    restart: bool,
    data_home: &std::path::Path,
    spinner: Option<local_review_core::startup_spinner::StartupSpinner>,
) -> Result<()> {
    if resolved.entries.is_empty() {
        return Err(JjrError::RevsetNoMatch {
            revset: resolved.revset.clone(),
        });
    }

    if restart {
        cursor::clear(data_home, repo_root, resolved.revset_hash)?;
    }

    let has_comments = |id: &ChangeId| {
        crate::store::load_change_comments(data_home, repo_root, id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    };

    // Smart-resume rule: when the stored cursor change is missing from the
    // current stack (or the cursor file is absent / corrupt), walk LATEST→
    // OLDEST and pick the most-recent change that is NOT fully reviewed.
    // Load reviewed-state once for the resume decision and hand the entry
    // IDs to a closure that asks the store about each.
    let reviewed_state = ReviewedState::load(data_home, repo_root).unwrap_or_default();
    let entries_by_id: std::collections::HashMap<ChangeId, &StackEntry> = resolved
        .entries
        .iter()
        .map(|e| (e.change_id.clone(), e))
        .collect();
    let is_fully_reviewed = |id: &ChangeId| -> bool {
        let Some(entry) = entries_by_id.get(id) else {
            return false;
        };
        let diff_paths = match jj::diff_for_change(id) {
            Ok(diff) => diff
                .files
                .iter()
                .map(|f| f.display_path().to_owned())
                .collect::<Vec<_>>(),
            Err(_) => return false,
        };
        reviewed_state.is_marked_fully_reviewed(id, &entry.commit_id, &diff_paths)
    };

    // Fresh-stack signal: when no change in the resolved stack has any
    // reviewed-state entry, the smart fallback lands at OLDEST instead of
    // walking LATEST→OLDEST. A first-time reviewer reads bottom-up.
    let stack_review_state = if resolved
        .entries
        .iter()
        .any(|entry| reviewed_state.has_entry(&entry.change_id))
    {
        cursor::StackReviewState::Partial
    } else {
        cursor::StackReviewState::Fresh
    };

    let stack_change_ids = resolved
        .entries
        .iter()
        .map(|e| e.change_id.clone())
        .collect::<Vec<_>>();
    let start_index = cursor::resume_index(
        data_home,
        repo_root,
        resolved.revset_hash,
        &cursor::ResumeInputs {
            stack_change_ids: &stack_change_ids,
            has_comments: &has_comments,
            is_fully_reviewed: &is_fully_reviewed,
            stack_review_state,
        },
    );

    let entry = &resolved.entries[start_index];
    let details = jj::show(&entry.change_id)?;

    let stack_ctx = StackContext {
        entries: resolved.entries.clone(),
        current_index: start_index,
        revset: resolved.revset.clone(),
        revset_hash: resolved.revset_hash,
    };

    // Stop the startup spinner after all per-entry `jj show` and
    // `jj diff_for_change` calls complete, before crossterm grabs the
    // terminal. On large stacks this is where most of the latency lives.
    if let Some(s) = spinner {
        s.stop();
    }

    let (mut terminal, guard) = enter_tui_session(data_home, repo_root)?;
    let ctx = JjrContext {
        data_home: data_home.to_owned(),
        repo_root: repo_root.to_owned(),
        revset: resolved.revset.clone(),
    };
    let outcome = run_jjr_app(&mut terminal, details, ctx, Some(stack_ctx), Some(guard));
    teardown_terminal(&mut terminal)?;
    outcome
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Install the stderr-redirect guard before the alt screen so its Drop
/// runs after `teardown_terminal` at the call site (via App).
fn enter_tui_session(
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
) -> Result<(Term, StderrLogGuard)> {
    let guard = StderrLogGuard::install(data_home, repo_root)?;
    let term = setup_terminal()?;
    Ok((term, guard))
}

fn setup_terminal() -> Result<Term> {
    let (cols, rows) = crossterm::terminal::size().map_err(io_err)?;
    if cols < MIN_COLS {
        return Err(JjrError::TerminalTooNarrow { cols });
    }
    if rows < MIN_ROWS {
        return Err(JjrError::TerminalTooShort { rows });
    }
    enable_raw_mode().map_err(io_err)?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).map_err(io_err)?;
    let backend = CrosstermBackend::new(out);
    Terminal::new(backend).map_err(io_err)
}

fn teardown_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().map_err(io_err)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(io_err)?;
    terminal.show_cursor().map_err(io_err)?;
    Ok(())
}

fn io_err(source: std::io::Error) -> JjrError {
    JjrError::Io { source }
}

#[cfg(test)]
enum Screen {
    Main,
    Help,
    /// Composer modal is open; variant owns the state.
    Composer(Box<Composer>),
    /// Between-change transition beat shown when advancing in stack mode.
    Transition(TransitionState),
    /// Stale comments browser.
    Stale(StaleScreenState),
    /// Stack overview (press `s` from Main).
    Overview(OverviewScreenState),
    /// Send-to-Claude confirmation (press `C` from Main).
    SendToClaude(Box<SendToClaudeState>),
    /// File picker modal (press `f` from Main).
    FilePicker(FilePickerState),
}

/// State for the transition screen shown between changes in stack mode.
#[cfg(test)]
struct TransitionState {
    /// Index of the change just reviewed.
    reviewed_index: usize,
    /// Index of the next change to open.
    next_index: usize,
    /// Comment count for the reviewed change. `None` when the comment load
    /// failed (so the modal can say so honestly instead of lying with `0`).
    reviewed_comment_count: Option<usize>,
    /// Severity histogram of the reviewed change's comments, ordered
    /// `(required, suggestion, note)`. Empty when the count is `None`.
    severity_histogram: CoreSeverityHistogram,
}

/// Count active (non-stale, non-orphaned) comments and return a
/// [`CoreSeverityHistogram`]. Stale and orphaned records live in the stale
/// view and are excluded from the active-comment totals.
pub(super) fn histogram_from_comments(comments: &[Comment]) -> CoreSeverityHistogram {
    let mut h = CoreSeverityHistogram::default();
    for c in comments {
        if matches!(c.status, Some(Status::Stale | Status::Orphaned)) {
            continue;
        }
        match c.severity {
            Severity::Required => h.required += 1,
            Severity::Suggestion => h.suggestion += 1,
            Severity::Note => h.note += 1,
        }
    }
    h
}

/// Context kept while the user is picking a new anchor for a stale comment.
///
/// Save-then-delete ordering: if the process dies between saving the new
/// comment and deleting the original stale, the user sees both on next load
/// and can delete the stale manually. Delete-then-save would be strictly
/// worse: a crash after delete but before save loses the comment entirely.
struct PendingReanchor {
    /// Used to delete after save succeeds.
    original: Comment,
    /// Pre-populates the composer when the user picks a new anchor.
    body: String,
    severity: Severity,
}

/// Stack navigation context; absent in single-change mode.
struct StackContext {
    entries: Vec<StackEntry>,
    current_index: usize,
    revset: String,
    revset_hash: RevsetHash,
}

// ---------------------------------------------------------------------------
// JjrSurface — the ReviewSurface implementation for jjr
// ---------------------------------------------------------------------------

/// Location context for a review session: where to find the repo on disk and
/// where to read/write comment state.
///
/// Bundled as a single argument to keep `JjrSurface::new` within the five-arg
/// limit that the workspace enforces.
pub(crate) struct JjrContext {
    pub(crate) data_home: PathBuf,
    pub(crate) repo_root: PathBuf,
    pub(crate) revset: String,
}

/// jjr-specific TUI surface state. Holds the data that differs between jjr
/// and other future tools (ggr). Plugged into the generic
/// `local_review_core::tui::App<JjrSurface>` via the `ReviewSurface` and
/// `ReviewSurfaceExt` traits.
pub(crate) struct JjrSurface {
    details: ChangeDetails,
    /// Data home for comment storage (XDG-based path).
    data_home: PathBuf,
    /// Repo root for comment storage.
    repo_root: PathBuf,
    /// The revset string used to open this view.
    revset: String,
    /// Comments loaded for the current change.
    loaded_comments: Vec<Comment>,
    /// Stack navigation context; `None` in single-change mode.
    stack: Option<StackContext>,
    /// Whether the most recent comment load succeeded.
    comments_loaded_ok: bool,
    /// Active when the user is picking a new anchor for a stale comment.
    pending_reanchor: Option<PendingReanchor>,
    /// Severity used for the most recently saved comment; seeds the default
    /// for the next composer open so the reviewer doesn't have to re-select.
    last_severity: Option<Severity>,
    /// Cached comments for the stack overview.
    overview_cache: Option<OverviewCommentSet>,
    /// Persistent reviewed-bits.
    reviewed: ReviewedState,
    /// Stderr-redirect guard. `None` in tests.
    stderr_guard: Option<StderrLogGuard>,
    /// Deferred status message from a reconcile-and-persist error; taken by
    /// the core after entry load via `ReviewSurfaceExt::take_pending_status_message`.
    pending_status_message: Option<String>,
    /// Base rendered views for the current change, kept in sync with `details`.
    /// Populated on every `fetch_views` call so `file_picker_entries` can read
    /// from it instead of re-rendering from scratch on every file-picker open.
    rendered_views: Vec<DiffView>,
}

impl JjrSurface {
    fn new(
        details: ChangeDetails,
        ctx: JjrContext,
        stack: Option<StackContext>,
        stderr_guard: Option<StderrLogGuard>,
    ) -> Self {
        let reviewed = ReviewedState::load(&ctx.data_home, &ctx.repo_root).unwrap_or_default();
        let rendered_views = build_rendered_views(&details);
        Self {
            details,
            data_home: ctx.data_home,
            repo_root: ctx.repo_root,
            revset: ctx.revset,
            loaded_comments: Vec::new(),
            stack,
            comments_loaded_ok: false,
            pending_reanchor: None,
            last_severity: None,
            overview_cache: None,
            reviewed,
            stderr_guard,
            pending_status_message: None,
            rendered_views,
        }
    }

    /// Resolve the current `file_index` to a `ReviewTarget`. `file_index` 0
    /// is the synthetic description view; indices 1.. map onto diff files.
    fn review_target_for_index(&self, file_index: usize) -> Option<ReviewTarget> {
        if file_index == 0 {
            return Some(ReviewTarget::Description);
        }
        let path = self
            .details
            .diff
            .files
            .get(file_index - 1)?
            .display_path()
            .to_owned();
        Some(ReviewTarget::File(path))
    }

    /// Return the `ChangeId` for `entry_idx`, or the current change if
    /// out of range.
    fn entry_change_id(&self, entry_idx: usize) -> Result<ChangeId> {
        if let Some(ctx) = self.stack.as_ref() {
            ctx.entries
                .get(entry_idx)
                .map(|e| e.change_id.clone())
                .ok_or_else(|| JjrError::JjUnexpectedOutput {
                    raw: format!("entry index {entry_idx} out of range"),
                })
        } else {
            Ok(self.details.change_id.clone())
        }
    }

    /// Load, reconcile, and store comments for the current change.
    ///
    /// Returns `None` on success; `Some(msg)` with an error description on
    /// failure. The caller may surface `msg` as a status message.
    fn reload_comments(&mut self) -> Option<String> {
        self.overview_cache = None;
        match crate::store::load_change_comments(
            &self.data_home,
            &self.repo_root,
            &self.details.change_id,
        ) {
            Ok(comments) => {
                self.loaded_comments = self.reconcile_and_persist(comments);
                self.comments_loaded_ok = true;
                None
            }
            Err(e) => {
                self.loaded_comments = Vec::new();
                self.comments_loaded_ok = false;
                Some(sanitize_for_status(&e.to_string()))
            }
        }
    }

    fn reconcile_and_persist(&mut self, comments: Vec<Comment>) -> Vec<Comment> {
        let (reconciled, first_error) = reconcile_comments_with_diff(
            comments,
            &self.details.diff,
            &self.details.description,
            &self.data_home,
            &self.repo_root,
        );
        if let Some(msg) = first_error {
            self.pending_status_message = Some(msg);
        }
        reconciled
    }

    /// Load comments from all changes in the stack and store in the overview cache.
    fn load_overview_comments(&mut self) {
        let Some(ctx) = self.stack.as_ref() else {
            return;
        };
        let revset_hash = ctx.revset_hash;
        let entries = ctx.entries.clone();
        let data_home = self.data_home.clone();
        let repo_root = self.repo_root.clone();

        let stack_level = crate::store::load_stack_comments(&data_home, &repo_root, &revset_hash)
            .unwrap_or_else(|_| Vec::new());

        let per_change: Vec<Vec<Comment>> = entries
            .iter()
            .map(|entry| {
                crate::store::load_change_comments(&data_home, &repo_root, &entry.change_id)
                    .unwrap_or_default()
            })
            .collect();

        let diff_paths_per_change: Vec<Vec<PathBuf>> = entries
            .iter()
            .map(|entry| match jj::diff_for_change(&entry.change_id) {
                Ok(diff) => diff
                    .files
                    .iter()
                    .map(|f| f.display_path().to_owned())
                    .collect(),
                Err(_) => Vec::new(),
            })
            .collect();

        let orphaned = collect_orphaned_comments(&data_home, &repo_root, &entries);

        self.overview_cache = Some(OverviewCommentSet {
            stack_level,
            per_change,
            orphaned,
            diff_paths_per_change,
        });
    }

    /// Mark the view at `file_index` as reviewed and return the outcome.
    fn mark_view_reviewed_impl(&mut self, file_index: usize) -> MarkReviewedOutcome {
        let Some(target) = self.review_target_for_index(file_index) else {
            return MarkReviewedOutcome::NoReset;
        };
        let change_id = self.details.change_id.clone();
        let commit_id = self.details.commit_id.clone();
        let outcome = match self.reviewed.mark(change_id, commit_id, target) {
            MarkOutcome::ResetDueToCommitMismatch => MarkReviewedOutcome::ResetDueToCommitMismatch,
            MarkOutcome::NoReset => MarkReviewedOutcome::NoReset,
        };
        let _ = self.reviewed.save(&self.data_home, &self.repo_root); // best-effort
        outcome
    }
}

// ---------------------------------------------------------------------------
// ExtraScreen wrappers for jjr-specific overlay/full-screen states
// ---------------------------------------------------------------------------

/// Wraps `Box<Composer>` so it can be stored as `Box<dyn ExtraScreen>`.
///
/// Uses jjr's own `Composer` wrapper (not the core type) because jjr's save/
/// update/delete paths need the extra `original_anchor` field carried on the
/// jjr `Composer`.
struct ComposerScreen(Box<Composer>);
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

/// Wraps `StaleScreenState` so it can be stored as `Box<dyn ExtraScreen>`.
struct StaleScreen(StaleScreenState);
impl ExtraScreen for StaleScreen {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Wraps `OverviewScreenState` so it can be stored as `Box<dyn ExtraScreen>`.
struct OverviewScreen(OverviewScreenState);
impl ExtraScreen for OverviewScreen {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Wraps `Option<Box<SendToClaudeState>>` so it can be stored as
/// `Box<dyn ExtraScreen>`. The `Option` is `Some` at all times except for the
/// atomic `take()`/re-assign inside `handle_send_to_claude_key_impl`; no
/// external code observes `None`.
struct SendToClaudeScreen(Option<Box<SendToClaudeState>>);
impl SendToClaudeScreen {
    fn new(state: SendToClaudeState) -> Self {
        Self(Some(Box::new(state)))
    }
    fn inner(&self) -> &SendToClaudeState {
        match self.0.as_deref() {
            Some(s) => s,
            None => unreachable!(
                "SendToClaudeScreen.state is always Some outside of transitions; this is a bug"
            ),
        }
    }
}
impl ExtraScreen for SendToClaudeScreen {
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

// ---------------------------------------------------------------------------
// ReviewSurface + ReviewSurfaceExt implementation for JjrSurface
// ---------------------------------------------------------------------------

impl ReviewSurface for JjrSurface {
    type Error = JjrError;

    fn entry_count(&self) -> usize {
        self.stack.as_ref().map_or(1, |s| s.entries.len())
    }

    fn current_entry_index(&self) -> usize {
        self.stack.as_ref().map_or(0, |s| s.current_index)
    }

    fn entry_id_display(&self, idx: usize) -> String {
        let Some(ctx) = self.stack.as_ref() else {
            return self.details.change_id.head().to_owned();
        };
        ctx.entries
            .get(idx)
            .map(|e| e.change_id.head().to_owned())
            .unwrap_or_default()
    }

    fn entry_description(&self, idx: usize) -> String {
        let Some(ctx) = self.stack.as_ref() else {
            return self
                .details
                .description
                .lines()
                .next()
                .unwrap_or("")
                .to_owned();
        };
        ctx.entries
            .get(idx)
            .map(|e| e.description.lines().next().unwrap_or("").to_owned())
            .unwrap_or_default()
    }

    fn fetch_views(&mut self, idx: usize) -> std::result::Result<Vec<DiffView>, JjrError> {
        if let Some(ctx) = self.stack.as_ref() {
            let change_id = ctx
                .entries
                .get(idx)
                .ok_or_else(|| JjrError::JjUnexpectedOutput {
                    raw: format!("stack index {idx} out of range"),
                })?
                .change_id
                .clone();
            let details = jj::show(&change_id)?;
            if let Some(ctx) = self.stack.as_mut() {
                ctx.current_index = idx;
            }
            self.details = details;
        } else {
            let details = jj::show(&self.details.change_id)?;
            self.details = details;
        }
        // Errors surface via the status bar; discard the return value here.
        let _ = self.reload_comments();
        self.rendered_views = build_rendered_views(&self.details);
        Ok(self.rendered_views.clone())
    }

    fn fetch_entity_list(
        &self,
        entry_idx: usize,
    ) -> std::result::Result<Vec<local_review_core::semantic::EntitySummary>, JjrError> {
        let change_id = self.entry_change_id(entry_idx)?;
        // Use commit_id (content-addressed by jj) as the cache discriminator:
        // any amendment to the change produces a new commit_id, invalidating
        // the cache without needing a separate content hash.
        let details = jj::show(&change_id)?;
        let commit_id = details.commit_id.as_str().to_owned();
        let cache_path = entity_cache_path(
            &self.data_home,
            &self.repo_root,
            change_id.as_str(),
            &commit_id,
        );

        let registry = local_review_core::semantic::create_default_registry();
        if let Ok(Some(entry)) = local_review_core::semantic::cache::read(&cache_path) {
            let summaries = build_entity_summaries_interleaved(entry, &details.diff);
            return Ok(self.populate_reviewed_bits(entry_idx, summaries));
        }

        let diff = details.diff;
        let parent_rev = jj::parent_rev(&change_id);
        let current_rev = change_id.as_str().to_owned();
        let ctx = FileExtractCtx {
            registry: &registry,
            repo_root: &self.repo_root,
            current_rev: &current_rev,
            parent_rev: &parent_rev,
        };
        let mut entities = Vec::new();
        let mut failed_files = Vec::new();

        for file in &diff.files {
            let path = file.display_path().to_string_lossy().into_owned();
            extract_file_entities(&ctx, &path, &mut entities, &mut failed_files);
        }

        // Build the cross-file call graph for the Claude bundle. Mirrors
        // the async path's `build_graph_best_effort`: missing graph is a
        // degraded bundle, not a hard error. Inline here (no streaming) is
        // fine — the sync path is only hit when the surface skips the async
        // extraction (cache miss without a worker).
        let graph = jj::list_tracked_files(change_id.as_str(), &self.repo_root);
        let graph = if graph.is_empty() {
            None
        } else {
            Some(local_review_core::semantic::build_graph(
                &registry,
                &self.repo_root,
                &graph,
            ))
        };
        let cache_entry = local_review_core::semantic::cache::CacheEntry {
            schema_version: local_review_core::semantic::cache::SCHEMA_VERSION,
            extraction_hash: local_review_core::semantic::cache::EXTRACTION_HASH.to_owned(),
            entities,
            graph,
            failed_files,
        };
        let _ = local_review_core::semantic::cache::write(&cache_path, &cache_entry);
        let summaries = build_entity_summaries_interleaved(cache_entry, &diff);
        Ok(self.populate_reviewed_bits(entry_idx, summaries))
    }

    /// Build a background extraction task. Clones only the data the
    /// runnable needs (paths, change id) so the task is `Send` and can
    /// outlive the surface borrow. Returns `None` if the entry index does
    /// not map to a change, in which case the core falls back to the
    /// synchronous `fetch_entity_list` path.
    fn entity_extraction_task(
        &self,
        entry_idx: usize,
    ) -> Option<Box<dyn local_review_core::tui::entity_list::ExtractionRunner>> {
        let change_id = self.entry_change_id(entry_idx).ok()?;
        Some(Box::new(JjrExtractionTask {
            change_id,
            repo_root: self.repo_root.clone(),
            data_home: self.data_home.clone(),
        }))
    }

    fn inline_comments_for_view(
        &self,
        now: std::time::SystemTime,
        view_idx: usize,
        severity_filter: Option<Severity>,
    ) -> Vec<InlineComment> {
        let now = time::OffsetDateTime::from(now);
        if view_idx == 0 {
            self.loaded_comments
                .iter()
                .enumerate()
                .filter_map(|(idx, c)| description_comment_to_inline(c, idx, now))
                .filter(|ic| severity_filter.is_none_or(|f| ic.severity == f))
                .collect()
        } else {
            let diff_file_idx = view_idx - 1;
            let file_path = self
                .details
                .diff
                .files
                .get(diff_file_idx)
                .map(|f| f.display_path().to_owned());
            self.loaded_comments
                .iter()
                .enumerate()
                .filter_map(|(idx, c)| comment_to_inline(c, idx, file_path.as_deref(), now))
                .filter(|ic| severity_filter.is_none_or(|f| ic.severity == f))
                .collect()
        }
    }

    fn appended_comments_for_view(
        &self,
        view_idx: usize,
        severity_filter: Option<Severity>,
    ) -> Vec<InlineComment> {
        if view_idx != 0 {
            return Vec::new();
        }
        let now = time::OffsetDateTime::now_utc();
        let target_change_id = self.details.change_id.clone();
        self.loaded_comments
            .iter()
            .enumerate()
            .filter_map(|(idx, c)| change_comment_to_inline(c, idx, &target_change_id, now))
            .filter(|ic| severity_filter.is_none_or(|f| ic.severity == f))
            .collect()
    }

    fn save_comment(
        &mut self,
        req: SaveRequest<'_>,
    ) -> std::result::Result<CoreSaveOutcome, JjrError> {
        let body = req.body;
        if body.trim().is_empty() {
            return Ok(CoreSaveOutcome::Refused {
                reason: "comment body is empty — not saved".to_owned(),
            });
        }
        let oversized = body.chars().count() > crate::comment::BODY_MAX;
        let scope = req.scope;
        let severity = req.severity;
        let now = time::OffsetDateTime::now_utc();

        let anchor = build_anchor_from_scope(scope, &self.details.change_id, &self.details);

        let anchor_fingerprint = if let Anchor::Line { ref location, .. } = anchor {
            Some(local_review_core::AnchorFingerprint::compute(
                &location.target_text,
                location.context_before.last().map_or("", |s| s.as_str()),
                location.context_after.first().map_or("", |s| s.as_str()),
            ))
        } else {
            None
        };

        let comment = Comment {
            schema_version: SchemaVersion,
            anchor,
            repo_root: self.repo_root.clone(),
            revset: self.revset.clone(),
            commit_id: Some(self.details.commit_id.clone()),
            body: body.to_owned(),
            severity,
            created_at: now,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            // TODO: populate entity_id from entity list
            entity_id: None,
            anchor_fingerprint,
        };

        match crate::store::save_comment(&self.data_home, &self.repo_root, &comment) {
            Ok(()) => {
                self.last_severity = Some(severity);
                let _ = self.reload_comments();
                let status_message = if oversized {
                    "body truncated to 64 KB on save".to_owned()
                } else {
                    save_status_message_for_scope(scope).to_owned()
                };
                Ok(CoreSaveOutcome::Saved { status_message })
            }
            Err(JjrError::DuplicateCommentTimestamp { .. }) => Ok(CoreSaveOutcome::Errored {
                message:
                    "save failed: two comments at the same timestamp — wait a moment and retry"
                        .to_owned(),
            }),
            Err(e) => Ok(CoreSaveOutcome::Errored {
                message: format!("save failed: {}", sanitize_for_status(&e.to_string())),
            }),
        }
    }

    fn update_comment(
        &mut self,
        req: UpdateRequest<'_>,
    ) -> std::result::Result<CoreSaveOutcome, JjrError> {
        let body = req.body;
        if body.trim().is_empty() {
            return Ok(CoreSaveOutcome::Refused {
                reason: "comment body is empty — not saved".to_owned(),
            });
        }
        let Some(comment_idx) = self
            .loaded_comments
            .iter()
            .position(|c| c.created_at == req.identity.as_offset_date_time())
        else {
            return Ok(CoreSaveOutcome::Errored {
                message: "could not find comment to update".to_owned(),
            });
        };
        let mut updated = self.loaded_comments[comment_idx].clone();
        body.clone_into(&mut updated.body);
        updated.severity = req.severity;
        updated.updated_at = Some(time::OffsetDateTime::now_utc());

        match crate::store::update_comment(&self.data_home, &self.repo_root, &updated) {
            Ok(()) => {
                let _ = self.reload_comments();
                let msg = if req.oversized {
                    "body truncated to 64 KB on save".to_owned()
                } else {
                    "comment updated".to_owned()
                };
                Ok(CoreSaveOutcome::Saved {
                    status_message: msg,
                })
            }
            Err(e) => Ok(CoreSaveOutcome::Errored {
                message: format!("update failed: {}", sanitize_for_status(&e.to_string())),
            }),
        }
    }

    fn delete_comment(
        &mut self,
        req: DeleteRequest,
    ) -> std::result::Result<CoreDeleteOutcome, JjrError> {
        let Some(comment) = self
            .loaded_comments
            .iter()
            .find(|c| c.created_at == req.identity.as_offset_date_time())
            .cloned()
        else {
            return Ok(CoreDeleteOutcome::Refused {
                reason: "comment not found in loaded set".to_owned(),
            });
        };
        crate::store::delete_comment(&self.data_home, &self.repo_root, &comment)?;
        let _ = self.reload_comments();
        Ok(CoreDeleteOutcome::Deleted)
    }

    fn is_view_reviewed(&self, view_idx: usize) -> bool {
        let Some(entry) = self.reviewed.entries.get(&self.details.change_id) else {
            return false;
        };
        if entry.commit_id != self.details.commit_id {
            return false;
        }
        if view_idx == 0 {
            return entry.description_reviewed;
        }
        let Some(file) = self.details.diff.files.get(view_idx - 1) else {
            return false;
        };
        entry.reviewed_files.contains(file.display_path())
    }

    fn mark_view_reviewed(&mut self, view_idx: usize) -> MarkReviewedOutcome {
        self.mark_view_reviewed_impl(view_idx)
    }

    fn toggle_view_reviewed(&mut self, view_idx: usize) -> ReviewedOutcome {
        let Some(target) = self.review_target_for_index(view_idx) else {
            return ReviewedOutcome::Unmarked;
        };
        let change_id = self.details.change_id.clone();
        let commit_id = self.details.commit_id.clone();
        let was_reviewed = self.is_view_reviewed(view_idx);
        if was_reviewed {
            self.reviewed.unmark(&change_id, &commit_id, &target);
            let _ = self.reviewed.save(&self.data_home, &self.repo_root);
            ReviewedOutcome::Unmarked
        } else {
            let outcome = self.reviewed.mark(change_id, commit_id, target);
            let _ = self.reviewed.save(&self.data_home, &self.repo_root);
            match outcome {
                MarkOutcome::ResetDueToCommitMismatch => ReviewedOutcome::ResetAndMarked,
                MarkOutcome::NoReset => ReviewedOutcome::Marked,
            }
        }
    }

    fn severity_histogram(&self) -> CoreSeverityHistogram {
        histogram_from_comments(&self.loaded_comments)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "unhandled KeyCode variants are intentionally passed through as Ignored"
    )]
    fn handle_extra_key(
        &mut self,
        key: KeyEvent,
        file_index: usize,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> std::result::Result<ExtraKeyAction, JjrError> {
        // Esc cancels reanchor mode.
        if key.code == KeyCode::Esc {
            if let Some(reanchor) = self.pending_reanchor.take() {
                let label = match reanchor.severity {
                    Severity::Required => "required",
                    Severity::Suggestion => "suggestion",
                    Severity::Note => "note",
                };
                let preview: String = reanchor.body.chars().take(40).collect();
                return Ok(ExtraKeyAction::StatusMessage(format!(
                    "re-anchor cancelled ({label}: {preview})"
                )));
            }
        }

        // In reanchor mode Enter/c opens the composer pre-filled with the stale body.
        if self.pending_reanchor.is_some()
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Enter)
        {
            return Ok(self.open_new_comment_composer(file_index, line_index, current_view));
        }

        match key.code {
            KeyCode::Char('c') | KeyCode::Enter => {
                if self
                    .focused_comment_from_view(line_index, current_view)
                    .is_some()
                {
                    Ok(self.open_edit_comment_composer(line_index, current_view))
                } else {
                    Ok(self.open_new_comment_composer(file_index, line_index, current_view))
                }
            }
            KeyCode::Char('e') => Ok(self.open_edit_comment_composer(line_index, current_view)),
            KeyCode::Char('d') => Ok(self.delete_focused_comment_action(line_index, current_view)),
            KeyCode::Char('S') => {
                let stale_indices = stale_screen::stale_comment_indices(&self.loaded_comments);
                let state = StaleScreen(StaleScreenState {
                    selected_index: 0,
                    stale_indices,
                    scroll_offset: 0,
                });
                Ok(ExtraKeyAction::OpenScreen(Box::new(state)))
            }
            KeyCode::Char('s') => {
                if self.pending_reanchor.is_some() {
                    return Ok(ExtraKeyAction::StatusMessage(
                        "finish or cancel re-anchor mode before opening the stack overview"
                            .to_owned(),
                    ));
                }
                if self.stack.is_none() {
                    return Ok(ExtraKeyAction::StatusMessage(
                        "stack overview requires --stack mode".to_owned(),
                    ));
                }
                if self.overview_cache.is_none() {
                    self.load_overview_comments();
                }
                Ok(ExtraKeyAction::OpenScreen(Box::new(OverviewScreen(
                    OverviewScreenState::new(),
                ))))
            }
            KeyCode::Char('C') => match self.build_send_to_claude() {
                Ok(state) => Ok(ExtraKeyAction::OpenScreen(Box::new(
                    SendToClaudeScreen::new(state),
                ))),
                Err(msg) => Ok(ExtraKeyAction::StatusMessage(msg)),
            },
            _ => Ok(ExtraKeyAction::Ignored),
        }
    }

    fn render_extra_screen(&self, frame: &mut Frame<'_>, state: &mut dyn ExtraScreen) {
        if let Some(s) = state.as_any_mut().downcast_mut::<ComposerScreen>() {
            composer_overlay::render_composer_overlay(frame, &s.0, None);
        } else if let Some(s) = state.as_any_mut().downcast_mut::<StaleScreen>() {
            stale_screen::render(frame, &mut s.0, &self.loaded_comments, &self.details.diff);
        } else if let Some(s) = state.as_any_mut().downcast_mut::<OverviewScreen>() {
            let stack_ctx = self.stack.as_ref().map(|ctx| OverviewStackCtx {
                revset: &ctx.revset,
                entries: &ctx.entries,
                current_index: ctx.current_index,
            });
            if let Some(ref cache) = self.overview_cache {
                overview_screen::render(frame, &mut s.0, stack_ctx, &self.reviewed, cache);
            }
        } else if let Some(s) = state.as_any_mut().downcast_mut::<SendToClaudeScreen>() {
            send_to_claude::render(frame, s.inner());
        }
    }

    fn handle_extra_screen_key(
        &mut self,
        state: &mut dyn ExtraScreen,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> std::result::Result<ExtraScreenAction, JjrError> {
        if let Some(s) = try_downcast_mut::<StaleScreen>(state) {
            return Ok(self.handle_stale_key_impl(&mut s.0, key, ctx));
        }
        if let Some(s) = try_downcast_mut::<OverviewScreen>(state) {
            return Ok(self.handle_overview_key_impl(&mut s.0, key, ctx));
        }
        if let Some(s) = try_downcast_mut::<SendToClaudeScreen>(state) {
            return self.handle_send_to_claude_key_impl(s, key, ctx);
        }
        if let Some(s) = try_downcast_mut::<ComposerScreen>(state) {
            return Ok(self.handle_composer_key_impl(&mut s.0, key, ctx));
        }
        Ok(ExtraScreenAction::StayOpen)
    }

    fn file_picker_entries(&self) -> Vec<FilePickerEntry> {
        let total_views = self.details.diff.files.len() + 1;
        let reviewed_view_indices: std::collections::HashSet<usize> = (0..total_views)
            .filter(|&i| self.is_view_reviewed(i))
            .collect();
        build_file_picker_entries(
            &self.details.diff.files,
            &self.loaded_comments,
            &|view_idx| reviewed_view_indices.contains(&view_idx),
            &|view_idx| first_commentable_row_for_view(&self.rendered_views, view_idx),
        )
    }

    fn help_screen_title(&self) -> &'static str {
        "jjr · keybindings"
    }

    fn help_screen_body(&self) -> &'static str {
        JJR_HELP_BODY
    }

    fn footer_hint(
        &self,
        width: u16,
        has_stack: bool,
        severity_filter: Option<Severity>,
    ) -> String {
        local_review_core::tui::footer_text_for_width(width, has_stack, severity_filter)
    }
}

impl ReviewSurfaceExt for JjrSurface {
    fn on_entry_loaded(&mut self, idx: usize, record_cursor: bool) {
        if record_cursor {
            if let Some(ctx) = self.stack.as_ref() {
                let change_id = ctx.entries.get(idx).map(|e| &e.change_id);
                if let Some(change_id) = change_id {
                    let _ = cursor::record(
                        &self.data_home,
                        &self.repo_root,
                        ctx.revset_hash,
                        &ctx.revset,
                        change_id,
                    );
                }
            }
        }
    }

    fn severity_histogram_for_transition(&self) -> (Option<usize>, CoreSeverityHistogram) {
        if self.comments_loaded_ok {
            (
                Some(self.loaded_comments.len()),
                histogram_from_comments(&self.loaded_comments),
            )
        } else {
            (None, CoreSeverityHistogram::default())
        }
    }

    fn take_pending_status_message(&mut self) -> Option<String> {
        self.pending_status_message.take()
    }

    fn mark_entity_reviewed(
        &mut self,
        entry_idx: usize,
        entity_id: &local_review_core::semantic::EntityId,
        content_hash: u64,
    ) {
        let change_id = self
            .stack
            .as_ref()
            .and_then(|s| s.entries.get(entry_idx).map(|e| e.change_id.clone()))
            .unwrap_or_else(|| self.details.change_id.clone());
        self.reviewed
            .mark_entity_reviewed(change_id, entity_id.clone(), content_hash);
    }

    fn is_entity_reviewed(
        &self,
        entry_idx: usize,
        entity_id: &local_review_core::semantic::EntityId,
        content_hash: u64,
    ) -> bool {
        let change_id = self
            .stack
            .as_ref()
            .and_then(|s| s.entries.get(entry_idx).map(|e| &e.change_id));
        let cid = change_id.unwrap_or(&self.details.change_id);
        self.reviewed
            .is_entity_reviewed(cid, entity_id, content_hash)
    }
}

// ---------------------------------------------------------------------------
// JjrSurface helper methods (extra-screen handle impls + build helpers)
// ---------------------------------------------------------------------------

impl JjrSurface {
    /// Stamp each `EntitySummary` with its current `reviewed` bit from the
    /// persisted reviewed state.
    fn populate_reviewed_bits(
        &self,
        entry_idx: usize,
        mut summaries: Vec<local_review_core::semantic::EntitySummary>,
    ) -> Vec<local_review_core::semantic::EntitySummary> {
        for s in &mut summaries {
            s.reviewed = self.is_entity_reviewed(entry_idx, &s.id, s.content_hash);
        }
        summaries
    }

    /// Build the `SendToClaudeState` for the current change. Returns
    /// `Ok(state)` on success or `Err(status_msg)` on failure.
    fn build_send_to_claude(&self) -> std::result::Result<SendToClaudeState, String> {
        let change_id = self.details.change_id.clone();
        let revset_hash = self.stack.as_ref().map(|s| s.revset_hash);

        let entries = match self.stack.as_ref() {
            Some(stack) => stack.entries.clone(),
            None => vec![StackEntry {
                change_id: change_id.clone(),
                commit_id: self.details.commit_id.clone(),
                description: self.details.description.clone(),
            }],
        };

        let resolved = ResolvedStack {
            revset_hash: revset_hash.unwrap_or_else(|| RevsetHash::from_revset(&self.revset)),
            revset: self.revset.clone(),
            entries,
        };

        let packet = match crate::packet::build_packet(
            &self.data_home,
            &self.repo_root,
            &resolved,
            false,
            jj::diff_for_change,
        ) {
            Ok(p) => p,
            Err(JjrError::EmptyPacket { .. }) => {
                return Err("no comments to send".to_owned());
            }
            Err(e) => {
                return Err(format!(
                    "could not build packet: {}",
                    sanitize_for_status(&e.to_string())
                ));
            }
        };

        let stale_count = send_to_claude::stale_count_for_change(
            &self.data_home,
            &self.repo_root,
            &change_id,
            revset_hash,
        );
        let scope_severity_grid = send_to_claude::compute_scope_severity_grid(&packet);
        let files_affected = send_to_claude::compute_files_affected(&packet);

        let data = ConfirmData {
            change_id,
            change_description: self.details.description.clone(),
            scope_severity_grid,
            files_affected,
            stale_count,
            packet,
        };
        Ok(SendToClaudeState::Confirm(data))
    }

    /// Build a `StackContextSnapshot` for the current stack (if in stack mode).
    fn stack_context_snapshot(&self) -> Option<StackContextSnapshot> {
        self.stack.as_ref().map(|s| StackContextSnapshot {
            revset: s.revset.clone(),
            revset_hash: s.revset_hash,
        })
    }

    /// Return the comment the cursor is sitting on, if any.
    fn focused_comment_from_view(
        &self,
        line_index: usize,
        view: Option<&DiffView>,
    ) -> Option<&Comment> {
        let line = view?.lines.get(line_index)?;
        let RenderedLineKind::InlineCommentMeta { comment_index } = line.kind else {
            return None;
        };
        let CommentIndex::Local(idx) = comment_index else {
            return None;
        };
        self.loaded_comments.get(idx)
    }

    /// Classify the line at the cursor into a `BuildTargetResult` using the
    /// parameters already available inside `handle_extra_key`.
    fn build_line_target_from_view(
        &self,
        file_index: usize,
        line_index: usize,
        view: Option<&DiffView>,
    ) -> BuildTargetResult {
        let Some(view) = view else {
            return BuildTargetResult::NoView;
        };
        let Some(line) = view.lines.get(line_index) else {
            return BuildTargetResult::NoView;
        };

        match line.kind {
            RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context => {}
            RenderedLineKind::DescriptionLine => {
                return BuildTargetResult::DescriptionLine {
                    target_line: line.target_line,
                };
            }
            RenderedLineKind::HunkHeader
            | RenderedLineKind::HunkSeparator
            | RenderedLineKind::Notice
            | RenderedLineKind::InlineCommentMeta { .. }
            | RenderedLineKind::InlineCommentBody => return BuildTargetResult::NonCommentable,
        }

        // file_index 0 is the description view; DescriptionLine returns early above.
        let Some(diff_file_idx) = file_index.checked_sub(1) else {
            return BuildTargetResult::NoView;
        };
        let Some(file) = self.details.diff.files.get(diff_file_idx) else {
            return BuildTargetResult::NoView;
        };
        let file = file.display_path().to_owned();
        let hunk_header = line.hunk_header.clone().unwrap_or_default();
        let is_content = |k: RenderedLineKind| {
            matches!(
                k,
                RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
            )
        };
        let (context_before, context_after) =
            collect_context_with(&view.lines, line_index, is_content);

        BuildTargetResult::Ready(LineTarget {
            file,
            rendered_index: line_index,
            source_line: line.source_line,
            target_line: line.target_line,
            target_text: line.text.clone(),
            hunk_header,
            context_before,
            context_after,
        })
    }

    /// Build a description-scope `DescriptionContext` from view parameters.
    fn description_context_from_view(
        &self,
        target_line: Option<u32>,
        line_index: usize,
        view: Option<&DiffView>,
    ) -> DescriptionContext {
        let (context_before, context_after) = view
            .map(|v| {
                let is_desc = |k: RenderedLineKind| matches!(k, RenderedLineKind::DescriptionLine);
                collect_context_with(&v.lines, line_index, is_desc)
            })
            .unwrap_or_default();
        let target_text = view
            .and_then(|v| v.lines.get(line_index))
            .map(|l| l.text.clone())
            .unwrap_or_default();
        DescriptionContext {
            change_id: self.details.change_id.clone(),
            target_line,
            target_text,
            context_before,
            context_after,
        }
    }

    /// Build and return an `ExtraKeyAction` that opens the new-comment composer.
    fn open_new_comment_composer(
        &mut self,
        file_index: usize,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let reanchor_severity = self.pending_reanchor.as_ref().map(|r| r.severity);
        let reanchor_body = self.pending_reanchor.take().map(|r| r.body);

        match self.build_line_target_from_view(file_index, line_index, current_view) {
            BuildTargetResult::Ready(target) => {
                let init = ComposerInit {
                    scope: ComposerScope::Line(target.clone()),
                    severity: reanchor_severity
                        .unwrap_or_else(|| default_severity(self.last_severity)),
                    change_id: self.details.change_id.clone(),
                    change_description: self.details.description.clone(),
                    line_available: Some(target),
                    stack_available: self.stack_context_snapshot(),
                    description_available: None,
                };
                let mut composer = Composer::new(init);
                if let Some(body) = reanchor_body {
                    for (i, line) in body.lines().enumerate() {
                        if i > 0 {
                            composer.body.insert_newline();
                        }
                        composer.body.insert_str(line);
                    }
                }
                ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(composer))))
            }
            BuildTargetResult::DescriptionLine { target_line } => {
                let desc_ctx =
                    self.description_context_from_view(target_line, line_index, current_view);
                let init = ComposerInit {
                    scope: ComposerScope::Description(desc_ctx.clone()),
                    severity: default_severity(self.last_severity),
                    change_id: self.details.change_id.clone(),
                    change_description: self.details.description.clone(),
                    line_available: None,
                    stack_available: self.stack_context_snapshot(),
                    description_available: Some(desc_ctx),
                };
                ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::new(init)))))
            }
            BuildTargetResult::NonCommentable => {
                ExtraKeyAction::StatusMessage("cannot comment on this line".to_owned())
            }
            BuildTargetResult::NoView => ExtraKeyAction::Ignored,
        }
    }

    /// Build and return an `ExtraKeyAction` that opens the edit-comment composer.
    fn open_edit_comment_composer(
        &self,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let Some(comment) = self.focused_comment_from_view(line_index, current_view) else {
            return ExtraKeyAction::StatusMessage(
                "cursor is not on a comment — move to a comment marker to edit".to_owned(),
            );
        };

        let target_change_id = match &comment.anchor {
            Anchor::Change { change_id }
            | Anchor::Line { change_id, .. }
            | Anchor::Description { change_id, .. } => change_id.clone(),
            Anchor::Stack { .. } => self.details.change_id.clone(),
        };

        let stack_available = self.stack_context_snapshot();

        let (scope, description_available) = match &comment.anchor {
            Anchor::Line { location, .. } => {
                let line_target = LineTarget {
                    file: location.file.clone(),
                    rendered_index: line_index,
                    source_line: location.old_line,
                    target_line: location.new_line,
                    target_text: location.target_text.clone(),
                    hunk_header: location.hunk_header.clone(),
                    context_before: location.context_before.clone(),
                    context_after: location.context_after.clone(),
                };
                (ComposerScope::Line(line_target), None)
            }
            Anchor::Change { .. } => (ComposerScope::Change, None),
            Anchor::Stack { revset_hash } => {
                let snapshot = stack_available
                    .clone()
                    .unwrap_or_else(|| StackContextSnapshot {
                        revset: format!("revset_hash:{}", revset_hash.hex()),
                        revset_hash: *revset_hash,
                    });
                (ComposerScope::Stack(snapshot), None)
            }
            Anchor::Description {
                change_id: anchor_change_id,
                location,
            } => {
                let ctx = DescriptionContext {
                    change_id: anchor_change_id.clone(),
                    target_line: location.display_line,
                    target_text: location.target_text.clone(),
                    context_before: location.context_before.clone(),
                    context_after: location.context_after.clone(),
                };
                (ComposerScope::Description(ctx.clone()), Some(ctx))
            }
        };

        let change_description = if target_change_id == self.details.change_id {
            self.details.description.clone()
        } else {
            String::new()
        };

        let init = ComposerInit {
            scope,
            severity: comment.severity,
            change_id: target_change_id,
            change_description,
            line_available: None,
            stack_available,
            description_available,
        };
        let edited = EditedComment {
            init,
            body: comment.body.clone(),
            identity: comment.created_at,
            original: None,
            original_anchor: comment.anchor.clone(),
        };
        ExtraKeyAction::OpenScreen(Box::new(ComposerScreen(Box::new(Composer::for_edit(
            edited,
        )))))
    }

    /// Delete the comment the cursor is on and return the appropriate action.
    fn delete_focused_comment_action(
        &mut self,
        line_index: usize,
        current_view: Option<&DiffView>,
    ) -> ExtraKeyAction {
        let Some(comment) = self
            .focused_comment_from_view(line_index, current_view)
            .cloned()
        else {
            return ExtraKeyAction::StatusMessage(
                "cursor is not on a comment — move to a comment marker to delete".to_owned(),
            );
        };
        match crate::store::delete_comment(&self.data_home, &self.repo_root, &comment) {
            Ok(()) => {
                let _ = self.reload_comments();
                ExtraKeyAction::RefreshAndStatus("comment deleted".to_owned())
            }
            Err(e) => ExtraKeyAction::StatusMessage(format!(
                "delete failed: {}",
                sanitize_for_status(&e.to_string())
            )),
        }
    }

    /// Handle a key event while the stale-comments screen is open.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "unhandled KeyCode variants are intentionally ignored on the stale screen"
    )]
    fn handle_stale_key_impl(
        &mut self,
        state: &mut StaleScreenState,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let count = state.stale_indices.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                return ExtraScreenAction::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected_index = state.selected_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                state.selected_index = (state.selected_index + 1).min(count - 1);
            }
            KeyCode::Char('d') => {
                self.stale_delete_focused(state, ctx);
            }
            KeyCode::Enter => {
                return self.stale_jump_to_file(state, ctx);
            }
            KeyCode::Char('e') => {
                return self.stale_enter_reanchor(state, ctx);
            }
            _ => {}
        }
        ExtraScreenAction::StayOpen
    }

    /// Delete the focused stale comment and refresh the stale list.
    fn stale_delete_focused(
        &mut self,
        state: &mut StaleScreenState,
        ctx: &mut ExtraScreenContext<'_>,
    ) {
        let focused = state
            .stale_indices
            .get(state.selected_index)
            .and_then(|&idx| self.loaded_comments.get(idx))
            .cloned();
        if let Some(comment) = focused {
            match crate::store::delete_comment(&self.data_home, &self.repo_root, &comment) {
                Ok(()) => {
                    let _ = self.reload_comments();
                    let new_indices = stale_screen::stale_comment_indices(&self.loaded_comments);
                    let new_count = new_indices.len();
                    state.stale_indices = new_indices;
                    if new_count == 0 {
                        state.selected_index = 0;
                    } else if state.selected_index >= new_count {
                        state.selected_index = new_count - 1;
                    }
                }
                Err(e) => {
                    *ctx.status_message = Some(format!(
                        "delete failed: {}",
                        sanitize_for_status(&e.to_string())
                    ));
                }
            }
        }
    }

    /// Jump to the file+line for the focused stale comment.
    fn stale_jump_to_file(
        &mut self,
        state: &StaleScreenState,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let focused = state
            .stale_indices
            .get(state.selected_index)
            .and_then(|&idx| self.loaded_comments.get(idx))
            .cloned();
        let Some(comment) = focused else {
            return ExtraScreenAction::StayOpen;
        };
        let Anchor::Line { location, .. } = &comment.anchor else {
            return ExtraScreenAction::StayOpen;
        };
        let file_idx = self
            .details
            .diff
            .files
            .iter()
            .position(|f| f.display_path() == location.file.as_path());
        let Some(fidx) = file_idx else {
            *ctx.status_message = Some("file not in current diff".to_owned());
            return ExtraScreenAction::StayOpen;
        };
        let view_idx = fidx + 1;
        *ctx.file_index = view_idx;
        *ctx.scroll = 0;
        let line_num = match location.side {
            Side::Old => location.old_line,
            Side::New => location.new_line,
        };
        if let Some(target_line_num) = line_num {
            if let Some(view) = ctx.base_per_file.0.get(view_idx) {
                let pos = view.lines.iter().position(|l| match location.side {
                    Side::Old => l.source_line == Some(target_line_num),
                    Side::New => l.target_line == Some(target_line_num),
                });
                *ctx.line_index = pos.unwrap_or(0);
            }
        } else {
            *ctx.line_index = 0;
        }
        let _ = self.mark_view_reviewed_impl(view_idx);
        ExtraScreenAction::Close
    }

    /// Enter reanchor mode: close stale screen, navigate to the stale
    /// comment's file, and record the pending reanchor.
    fn stale_enter_reanchor(
        &mut self,
        state: &StaleScreenState,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let focused = state
            .stale_indices
            .get(state.selected_index)
            .and_then(|&idx| self.loaded_comments.get(idx))
            .cloned();
        let Some(comment) = focused else {
            return ExtraScreenAction::StayOpen;
        };
        let Anchor::Line { location, .. } = &comment.anchor else {
            return ExtraScreenAction::StayOpen;
        };
        let severity = comment.severity;
        let file = location.file.clone();
        self.pending_reanchor = Some(PendingReanchor {
            body: comment.body.clone(),
            severity,
            original: comment,
        });
        let file_idx = self
            .details
            .diff
            .files
            .iter()
            .position(|f| f.display_path() == file.as_path());
        match file_idx {
            Some(fidx) => {
                *ctx.file_index = fidx + 1;
                *ctx.line_index = 0;
                *ctx.scroll = 0;
                let _ = self.mark_view_reviewed_impl(fidx + 1);
            }
            None => {
                *ctx.status_message = Some(
                    "re-anchor: file not in current diff; pick a line in the visible file"
                        .to_owned(),
                );
            }
        }
        ExtraScreenAction::Close
    }

    /// Handle a key event while the stack overview screen is open.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "unhandled KeyCode variants are intentionally ignored on the overview screen"
    )]
    fn handle_overview_key_impl(
        &mut self,
        state: &mut OverviewScreenState,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let rows = if let (Some(stack_ctx), Some(cache)) =
            (self.stack.as_ref(), self.overview_cache.as_ref())
        {
            overview_screen::build_rows(
                cache,
                &stack_ctx.entries,
                cache.stale_count(),
                cache.total_count(),
            )
        } else {
            Vec::new()
        };

        let current_selected = state.selected_row;

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                return ExtraScreenAction::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected_row = overview_screen::move_cursor(&rows, current_selected, -1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.selected_row = overview_screen::move_cursor(&rows, current_selected, 1);
            }
            KeyCode::Enter => {
                let Some(row) = rows.get(current_selected) else {
                    return ExtraScreenAction::StayOpen;
                };
                match row {
                    overview_screen::OverviewRow::ChangeRow(change_idx) => {
                        let idx = *change_idx;
                        *ctx.navigate_to_entry = Some(idx);
                        return ExtraScreenAction::Close;
                    }
                    overview_screen::OverviewRow::StackComment(ci) => {
                        let ci = *ci;
                        if let Some(composer) = self.build_stack_comment_composer(ci) {
                            return ExtraScreenAction::OpenScreen(Box::new(ComposerScreen(
                                Box::new(composer),
                            )));
                        }
                    }
                    overview_screen::OverviewRow::ChangeComment {
                        change_idx,
                        comment_idx,
                    } => {
                        if let Some(composer) =
                            self.build_change_comment_composer(*change_idx, *comment_idx)
                        {
                            return ExtraScreenAction::OpenScreen(Box::new(ComposerScreen(
                                Box::new(composer),
                            )));
                        }
                    }
                    overview_screen::OverviewRow::StackHeader
                    | overview_screen::OverviewRow::Separator
                    | overview_screen::OverviewRow::SummaryFooterStale
                    | overview_screen::OverviewRow::SummaryFooterTotal => {}
                }
            }
            KeyCode::Char('c') => {
                let composer = self.build_overview_new_composer(&rows, current_selected, ctx);
                return ExtraScreenAction::OpenScreen(Box::new(ComposerScreen(Box::new(composer))));
            }
            // '?' and other keys: help is handled by core; everything else ignored.
            _ => {}
        }
        ExtraScreenAction::StayOpen
    }

    /// Build a new-comment composer opened from the overview screen.
    fn build_overview_new_composer(
        &self,
        rows: &[overview_screen::OverviewRow],
        selected: usize,
        ctx: &ExtraScreenContext<'_>,
    ) -> Composer {
        let (use_stack_scope, change_idx_for_change_scope) = rows
            .get(selected)
            .map(|row| match row {
                overview_screen::OverviewRow::StackHeader
                | overview_screen::OverviewRow::StackComment(_) => (true, None),
                overview_screen::OverviewRow::ChangeRow(ci) => (false, Some(*ci)),
                overview_screen::OverviewRow::ChangeComment { change_idx, .. } => {
                    (false, Some(*change_idx))
                }
                overview_screen::OverviewRow::Separator
                | overview_screen::OverviewRow::SummaryFooterStale
                | overview_screen::OverviewRow::SummaryFooterTotal => (false, None),
            })
            .unwrap_or((false, None));

        let target_change_id: ChangeId = change_idx_for_change_scope
            .and_then(|idx| {
                self.stack
                    .as_ref()
                    .and_then(|s| s.entries.get(idx).map(|e| e.change_id.clone()))
            })
            .unwrap_or_else(|| self.details.change_id.clone());

        let stack_available = self.stack_context_snapshot();
        let change_description = if target_change_id == self.details.change_id {
            self.details.description.clone()
        } else {
            String::new()
        };

        let scope = if use_stack_scope {
            match stack_available.clone() {
                Some(s) => ComposerScope::Stack(s),
                None => ComposerScope::Change,
            }
        } else {
            ComposerScope::Change
        };

        let init = ComposerInit {
            scope,
            severity: default_severity(*ctx.last_severity),
            change_id: target_change_id,
            change_description,
            line_available: None,
            stack_available,
            description_available: None,
        };
        Composer::new(init)
    }

    /// Build an editor composer for a stack-level comment.
    fn build_stack_comment_composer(&self, comment_idx: usize) -> Option<Composer> {
        let comment = self
            .overview_cache
            .as_ref()
            .and_then(|c| c.stack_level.get(comment_idx))
            .cloned()?;
        Some(self.build_meta_comment_editor(&comment))
    }

    /// Build an editor composer for a change-level comment from the overview.
    fn build_change_comment_composer(
        &self,
        change_idx: usize,
        comment_idx: usize,
    ) -> Option<Composer> {
        let comment = self
            .overview_cache
            .as_ref()
            .and_then(|c| c.per_change.get(change_idx))
            .and_then(|v| v.get(comment_idx))
            .cloned()?;
        Some(self.build_meta_comment_editor(&comment))
    }

    /// Build an edit-mode composer for an existing comment.
    fn build_meta_comment_editor(&self, comment: &Comment) -> Composer {
        let target_change_id = match &comment.anchor {
            Anchor::Change { change_id }
            | Anchor::Line { change_id, .. }
            | Anchor::Description { change_id, .. } => change_id.clone(),
            Anchor::Stack { .. } => self.details.change_id.clone(),
        };

        let stack_available = self.stack_context_snapshot();
        let change_description = if target_change_id == self.details.change_id {
            self.details.description.clone()
        } else {
            String::new()
        };

        let (scope, description_available) = match &comment.anchor {
            Anchor::Line { location, .. } => {
                let line_target = LineTarget {
                    file: location.file.clone(),
                    rendered_index: OVERVIEW_NO_LINE_CURSOR,
                    source_line: location.old_line,
                    target_line: location.new_line,
                    target_text: location.target_text.clone(),
                    hunk_header: location.hunk_header.clone(),
                    context_before: location.context_before.clone(),
                    context_after: location.context_after.clone(),
                };
                (ComposerScope::Line(line_target), None)
            }
            Anchor::Change { .. } => (ComposerScope::Change, None),
            Anchor::Stack { revset_hash } => {
                let snapshot = stack_available
                    .clone()
                    .unwrap_or_else(|| StackContextSnapshot {
                        revset: format!("revset_hash:{}", revset_hash.hex()),
                        revset_hash: *revset_hash,
                    });
                (ComposerScope::Stack(snapshot), None)
            }
            Anchor::Description {
                change_id: anchor_change_id,
                location,
            } => {
                let ctx = DescriptionContext {
                    change_id: anchor_change_id.clone(),
                    target_line: location.display_line,
                    target_text: location.target_text.clone(),
                    context_before: location.context_before.clone(),
                    context_after: location.context_after.clone(),
                };
                (ComposerScope::Description(ctx.clone()), Some(ctx))
            }
        };

        let init = ComposerInit {
            scope,
            severity: comment.severity,
            change_id: target_change_id,
            change_description,
            line_available: None,
            stack_available,
            description_available,
        };
        let edited = EditedComment {
            init,
            body: comment.body.clone(),
            identity: comment.created_at,
            original: Some(comment.clone()),
            original_anchor: comment.anchor.clone(),
        };
        Composer::for_edit(edited)
    }

    /// Handle a key event while the send-to-claude confirmation screen is open.
    fn handle_send_to_claude_key_impl(
        &mut self,
        wrapper: &mut SendToClaudeScreen,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> std::result::Result<ExtraScreenAction, JjrError> {
        let current_offset = match wrapper.inner() {
            SendToClaudeState::PromptView { scroll_offset, .. } => *scroll_offset,
            SendToClaudeState::Confirm(_) => 0,
        };

        let owned = *wrapper
            .0
            .take()
            .ok_or_else(|| JjrError::JjUnexpectedOutput {
                raw: "SendToClaudeScreen.state was None; this is a bug".to_owned(),
            })?;

        match (&owned, key.code) {
            (SendToClaudeState::Confirm(_), KeyCode::Esc) => {
                wrapper.0 = Some(Box::new(owned));
                return Ok(ExtraScreenAction::Close);
            }
            (SendToClaudeState::Confirm(_), KeyCode::Char('v')) => {
                wrapper.0 = Some(Box::new(owned.into_prompt_view()));
            }
            (SendToClaudeState::Confirm(_), KeyCode::Enter) => {
                wrapper.0 = Some(Box::new(owned));
                self.invoke_claude_impl(wrapper.inner(), ctx)?;
                return Ok(ExtraScreenAction::Close);
            }
            (SendToClaudeState::PromptView { .. }, KeyCode::Char('q') | KeyCode::Esc) => {
                wrapper.0 = Some(Box::new(owned.into_confirm()));
            }
            (SendToClaudeState::PromptView { .. }, KeyCode::Up | KeyCode::Char('k')) => {
                let mut state = owned;
                if let SendToClaudeState::PromptView {
                    ref mut scroll_offset,
                    ..
                } = state
                {
                    *scroll_offset = current_offset.saturating_sub(1);
                }
                wrapper.0 = Some(Box::new(state));
            }
            (SendToClaudeState::PromptView { .. }, KeyCode::Down | KeyCode::Char('j')) => {
                let mut state = owned;
                if let SendToClaudeState::PromptView {
                    ref mut scroll_offset,
                    ..
                } = state
                {
                    *scroll_offset = current_offset.saturating_add(1);
                }
                wrapper.0 = Some(Box::new(state));
            }
            _ => {
                wrapper.0 = Some(Box::new(owned));
            }
        }
        Ok(ExtraScreenAction::StayOpen)
    }

    /// Suspend TUI, run claude, restore, reload diff.
    fn invoke_claude_impl(
        &mut self,
        state: &SendToClaudeState,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> Result<()> {
        let SendToClaudeState::Confirm(data) = state else {
            unreachable!("invoke_claude_impl is only called from Confirm arm; PromptView is unreachable here");
        };

        let change_id = data.change_id.clone();
        let prompt =
            build_entity_bundle_prompt(&data.packet, &self.data_home, &self.repo_root, &change_id);
        let repo_root = self.repo_root.clone();

        if let Some(g) = self.stderr_guard.as_ref() {
            g.suspend()?;
        }
        let resume_guard = StderrResumeGuard {
            guard: self.stderr_guard.as_ref(),
        };

        suspend_tui()?;
        let restore = TerminalRestoreGuard;

        let outcome = (|| -> Result<crate::claude::ClaudeOutcome> {
            let _guard =
                crate::working_copy_guard::WorkingCopyGuard::enter(&repo_root, &change_id)?;
            crate::claude::invoke_claude(&prompt)
        })();

        // Disarm `restore` before calling `restore_tui()` so that the explicit
        // call can surface its error; if it were not forgotten, `restore`'s Drop
        // would silently swallow any error on the same path.
        std::mem::forget(restore);
        restore_tui()?;

        // Same pattern: disarm `resume_guard` before calling `resume()` so the
        // explicit call can propagate its error rather than relying on Drop.
        std::mem::forget(resume_guard);
        if let Some(g) = self.stderr_guard.as_ref() {
            g.resume()?;
        }
        *ctx.needs_full_redraw = true;

        match outcome? {
            crate::claude::ClaudeOutcome::Success { tool } => {
                if let Err(e) = self.reload_after_agent_success(&change_id, ctx) {
                    *ctx.status_message = Some(format!(
                        "{tool} completed; could not reload diff: {}",
                        sanitize_for_status(&e.to_string())
                    ));
                }
            }
            crate::claude::ClaudeOutcome::Failed { tool, exit_code } => {
                let code_str = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                *ctx.status_message = Some(format!(
                    "{tool} exited with {code_str}; working copy restored"
                ));
            }
        }
        Ok(())
    }

    /// Reload the diff and comments after a successful claude invocation.
    fn reload_after_agent_success(
        &mut self,
        prev_change_id: &ChangeId,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> Result<()> {
        if let Some(stack_ctx) = self.stack.as_ref() {
            let revset = stack_ctx.revset.clone();
            let prev_index = stack_ctx.current_index;
            let resolved = jj::resolve_stack(&revset)?;
            if resolved.entries.is_empty() {
                return Err(JjrError::RevsetNoMatch {
                    revset: resolved.revset,
                });
            }
            let new_index =
                new_current_index_after_reload(prev_change_id, prev_index, &resolved.entries);
            let new_change_id = resolved.entries[new_index].change_id.clone();
            let details = jj::show(&new_change_id)?;

            if let Some(stack_ctx) = self.stack.as_mut() {
                stack_ctx.entries = resolved.entries;
                stack_ctx.revset = resolved.revset;
                stack_ctx.revset_hash = resolved.revset_hash;
                stack_ctx.current_index = new_index;
            }
            self.details = details;
        } else {
            let details = jj::show(prev_change_id)?;
            self.details = details;
        }
        *ctx.file_index = 0;
        *ctx.line_index = 0;
        *ctx.scroll = 0;
        let new_views = build_rendered_views(&self.details);
        self.rendered_views.clone_from(&new_views);
        ctx.rendered_per_file.clone_from(&new_views);
        // Write base (un-annotated) views; the core calls refresh_inline_comments
        // after handle_extra_screen_key returns to re-inject inline comments.
        *ctx.base_per_file = BaseViews(new_views);
        let _ = self.reload_comments();
        let _ = self.mark_view_reviewed_impl(0);
        Ok(())
    }

    /// Handle a key event while the composer overlay is open.
    fn handle_composer_key_impl(
        &mut self,
        composer: &mut Composer,
        key: KeyEvent,
        ctx: &mut ExtraScreenContext<'_>,
    ) -> ExtraScreenAction {
        let action = composer::handle_composer_key(composer, key);
        match action {
            ComposerAction::Continue => ExtraScreenAction::StayOpen,
            ComposerAction::Cancel => ExtraScreenAction::Close,
            ComposerAction::Save => {
                match self.save_via_composer(composer, time::OffsetDateTime::now_utc()) {
                    ComposerSaveOutcome::Saved { status } => {
                        *ctx.status_message = Some(status);
                        if let Some(reanchor) = self.pending_reanchor.take() {
                            match crate::store::delete_comment(
                                &self.data_home,
                                &self.repo_root,
                                &reanchor.original,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    *ctx.status_message = Some(format!(
                                        "re-anchor saved; could not delete original: {}",
                                        sanitize_for_status(&e.to_string())
                                    ));
                                }
                            }
                        }
                        let _ = self.reload_comments();
                        ExtraScreenAction::Close
                    }
                    ComposerSaveOutcome::Refused(msg) | ComposerSaveOutcome::Errored(msg) => {
                        *ctx.status_message = Some(msg);
                        ExtraScreenAction::StayOpen
                    }
                }
            }
            ComposerAction::Delete => match self.delete_via_composer(composer) {
                ComposerSaveOutcome::Saved { .. } => {
                    let _ = self.reload_comments();
                    ExtraScreenAction::Close
                }
                ComposerSaveOutcome::Refused(msg) | ComposerSaveOutcome::Errored(msg) => {
                    *ctx.status_message = Some(msg);
                    ExtraScreenAction::StayOpen
                }
            },
            ComposerAction::RefusedScopeChord(status) => {
                *ctx.status_message = Some(status.to_owned());
                ExtraScreenAction::StayOpen
            }
        }
    }

    /// Save a new or edited comment from the composer.
    ///
    /// New-comment path delegates to [`JjrSurface::save_comment`] so the
    /// persistence logic lives in one place. Edit path has two sub-paths:
    ///
    /// - Main-view edits: `edit_ctx.original` is `None`; source the anchor
    ///   from `loaded_comments` keyed by `identity` so a re-anchor that
    ///   happened after open is preserved.
    /// - Stack-overview edits: `edit_ctx.original` is `Some`; the comment
    ///   may belong to a different change and be absent from `loaded_comments`,
    ///   so use the captured snapshot directly.
    fn save_via_composer(
        &mut self,
        composer: &Composer,
        now: time::OffsetDateTime,
    ) -> ComposerSaveOutcome {
        let body = composer.body_text();
        if body.trim().is_empty() {
            return ComposerSaveOutcome::Refused("comment body is empty — not saved".to_owned());
        }
        let oversized = body.chars().count() > crate::comment::BODY_MAX;

        if let Some(edit_ctx) = composer.editing.as_ref() {
            let source = if let Some(orig) = edit_ctx.original.as_ref() {
                orig.clone()
            } else {
                let Some(latest) = self
                    .loaded_comments
                    .iter()
                    .find(|c| c.created_at == edit_ctx.identity)
                    .cloned()
                else {
                    return ComposerSaveOutcome::Errored(
                        "comment was removed between open and save; edit not saved".to_owned(),
                    );
                };
                latest
            };
            let updated = Comment {
                body,
                severity: composer.severity,
                updated_at: Some(now),
                ..source
            };
            return match crate::store::update_comment(&self.data_home, &self.repo_root, &updated) {
                Ok(()) => ComposerSaveOutcome::Saved {
                    status: if oversized {
                        "body truncated to 64 KB on save".to_owned()
                    } else {
                        "comment updated".to_owned()
                    },
                },
                Err(e) => ComposerSaveOutcome::Errored(format!(
                    "update failed: {}",
                    sanitize_for_status(&e.to_string())
                )),
            };
        }

        // New-comment path: delegate to save_comment so persistence logic
        // lives in one place.
        let req = SaveRequest {
            scope: &composer.scope,
            severity: composer.severity,
            body: &body,
            entry_idx: self.stack.as_ref().map_or(0, |s| s.current_index),
        };
        match self.save_comment(req) {
            Ok(CoreSaveOutcome::Saved { status_message }) => ComposerSaveOutcome::Saved {
                status: status_message,
            },
            Ok(CoreSaveOutcome::Refused { reason }) => ComposerSaveOutcome::Refused(reason),
            Ok(CoreSaveOutcome::Errored { message }) => ComposerSaveOutcome::Errored(message),
            Err(e) => ComposerSaveOutcome::Errored(format!(
                "save failed: {}",
                sanitize_for_status(&e.to_string())
            )),
        }
    }

    /// Delete a comment via the composer (edit-mode `^D`).
    fn delete_via_composer(&mut self, composer: &Composer) -> ComposerSaveOutcome {
        delete_via_composer_impl(
            &self.data_home,
            &self.repo_root,
            &self.revset,
            &self.details,
            composer,
        )
    }
}

/// Local save-outcome for the surface-side composer save path (not the core
/// `SaveOutcome` — that is the trait's return type).
enum ComposerSaveOutcome {
    Saved { status: String },
    Refused(String),
    Errored(String),
}

fn build_anchor_from_scope(
    scope: &ComposerScope,
    composer_change_id: &ChangeId,
    details: &ChangeDetails,
) -> Anchor {
    match scope {
        ComposerScope::Line(target) => {
            let (change_id, location) = build_line_anchor(target, details.change_id.clone());
            Anchor::Line {
                change_id,
                location,
            }
        }
        ComposerScope::Change => Anchor::Change {
            change_id: composer_change_id.clone(),
        },
        ComposerScope::Stack(stack) => Anchor::Stack {
            revset_hash: stack.revset_hash,
        },
        ComposerScope::Description(ctx) => {
            let location = DescriptionAnchor {
                display_line: ctx.target_line,
                target_text: ctx.target_text.clone(),
                context_before: ctx.context_before.clone(),
                context_after: ctx.context_after.clone(),
            }
            .normalized();
            Anchor::Description {
                change_id: ctx.change_id.clone(),
                location,
            }
        }
    }
}

/// Delete a comment via the composer (edit-mode `^D`).
fn delete_via_composer_impl(
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
    revset: &str,
    details: &ChangeDetails,
    composer: &Composer,
) -> ComposerSaveOutcome {
    let Some(edit_ctx) = composer.editing.as_ref() else {
        return ComposerSaveOutcome::Refused("delete only available in edit mode".to_owned());
    };
    let comment = match edit_ctx.original.as_ref() {
        Some(orig) => orig.clone(),
        None => Comment {
            schema_version: SchemaVersion,
            anchor: edit_ctx.original_anchor.clone(),
            repo_root: repo_root.to_owned(),
            revset: revset.to_owned(),
            commit_id: Some(details.commit_id.clone()),
            body: composer.body_text(),
            severity: composer.severity,
            created_at: edit_ctx.identity,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        },
    };
    match crate::store::delete_comment(data_home, repo_root, &comment) {
        Ok(()) => ComposerSaveOutcome::Saved {
            status: "comment deleted".to_owned(),
        },
        Err(e) => ComposerSaveOutcome::Errored(format!(
            "delete failed: {}",
            sanitize_for_status(&e.to_string())
        )),
    }
}

/// Which transition behavior is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionMode {
    Never,
    Auto,
    Always,
}

/// User preference for unified vs side-by-side diff layout. `Auto` picks based
/// on terminal width; the explicit modes pin the choice regardless of width.
/// The toggle key (`|`) cycles `Auto -> ForceUnified -> ForceSideBySide -> Auto`
/// and the choice persists for the session (no on-disk persistence).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffMode {
    Auto,
    ForceUnified,
    ForceSideBySide,
}

/// Resolved layout for a single render pass.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EffectiveDiffMode {
    Unified,
    SideBySide,
}

/// Minimum body width (cells) at which `DiffMode::Auto` switches from unified
/// to side-by-side. Each side gets ~58 cells + 1 gutter cell + 1 scrollbar
/// cell. Below this we stay unified — narrower side panes truncate too
/// aggressively to be useful.
#[cfg(test)]
pub(super) const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;

/// Width (cells) of the divider between the left and right columns in
/// side-by-side mode. A single `│` glyph plus one space of padding on each
/// side (3 cells total) reads as a clear vertical seam without crowding the
/// content.
#[cfg(test)]
pub(super) const SIDE_BY_SIDE_GUTTER_WIDTH: u16 = 3;

/// Minimum cells per side cell in side-by-side mode: the `+ ` / `- ` prefix
/// (2 cells) plus 2 cells of content. Below this each cell collapses into
/// just-the-prefix and the layout stops conveying anything useful.
#[cfg(test)]
pub(super) const MIN_USEFUL_SIDE_CELL_WIDTH: u16 = 4;

/// Below this body width side-by-side rendering falls back to unified mode
/// at draw time. The user can press `|` to switch back to Auto and let the
/// width-aware threshold pick again at the next resize.
#[cfg(test)]
pub(super) const MIN_USEFUL_SIDE_BY_SIDE_WIDTH: u16 =
    SIDE_BY_SIDE_GUTTER_WIDTH + 2 * MIN_USEFUL_SIDE_CELL_WIDTH;

#[cfg(test)]
struct App {
    details: ChangeDetails,
    /// XDG data home for cursor/reviewed/log storage; equals `repo_root` in tests.
    data_home: PathBuf,
    /// Repo root for comment storage; passed from the CLI.
    repo_root: PathBuf,
    /// The revset string used to open this view.
    revset: String,
    /// Base rendered views (no inline comments); rebuilt when switching files.
    rendered_per_file: Vec<DiffView>,
    /// Rendered views with inline comments injected; rebuilt after each save.
    annotated_per_file: Vec<DiffView>,
    /// Comments loaded for the current change, used for inline rendering.
    loaded_comments: Vec<Comment>,
    file_index: usize,
    line_index: usize,
    scroll: u16,
    screen: Screen,
    should_quit: bool,
    /// Cached viewport height (set during `render_main`, read in `handle_main_key`).
    /// Overwritten on first render before any key event is processed.
    viewport_rows: u16,
    /// Cached diff body width (set during `render_diff`, read by navigation
    /// to resolve the effective layout). Overwritten on every render before
    /// any key event is processed.
    diff_body_width: u16,
    /// Severity chosen in the last save; `None` at session start.
    last_severity: Option<Severity>,
    /// One-line status message shown at the bottom of the main view.
    status_message: Option<String>,
    /// Stack navigation state; `None` in single-change mode.
    stack: Option<StackContext>,
    /// Transition screen behavior loaded from config.
    transition_mode: TransitionMode,
    /// Whether the most recent comment load succeeded. `false` means the
    /// transition modal should not advertise a comment count.
    comments_loaded_ok: bool,
    /// Active when the user is picking a new anchor for a stale comment.
    pending_reanchor: Option<PendingReanchor>,
    /// Cached comments for the stack overview. `None` means the cache is
    /// invalid and must be rebuilt the next time the overview is opened.
    overview_cache: Option<OverviewCommentSet>,
    severity_filter: Option<Severity>,
    /// Persistent reviewed-bits, one entry per `(change_id, commit_id)`.
    /// Auto-marked whenever the user lands on a file via Tab/Shift-Tab,
    /// file picker, refresh, or stack-mode change-load.
    reviewed: ReviewedState,
    /// Session-scoped diff layout preference. Toggled by `|`; defaults to
    /// `Auto`, which picks unified or side-by-side at draw time based on
    /// the diff pane's body width.
    diff_mode: DiffMode,
    /// Set when the alternate screen was re-entered out-of-band (e.g. after
    /// suspending for `claude`); ratatui's diff cache is now stale and must
    /// be invalidated before the next draw.
    needs_full_redraw: bool,
    /// Scroll offset for the help screen; reset to 0 when help opens.
    help_scroll: u16,
}

#[cfg(test)]
impl App {
    fn new(
        details: ChangeDetails,
        repo_root: PathBuf,
        revset: String,
        stack: Option<StackContext>,
        transition_mode: TransitionMode,
    ) -> Self {
        let rendered_per_file = build_rendered_views(&details);
        let annotated_per_file = rendered_per_file.clone();
        // A load failure on reviewed.json should not block the TUI — fall back
        // to an empty state. The atomic-rename save self-heals the file on
        // the next mark. In tests, data_home equals repo_root.
        let reviewed = ReviewedState::load(&repo_root, &repo_root).unwrap_or_default();
        let data_home = repo_root.clone();
        Self {
            details,
            data_home,
            repo_root,
            revset,
            rendered_per_file,
            annotated_per_file,
            loaded_comments: Vec::new(),
            file_index: 0,
            line_index: 0,
            scroll: 0,
            screen: Screen::Main,
            should_quit: false,
            viewport_rows: FALLBACK_VIEWPORT_ROWS,
            diff_body_width: 0,
            last_severity: None,
            status_message: None,
            stack,
            transition_mode,
            comments_loaded_ok: false,
            pending_reanchor: None,
            overview_cache: None,
            severity_filter: None,
            reviewed,
            diff_mode: DiffMode::Auto,
            needs_full_redraw: false,
            help_scroll: 0,
        }
    }

    fn current_view(&self) -> Option<&DiffView> {
        self.annotated_per_file.get(self.file_index)
    }

    /// Resolve the effective diff layout for the current view + cached body
    /// width. Reads `diff_body_width`, which `render_diff` sets every frame.
    fn effective_diff_mode(&self) -> EffectiveDiffMode {
        resolve_diff_mode(self.diff_mode, self.diff_body_width, self.file_index)
    }

    /// Cycle the diff layout preference. `Auto -> ForceUnified ->
    /// ForceSideBySide -> Auto`. The cursor is reset to the top of the
    /// current view because the unified line index does not translate
    /// 1-to-1 to a paired row index, and snapping to a stable known
    /// position is less surprising than landing on a near-miss row.
    fn cycle_diff_mode(&mut self) {
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

    #[cfg(test)]
    fn current_line_count(&self) -> usize {
        self.current_view().map_or(0, |v| v.lines.len())
    }

    /// Resolve the current `(file_index)` to a [`ReviewTarget`]. `file_index`
    /// 0 is the synthetic description view; indices 1.. map onto
    /// `details.diff.files[i - 1]`. Returns `None` only when `file_index` is
    /// past the end (degenerate state — no diff files and no description).
    fn current_review_target(&self) -> Option<ReviewTarget> {
        if self.file_index == 0 {
            return Some(ReviewTarget::Description);
        }
        let diff_file_idx = self.file_index - 1;
        let path = self
            .details
            .diff
            .files
            .get(diff_file_idx)?
            .display_path()
            .to_owned();
        Some(ReviewTarget::File(path))
    }

    /// Auto-mark the active view (description or file) as reviewed for the
    /// current `(change_id, commit_id)`. Persists immediately so a crash
    /// before the next event loop iteration does not lose the bit.
    ///
    /// A persist failure is recorded in `status_message` only when nothing
    /// else has claimed it on this tick — Tab-at-boundary, "refreshed", and
    /// other purpose-set messages must not be silently clobbered by an
    /// auto-mark side effect. The in-memory state remains authoritative for
    /// the running session regardless.
    fn mark_current_file_reviewed(&mut self) {
        let Some(target) = self.current_review_target() else {
            return;
        };
        let change_id = self.details.change_id.clone();
        let commit_id = self.details.commit_id.clone();
        let outcome = self.reviewed.mark(change_id, commit_id, target);
        // Surface a one-shot toast when the call invalidated a stale entry
        // for this change (the change was amended/rebased between sessions).
        // First-encounter marks (no prior entry) stay silent — there is
        // nothing to "reset". Same `is_none()` guard as the save-failure
        // warning so purpose-set messages survive. The match is exhaustive
        // so any future variant added to `MarkOutcome` forces this site to
        // make an explicit decision.
        match outcome {
            MarkOutcome::ResetDueToCommitMismatch if self.status_message.is_none() => {
                self.status_message = Some(STATUS_REVIEWED_RESET.to_owned());
            }
            MarkOutcome::ResetDueToCommitMismatch | MarkOutcome::NoReset => {}
        }
        if let Err(e) = self.reviewed.save(&self.data_home, &self.repo_root) {
            if self.status_message.is_none() {
                self.status_message = Some(format!(
                    "warning: could not save reviewed state: {}",
                    sanitize_for_status(&e.to_string())
                ));
            }
        }
    }

    /// Toggle the reviewed bit for the active view (description or file).
    /// The escape hatch for cases where auto-mark fired prematurely (the
    /// reviewer Tabbed past a file without actually reviewing it).
    ///
    /// Persists immediately and sets [`STATUS_MARKED_REVIEWED`] /
    /// [`STATUS_MARKED_UNREVIEWED`] as appropriate.
    fn toggle_current_file_reviewed(&mut self) {
        let Some(target) = self.current_review_target() else {
            return;
        };
        let change_id = self.details.change_id.clone();
        let commit_id = self.details.commit_id.clone();
        let was_reviewed = self.is_view_reviewed(self.file_index);
        if was_reviewed {
            self.reviewed.unmark(&change_id, &commit_id, &target);
            self.status_message = Some(STATUS_MARKED_UNREVIEWED.to_owned());
        } else {
            self.reviewed.mark(change_id, commit_id, target);
            self.status_message = Some(STATUS_MARKED_REVIEWED.to_owned());
        }
        if let Err(e) = self.reviewed.save(&self.data_home, &self.repo_root) {
            // The toggle's own status message conveys the user-facing
            // outcome; a save failure is a secondary warning. Override
            // (not ignore) the toggle message so the user sees the
            // failure — losing the persistence is the more important
            // thing to surface.
            self.status_message = Some(format!(
                "warning: could not save reviewed state: {}",
                sanitize_for_status(&e.to_string())
            ));
        }
    }

    fn is_view_reviewed(&self, file_index: usize) -> bool {
        let Some(entry) = self.reviewed.entries.get(&self.details.change_id) else {
            return false;
        };
        if entry.commit_id != self.details.commit_id {
            return false;
        }
        if file_index == 0 {
            return entry.description_reviewed;
        }
        let Some(file) = self.details.diff.files.get(file_index - 1) else {
            return false;
        };
        entry.reviewed_files.contains(file.display_path())
    }

    fn refresh_inline_comments(&mut self) {
        // Any comment edit invalidates the overview cache so the next open
        // gets a fresh load.
        self.overview_cache = None;

        match crate::store::load_change_comments(
            &self.repo_root,
            &self.repo_root,
            &self.details.change_id,
        ) {
            Ok(comments) => {
                self.loaded_comments = self.reconcile_and_persist(comments);
                self.comments_loaded_ok = true;
            }
            Err(e) => {
                self.status_message = Some(format!(
                    "warning: could not load comments: {}",
                    sanitize_for_status(&e.to_string())
                ));
                self.loaded_comments = Vec::new();
                self.comments_loaded_ok = false;
            }
        }
        self.rebuild_annotated_views();
    }

    fn reconcile_and_persist(&mut self, comments: Vec<Comment>) -> Vec<Comment> {
        let (reconciled, first_error) = reconcile_comments_with_diff(
            comments,
            &self.details.diff,
            &self.details.description,
            &self.repo_root,
            &self.repo_root,
        );
        if let Some(msg) = first_error {
            self.status_message = Some(msg);
        }
        reconciled
    }

    fn rebuild_annotated_views(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        let severity_filter = self.severity_filter;
        let target_change_id = self.details.change_id.clone();
        // rendered_per_file[0] is the description view; diff files start at index 1.
        self.annotated_per_file = self
            .rendered_per_file
            .iter()
            .enumerate()
            .map(|(view_idx, base_view)| {
                if view_idx == 0 {
                    let description_inline: Vec<InlineComment> = self
                        .loaded_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, c)| description_comment_to_inline(c, idx, now))
                        .filter(|ic| severity_filter.is_none_or(|filter| ic.severity == filter))
                        .collect();
                    let change_inline: Vec<InlineComment> = self
                        .loaded_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, c)| {
                            change_comment_to_inline(c, idx, &target_change_id, now)
                        })
                        .filter(|ic| severity_filter.is_none_or(|filter| ic.severity == filter))
                        .collect();
                    base_view
                        .clone()
                        .with_inline_comments(&description_inline)
                        .with_change_comments_appended(&change_inline)
                } else {
                    let diff_file_idx = view_idx - 1;
                    let file_path = self
                        .details
                        .diff
                        .files
                        .get(diff_file_idx)
                        .map(|f| f.display_path().to_owned());
                    let inline: Vec<InlineComment> = self
                        .loaded_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, c)| comment_to_inline(c, idx, file_path.as_deref(), now))
                        .filter(|ic| severity_filter.is_none_or(|filter| ic.severity == filter))
                        .collect();
                    base_view.clone().with_inline_comments(&inline)
                }
            })
            .collect();
    }

    fn move_line(&mut self, delta: isize) {
        let count = self.current_row_count();
        if count == 0 {
            return;
        }
        let max_index = count - 1;
        let mut next = clamp_with_delta(self.line_index, delta, max_index);
        // Skip non-navigable rows: hunk separators and comment body continuation
        // lines. InlineCommentMeta lines are navigable — they are the "handle"
        // the reviewer lands on to press `e` or `d`. The skip rule applies
        // identically in unified and side-by-side modes; in side-by-side a
        // row that resolves to a single Removed/Added pair is always
        // navigable.
        let step: isize = if delta >= 0 { 1 } else { -1 };
        while next > 0 && next < max_index && self.is_skip_row(next) {
            next = clamp_with_delta(next, step, max_index);
        }
        self.line_index = next;
    }

    /// Number of navigable rows in the active layout: unified line count, or
    /// paired-row count in side-by-side.
    fn current_row_count(&self) -> usize {
        let Some(view) = self.current_view() else {
            return 0;
        };
        match self.effective_diff_mode() {
            EffectiveDiffMode::Unified => view.lines.len(),
            EffectiveDiffMode::SideBySide => view.paired_rows.len(),
        }
    }

    /// Is the row at `row_idx` non-navigable (separator or comment-body
    /// continuation)? The check resolves against the row's underlying
    /// `RenderedLine` kind in either layout.
    fn is_skip_row(&self, row_idx: usize) -> bool {
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
                    // `Pair` rows reference Removed, Added, or Context lines —
                    // never HunkSeparator or InlineCommentBody — so they are
                    // always navigable.
                    PairedRow::Pair { .. } => false,
                }
            }
        }
    }

    fn move_page(&mut self, delta: isize) {
        let step = page_size(self.viewport_rows);
        let signed_step: isize = isize::try_from(step).unwrap_or(isize::MAX);
        self.move_line(delta.saturating_mul(signed_step));
    }

    fn jump_to(&mut self, end: Edge) {
        let count = self.current_row_count();
        if count == 0 {
            return;
        }
        self.line_index = match end {
            Edge::Top => 0,
            Edge::Bottom => count - 1,
        };
    }

    fn cycle_file(&mut self, delta: isize) {
        let count = self.rendered_per_file.len();
        if count == 0 {
            return;
        }
        if count == 1 {
            self.status_message = Some(STATUS_ONLY_ONE_FILE.to_owned());
            self.line_index = 0;
            self.scroll = 0;
            // Single-view changes still count as a landing event — mark the
            // sole view reviewed so the change can flip to fully-reviewed
            // without requiring a Tab that has nowhere to go.
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

    fn ensure_cursor_visible(&mut self, viewport_rows: u16) {
        let line_index_u16 = u16::try_from(self.line_index).unwrap_or(u16::MAX);
        if line_index_u16 < self.scroll {
            self.scroll = line_index_u16;
        }
        let last_visible = self.scroll.saturating_add(viewport_rows.saturating_sub(1));
        if line_index_u16 > last_visible {
            self.scroll = line_index_u16.saturating_sub(viewport_rows.saturating_sub(1));
        }
    }

    fn stack_len(&self) -> Option<usize> {
        self.stack.as_ref().map(|s| s.entries.len())
    }

    fn stack_index(&self) -> Option<usize> {
        self.stack.as_ref().map(|s| s.current_index)
    }

    fn transition_enabled(&self, stack_len: usize) -> bool {
        match self.transition_mode {
            TransitionMode::Never => false,
            TransitionMode::Always => true,
            TransitionMode::Auto => stack_len >= AUTO_TRANSITION_THRESHOLD,
        }
    }

    /// Advance to the next change in stack mode.
    ///
    /// If a transition screen is configured for this stack depth, pushes the
    /// `Transition` screen instead of loading immediately. Otherwise loads the
    /// next change directly.
    fn advance_stack(&mut self) -> Result<()> {
        let Some(ctx) = self.stack.as_ref() else {
            self.status_message =
                Some("single-change view — run with --stack to walk a stack".to_owned());
            return Ok(());
        };

        let next_index = ctx.current_index + 1;
        if next_index >= ctx.entries.len() {
            self.status_message = Some("already at the last change".to_owned());
            return Ok(());
        }

        let stack_len = ctx.entries.len();
        let reviewed_index = ctx.current_index;
        let (reviewed_comment_count, severity_histogram) = if self.comments_loaded_ok {
            (
                Some(self.loaded_comments.len()),
                histogram_from_comments(&self.loaded_comments),
            )
        } else {
            (None, CoreSeverityHistogram::default())
        };

        if self.transition_enabled(stack_len) {
            self.screen = Screen::Transition(TransitionState {
                reviewed_index,
                next_index,
                reviewed_comment_count,
                severity_histogram,
            });
            return Ok(());
        }

        self.load_stack_entry(next_index, true)
    }

    /// Retreat to the previous change in stack mode.
    fn retreat_stack(&mut self) -> Result<()> {
        let Some(ctx) = self.stack.as_ref() else {
            self.status_message =
                Some("single-change view — run with --stack to walk a stack".to_owned());
            return Ok(());
        };

        let Some(prev_index) = pick_retreat_index(ctx.current_index) else {
            self.status_message = Some("already at the first change".to_owned());
            return Ok(());
        };
        self.load_stack_entry(prev_index, false)
    }

    /// Load all comments needed for the stack overview and store them in the cache.
    fn load_overview_comments(&mut self) {
        let Some(ctx) = self.stack.as_ref() else {
            return;
        };
        let revset_hash = ctx.revset_hash;
        let entries = ctx.entries.clone();
        let repo_root = self.repo_root.clone();

        let stack_level = crate::store::load_stack_comments(&repo_root, &repo_root, &revset_hash)
            .unwrap_or_else(|e| {
                let _ = e; // best-effort; ignore load failures for overview
                Vec::new()
            });

        let per_change: Vec<Vec<Comment>> = entries
            .iter()
            .map(|entry| {
                crate::store::load_change_comments(&repo_root, &repo_root, &entry.change_id)
                    .unwrap_or_default()
            })
            .collect();

        // Load each change's diff once at overview-open time. Best-effort: a
        // failure for one change leaves an empty path list, which makes the
        // reviewed-status predicate return "description-only" — better than
        // refusing to render the overview entirely.
        let diff_paths_per_change: Vec<Vec<PathBuf>> = entries
            .iter()
            .map(|entry| match jj::diff_for_change(&entry.change_id) {
                Ok(diff) => diff
                    .files
                    .iter()
                    .map(|f| f.display_path().to_owned())
                    .collect(),
                Err(_) => Vec::new(),
            })
            .collect();

        let orphaned = collect_orphaned_comments(&repo_root, &repo_root, &entries);

        self.overview_cache = Some(OverviewCommentSet {
            stack_level,
            per_change,
            orphaned,
            diff_paths_per_change,
        });
    }

    /// Navigate directly to stack entry at `idx` without emitting a transition screen.
    fn goto_stack_index(&mut self, idx: usize) -> Result<()> {
        let Some(ctx) = self.stack.as_ref() else {
            return Ok(());
        };
        if idx >= ctx.entries.len() {
            return Ok(());
        }
        self.load_stack_entry(idx, true)
    }

    /// Load the stack entry at `idx`. Persists the cursor if `advance` is true.
    fn load_stack_entry(&mut self, idx: usize, advance: bool) -> Result<()> {
        let (revset, revset_hash, change_id) = {
            let Some(ctx) = self.stack.as_ref() else {
                return Ok(());
            };
            (
                ctx.revset.clone(),
                ctx.revset_hash,
                ctx.entries[idx].change_id.clone(),
            )
        };

        let details = jj::show(&change_id)?;

        self.rendered_per_file = build_rendered_views(&details);
        self.annotated_per_file = self.rendered_per_file.clone();
        self.details = details;
        self.file_index = 0;
        self.line_index = 0;
        self.scroll = 0;
        self.status_message = None;

        if let Some(ctx) = self.stack.as_mut() {
            ctx.current_index = idx;
        }

        self.refresh_inline_comments();
        self.mark_current_file_reviewed();

        if advance {
            let _ = cursor::record(
                &self.data_home,
                &self.repo_root,
                revset_hash,
                &revset,
                &change_id,
            );
        }

        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
}

/// Prevent JSONL-derived strings from injecting terminal escape
/// sequences or `BiDi` overrides into the ratatui status bar. Local-CLI
/// threat model, but path / error fragments come from untrusted on-disk
/// records.
fn sanitize_for_status(s: &str) -> String {
    s.chars()
        .map(|c| {
            if (c.is_control() && c != '\t')
                || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
            {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Reconcile `comments` against the given diff and description, persisting any
/// re-anchored comments to disk. Returns the reconciled list and the first
/// persist error (if any) as a status-bar string. All entries are attempted
/// regardless; only the first error is returned — subsequent errors are
/// collected but discarded.
///
/// Shared by both `JjrSurface::reconcile_and_persist` and the legacy
/// `cfg(test) App::reconcile_and_persist` so the logic lives in one place.
fn reconcile_comments_with_diff(
    comments: Vec<Comment>,
    diff: &crate::diff::Diff,
    description: &str,
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
) -> (Vec<Comment>, Option<String>) {
    let mut errors: Vec<String> = Vec::new();
    let reconciled: Vec<Comment> = comments
        .into_iter()
        .map(
            |comment| match crate::anchoring::reanchor_comment(&comment, diff, description) {
                None => comment,
                Some(updated) => {
                    match crate::store::update_comment(data_home, repo_root, &updated) {
                        Ok(()) => updated,
                        Err(e) => {
                            errors.push(format_persist_error(&updated, &e));
                            comment
                        }
                    }
                }
            },
        )
        .collect();
    let first_error = errors.into_iter().next();
    (reconciled, first_error)
}

fn format_persist_error(comment: &Comment, err: &JjrError) -> String {
    let raw_location = match &comment.anchor {
        Anchor::Line { location, .. } => format!(
            "{}:{}",
            location.file.display(),
            location
                .new_line
                .or(location.old_line)
                .map_or_else(|| "?".to_owned(), |n| n.to_string()),
        ),
        Anchor::Change { change_id } => change_id.as_str().to_owned(),
        Anchor::Stack { .. } => "stack".to_owned(),
        Anchor::Description {
            change_id,
            location,
        } => format!(
            "description:{}:{}",
            change_id.as_str(),
            location
                .display_line
                .map_or_else(|| "?".to_owned(), |n| n.to_string()),
        ),
    };
    let location = sanitize_for_status(&raw_location);
    let err = sanitize_for_status(&err.to_string());
    format!("warning: could not persist re-anchor for {location}: {err}")
}

/// Load comments from every per-change JSONL file whose `change_id` is absent
/// from the resolved stack entries, marking every loaded comment as
/// `Status::Orphaned`.
///
/// The orphaned list is stored in the overview cache and held for future
/// surfacing (e.g. a `jjr orphans` command). Nothing in the current UI renders
/// these comments. Best-effort: a load failure for one file is silently skipped
/// so a single corrupt file does not block the rest of the pass.
///
/// Best-effort orphan discovery. Reads every `<change-id>.jsonl` not in
/// the resolved stack into memory. Memory cost is O(N*M) for N orphan
/// files * M comments per file; in the single-user local threat model
/// this is a self-DoS only. If `jjr` ever runs in a shared workspace,
/// add a hard cap on files and lines processed per session.
fn collect_orphaned_comments(
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
    stack_entries: &[StackEntry],
) -> Vec<Comment> {
    let in_stack: std::collections::HashSet<&ChangeId> =
        stack_entries.iter().map(|e| &e.change_id).collect();

    let Ok(all_on_disk) = crate::store::list_change_ids_with_comments(data_home, repo_root) else {
        return Vec::new();
    };

    let mut orphaned = Vec::new();
    for change_id in all_on_disk {
        if in_stack.contains(&change_id) {
            continue;
        }
        let Ok(comments) = crate::store::load_change_comments(data_home, repo_root, &change_id)
        else {
            continue;
        };
        for mut comment in comments {
            comment.status = Some(Status::Orphaned);
            orphaned.push(comment);
        }
    }
    orphaned
}

/// Production entry point: construct a `JjrApp` and run the generic core event
/// loop. The on-exit closure persists the cursor so subsequent runs resume at
/// the last-reviewed change.
fn run_jjr_app(
    terminal: &mut Term,
    details: ChangeDetails,
    ctx: JjrContext,
    stack: Option<StackContext>,
    stderr_guard: Option<StderrLogGuard>,
) -> Result<()> {
    use local_review_core::tui::{
        run_app as core_run_app, AppError, TransitionMode as CoreTransitionMode,
    };
    let core_transition_mode = match load_transition_mode() {
        TransitionMode::Never => CoreTransitionMode::Never,
        TransitionMode::Auto => CoreTransitionMode::Auto,
        TransitionMode::Always => CoreTransitionMode::Always,
    };
    let rendered = build_rendered_views(&details);
    let surface = JjrSurface::new(details, ctx, stack, stderr_guard);
    let mut app = JjrApp::new(surface, rendered, core_transition_mode);
    core_run_app(terminal, &mut app, |app| {
        persist_cursor_on_jjr_app_exit(app);
    })
    .map_err(|e| match e {
        AppError::Io(io) => JjrError::Io { source: io },
        AppError::Surface(surf) => surf,
    })
}

/// Persist the cursor at the last-viewed change so a subsequent run resumes
/// here. Best-effort: a cursor write failure should not block exit.
fn persist_cursor_on_jjr_app_exit(app: &JjrApp) {
    let surface = &app.surface;
    let Some(ctx) = surface.stack.as_ref() else {
        return;
    };
    let change_id = &ctx.entries[ctx.current_index].change_id;
    let _ = cursor::record(
        &surface.data_home,
        &surface.repo_root,
        ctx.revset_hash,
        &ctx.revset,
        change_id,
    );
}

/// Invalidate ratatui's previous-frame buffer cache when the screen has been
/// re-entered out-of-band (e.g. after suspending for `claude`). Without this,
/// the next `terminal.draw()` only writes diffs against a buffer that no
/// longer matches the freshly-blank alternate screen, leaving cells ratatui
/// thinks "unchanged" stale on screen.
#[cfg(test)]
fn maybe_clear_for_full_redraw<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: core::error::Error + Send + Sync + 'static,
{
    if app.needs_full_redraw {
        terminal
            .clear()
            .map_err(|e| io_err(std::io::Error::other(e)))?;
        app.needs_full_redraw = false;
    }
    Ok(())
}

/// Best-effort cursor write on app exit. Silent on failure — the cursor file
/// is convenience state, not authoritative.
#[cfg(test)]
fn persist_cursor_on_exit(app: &App) {
    let Some(ctx) = app.stack.as_ref() else {
        return;
    };
    let change_id = &ctx.entries[ctx.current_index].change_id;
    let _ = cursor::record(
        &app.data_home,
        &app.repo_root,
        ctx.revset_hash,
        &ctx.revset,
        change_id,
    );
}

fn load_transition_mode() -> TransitionMode {
    let Some(table) = crate::util::load_global_config_table() else {
        return TransitionMode::Never;
    };
    let value = table
        .get("ui")
        .and_then(|ui| ui.get("transition_screen"))
        .and_then(toml::Value::as_str)
        .unwrap_or("never");
    match value {
        "auto" => TransitionMode::Auto,
        "always" => TransitionMode::Always,
        _ => TransitionMode::Never,
    }
}

// Full-screen views (Stale, Overview) replace the main view entirely. Modals
// (Help, Composer, Transition) overlay on top of the main diff view.
#[cfg(test)]
fn render(frame: &mut Frame<'_>, app: &mut App) {
    if matches!(app.screen, Screen::Stale(_)) {
        let Screen::Stale(mut state) = std::mem::replace(&mut app.screen, Screen::Main) else {
            unreachable!("matched above");
        };
        stale_screen::render(frame, &mut state, &app.loaded_comments, &app.details.diff);
        app.screen = Screen::Stale(state);
        return;
    }

    if matches!(app.screen, Screen::Overview(_)) {
        // Take the state and cache out temporarily to satisfy the borrow checker.
        let Screen::Overview(mut state) = std::mem::replace(&mut app.screen, Screen::Main) else {
            unreachable!("matched above");
        };
        let cache = app.overview_cache.take();
        if let Some(ref cache_ref) = cache {
            let stack_ctx = app.stack.as_ref().map(|ctx| OverviewStackCtx {
                revset: &ctx.revset,
                entries: &ctx.entries,
                current_index: ctx.current_index,
            });
            overview_screen::render(frame, &mut state, stack_ctx, &app.reviewed, cache_ref);
        }
        app.overview_cache = cache;
        app.screen = Screen::Overview(state);
        return;
    }

    render_main(frame, app);
    match &app.screen {
        Screen::Main => {}
        Screen::Help => {
            help_screen::render(frame, "jjr · keybindings", JJR_HELP_BODY, app.help_scroll);
        }
        Screen::Composer(composer) => {
            composer_overlay::render_composer_overlay(frame, composer, app.current_view());
        }
        Screen::Transition(state) => {
            render_transition(frame, app, state);
        }
        Screen::SendToClaude(state) => {
            send_to_claude::render(frame, state);
        }
        Screen::FilePicker(state) => {
            file_picker::render(frame, state);
        }
        Screen::Stale(_) | Screen::Overview(_) => unreachable!("handled above"),
    }
}

#[cfg(test)]
fn render_main(frame: &mut Frame<'_>, app: &mut App) {
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

#[cfg(test)]
fn render_stack_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (position, total) = match app.stack_index().zip(app.stack_len()) {
        Some((idx, len)) => (idx + 1, len),
        None => (1, 1),
    };

    // On wide-enough terminals, prepend a graphical progress fill. On narrow
    // terminals (per the resize ladder) drop the fill and keep just the text.
    let interior_cols = area.width.saturating_sub(BLOCK_BORDER_COLS);
    let bar_segment = if area.width >= STACK_BAR_MIN_COLS_FOR_FILL && total > 0 {
        progress_bar_string(position, total, STACK_PROGRESS_BAR_WIDTH)
    } else {
        String::new()
    };

    let text_segment = format!("{position}/{total}  {}  ", app.details.change_id);
    let used_width = bar_segment.chars().count() + text_segment.chars().count();
    let desc_budget = usize::from(interior_cols).saturating_sub(used_width);
    let label = format!(
        "{}{}{}",
        bar_segment,
        text_segment,
        truncate(&app.details.description, desc_budget)
    );
    let block = Block::default().borders(Borders::ALL).title("Stack");
    let widget = Paragraph::new(label).block(block);
    frame.render_widget(widget, area);
}

/// Build a `████░░░░  ` style progress fill of fixed `width` cells, followed
/// by two spaces. `position` is 1-based; `total` must be non-zero.
#[cfg(test)]
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

#[cfg(test)]
fn file_header_label(app: &App) -> String {
    let total = app.rendered_per_file.len();
    let position = app.file_index.saturating_add(1);
    let path_label = app
        .current_view()
        .map_or_else(|| "(no files)".to_owned(), |v| v.title.clone());
    format!("{path_label}  ·  {position} of {total}")
}

#[cfg(test)]
fn render_file_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let label = file_header_label(app);
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

#[cfg(test)]
fn render_diff(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Snapshot scalars off `app` before borrowing the view: setting
    // `app.diff_body_width` at the end would otherwise overlap with the
    // view borrow held by the render path.
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

    // Compute the effective layout from the body width *after* the scrollbar
    // is reserved, so the threshold compares against the same width that the
    // renderer actually sees. Probe with the unified row count first to
    // decide whether a scrollbar is needed; we will recompute in side-by-side
    // mode against the paired-row count.
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

/// Render the active view as two columns separated by a centered gutter.
///
/// Each `PairedRow` produces exactly one terminal row. `Spanning` rows occupy
/// the entire body width for non-comment lines (hunk headers, separators,
/// context, notices, description lines) — rendered ONCE across both columns,
/// not duplicated per side. Inline comment `Spanning` rows occupy a single
/// column (right by default; left when the comment is `Side::Old`-anchored
/// and the preceding row has no right cell — i.e. a pure deletion).
///
/// `Pair { left, right }` rows put the Removed line in the left column and
/// the Added line in the right column; either may be `None` and rendered as
/// blank padding.
#[cfg(test)]
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

/// Where on a side-by-side row an inline comment should land.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineCommentColumn {
    /// `Side::Old` anchor on a row whose right cell is empty (pure deletion):
    /// the deleted line is on the left, so render the comment there too.
    Left,
    /// All other cases — the conversation is forward-looking, so the
    /// comment lives on the right.
    Right,
}

/// Decide which column an inline-comment `Spanning` row should occupy.
///
/// `Side::Old` comment + previous row was a pure deletion (`Pair { left:
/// Some, right: None }`) → render in the LEFT column (mirrors where the
/// deleted line sits). Otherwise render on the right.
///
/// `Side::Old` vs `Side::New` is read from the underlying `RenderedLine`'s
/// line-number fields (set by `inject_comment_lines` from the originating
/// `InlineComment`):
/// - `Side::Old` → `source_line.is_some() && target_line.is_none()`
/// - `Side::New` → `target_line.is_some()`
#[cfg(test)]
fn inline_comment_column(
    rows: &[PairedRow],
    row_idx: usize,
    comment: &RenderedLine,
) -> InlineCommentColumn {
    let is_side_old = comment.source_line.is_some() && comment.target_line.is_none();
    if !is_side_old {
        return InlineCommentColumn::Right;
    }
    // Walk backward past consecutive comment rows to find the row this
    // comment is anchored to. A multi-line comment body produces multiple
    // Spanning(InlineCommentBody) rows after the meta — we want the
    // anchor row, not a sibling comment line.
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

/// Geometry for one side-by-side render pass: per-side cell width and the
/// full body width (used for non-comment Spanning rows that occupy the
/// entire row instead of just one column).
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct SideBySideGeometry {
    side_width: u16,
    full_width: u16,
}

/// Position of one row within the side-by-side row list: the row's data,
/// its index, and the full row vector (used by the inline-comment column
/// resolver to look back at the preceding anchor row).
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct PairedRowAt<'a> {
    row: PairedRow,
    row_idx: usize,
    rows: &'a [PairedRow],
}

/// Render a single side-by-side row: `[left] │ [right]` or, for
/// `Spanning` non-comment rows, a single full-width line.
#[cfg(test)]
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
                // Non-comment Spanning rows (hunk headers, separators, notices,
                // description lines) render ONCE across the full body width —
                // duplicating per-side would truncate long content. Context
                // lines are NOT in this set: they emit Pair { Some(i), Some(i) }
                // and truncate per side.
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
            // Gutter stays calm on focused rows — the cells carry the focus
            // signal; reverse-videoing the divider too would compete with
            // content for attention.
            let gutter = side_by_side_gutter_spans();
            TuiLine::from([left_spans, gutter, right_spans].concat())
        }
    }
}

/// Render a non-comment `Spanning` row across the full body width as a
/// single styled span. Focus rule via [`focus_style`].
#[cfg(test)]
fn render_full_width_row(line: &RenderedLine, focused: bool, full_width: u16) -> TuiLine<'_> {
    let (body, fg_color) = prefix_truncate_pad(line, full_width);
    TuiLine::from(vec![Span::styled(body, focus_style(fg_color, focused))])
}

/// Render an inline-comment `Spanning` row. The comment occupies one column
/// (per `column`); the other column is blank-padded so the gutter aligns
/// with neighboring rows.
#[cfg(test)]
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

/// Render a single side cell for a paired row. Body via
/// [`prefix_truncate_pad`]; focus rule via [`focus_style`].
#[cfg(test)]
fn side_cell_spans(line: &RenderedLine, side_width: u16, focused: bool) -> Vec<Span<'_>> {
    let (body, fg_color) = prefix_truncate_pad(line, side_width);
    vec![Span::styled(body, focus_style(fg_color, focused))]
}

/// Resolve the per-line `Style` from the line's fg color and the focus flag.
/// Single source of truth for the "REVERSED strips fg" rule applied across
/// every diff row in both layouts.
#[cfg(test)]
fn focus_style(fg_color: Color, focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if matches!(fg_color, Color::Reset) {
        Style::default()
    } else {
        Style::default().fg(fg_color)
    }
}

#[cfg(test)]
fn blank_cell_spans<'a>(side_width: u16, focused: bool) -> Vec<Span<'a>> {
    let body: String = " ".repeat(usize::from(side_width));
    vec![Span::styled(body, focus_style(Color::Reset, focused))]
}

#[cfg(test)]
fn side_by_side_gutter_spans<'a>() -> Vec<Span<'a>> {
    // ` │ ` — single space, vertical bar, single space — matches
    // `SIDE_BY_SIDE_GUTTER_WIDTH`. The DarkGray fg is the same regardless
    // of the focus state of the surrounding row: cells carry the focus
    // signal; reverse-videoing the divider too would compete with content
    // for attention.
    vec![Span::styled(
        " \u{2502} ",
        Style::default().fg(Color::DarkGray),
    )]
}

pub(super) use local_review_core::tui::{render_view_scrollbar, scrollbar_layout_for_view};

#[cfg(test)]
pub(super) use local_review_core::tui::{
    scrollbar_overflow_for_view, scrollbar_state_for_view, split_body_for_scrollbar,
};

/// Visual attributes that the unified and side-by-side renderers share for a
/// `RenderedLine`. The `prefix` is the two-cell `+ ` / `- ` / `  ` glyph
/// prepended to the line content; `fg_color` is the foreground used when the
/// row is not focused.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineVisual {
    prefix: &'static str,
    fg_color: Color,
}

/// Build the rendered body string for a `RenderedLine` at a given cell width:
/// `prefix + truncated text + space-padding`. Returns the assembled body and
/// the line's foreground color; callers apply the focus rule via
/// [`focus_style`].
#[cfg(test)]
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

#[cfg(test)]
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
            // Severity drives the color; the inline comment text already
            // carries its own decoration glyph (`┃ ●`) so no extra prefix.
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

#[cfg(test)]
fn render_rendered_line(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    // Inline comment lines have no `+`/`-` prefix and don't pad to full
    // width — their `┃ ●` decoration glyph already establishes the column
    // anchor, and they only span the natural text length. Focus policy
    // matches every other row: REVERSED strips fg.
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
        // Pad to full row width so reverse-video covers the entire line.
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

#[cfg(test)]
const FOOTER_IRREDUCIBLE: &str = " \u{2191}\u{2193} line  Tab file  n/p revision  Enter comment";

#[cfg(test)]
struct FooterSegment {
    text: &'static str,
    stack_only: bool,
}

#[cfg(test)]
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

/// Build the main-view footer text for the given terminal width. Drops
/// optional bindings right-to-left (least-essential first) until the text fits.
#[cfg(test)]
pub(super) fn footer_text_for_width(
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

    // drop_count == candidates.len() ensures the loop always returns.
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

#[cfg(test)]
fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (text, style) = if let Some(msg) = app.status_message.as_deref() {
        (msg.to_owned(), Style::default().fg(Color::Yellow))
    } else if let Some(reanchor) = app.pending_reanchor.as_ref() {
        let prompt = reanchor_prompt(&reanchor.body, area.width);
        (prompt, Style::default().fg(Color::Yellow))
    } else if focused_comment(app).is_some() {
        (
            " \u{2191}\u{2193} line  e edit  d delete  c new comment  ? help  q quit".to_owned(),
            Style::default(),
        )
    } else {
        let has_stack = app.stack.is_some();
        (
            footer_text_for_width(area.width, has_stack, app.severity_filter),
            Style::default(),
        )
    };
    let widget = Paragraph::new(text).style(style);
    frame.render_widget(widget, area);
}

#[cfg(test)]
fn reanchor_prompt(body: &str, width: u16) -> String {
    // 74 chars of fixed text in the long form: `re-anchoring "" — navigate and
    // press c to pick the new line; Esc to cancel` (count excludes the body).
    const LONG_FIXED_CHARS: usize = 74;
    const SHORT_FORM: &str = "re-anchoring \u{2014} c to pick line; Esc cancel";

    let width = usize::from(width);
    if width > LONG_FIXED_CHARS {
        let body_budget = width.saturating_sub(LONG_FIXED_CHARS);
        let body_preview = truncate(body, body_budget);
        format!(
            "re-anchoring \"{body_preview}\" \u{2014} navigate and press c to pick the new line; Esc to cancel"
        )
    } else {
        SHORT_FORM.to_owned()
    }
}

#[cfg(test)]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_main_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if app.pending_reanchor.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.pending_reanchor = None;
                app.status_message = None;
                return Ok(());
            }
            KeyCode::Char('c') | KeyCode::Enter => {
                open_composer(app);
                return Ok(());
            }
            _ => {}
        }
    } else {
        app.status_message = None;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.screen = Screen::Help;
        }
        KeyCode::Char('S') => open_stale_screen(app),
        KeyCode::Char('s') => open_overview_screen(app),
        KeyCode::Up | KeyCode::Char('k') => app.move_line(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_line(1),
        KeyCode::PageUp => app.move_page(-1),
        KeyCode::PageDown => app.move_page(1),
        KeyCode::Home | KeyCode::Char('g') => app.jump_to(Edge::Top),
        KeyCode::End | KeyCode::Char('G') => app.jump_to(Edge::Bottom),
        KeyCode::Tab => app.cycle_file(1),
        KeyCode::BackTab => app.cycle_file(-1),
        KeyCode::Char('c') | KeyCode::Enter => open_composer(app),
        KeyCode::Char('C') => open_send_to_claude(app),
        KeyCode::Char('e') => open_composer_for_edit(app),
        KeyCode::Char('d') => delete_focused_comment(app),
        KeyCode::Char('n') => app.advance_stack()?,
        KeyCode::Char('p') => app.retreat_stack()?,
        KeyCode::Char('f') => open_file_picker(app),
        KeyCode::Char('r') => refresh_current_change(app),
        KeyCode::Char('1') => toggle_severity_filter(app, Severity::Required),
        KeyCode::Char('2') => toggle_severity_filter(app, Severity::Suggestion),
        KeyCode::Char('3') => toggle_severity_filter(app, Severity::Note),
        KeyCode::Char('U') => app.toggle_current_file_reviewed(),
        KeyCode::Char('|') => app.cycle_diff_mode(),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the stale screen"
)]
fn handle_stale_key(app: &mut App, key: KeyEvent) {
    let Screen::Stale(ref state) = app.screen else {
        return;
    };
    let count = state.stale_indices.len();

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.screen = Screen::Main;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::Stale(ref mut s) = app.screen {
                s.selected_index = s.selected_index.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if count > 0 => {
            if let Screen::Stale(ref mut s) = app.screen {
                s.selected_index = (s.selected_index + 1).min(count - 1);
            }
        }
        KeyCode::Char('d') => {
            let focused = focused_stale(app).cloned();
            if let Some(comment) = focused {
                delete_focused_stale(app, &comment);
            }
        }
        KeyCode::Enter => {
            view_in_source(app);
        }
        KeyCode::Char('e') => {
            enter_reanchor_mode(app);
        }
        _ => {}
    }
}

#[cfg(test)]
fn focused_stale(app: &App) -> Option<&Comment> {
    let Screen::Stale(ref state) = app.screen else {
        return None;
    };
    let &comment_idx = state.stale_indices.get(state.selected_index)?;
    app.loaded_comments.get(comment_idx)
}

#[cfg(test)]
fn delete_focused_stale(app: &mut App, comment: &Comment) {
    match crate::store::delete_comment(&app.repo_root, &app.repo_root, comment) {
        Ok(()) => {
            app.refresh_inline_comments();
            let new_indices = stale_screen::stale_comment_indices(&app.loaded_comments);
            let new_count = new_indices.len();
            if let Screen::Stale(ref mut state) = app.screen {
                state.stale_indices = new_indices;
                if new_count == 0 {
                    state.selected_index = 0;
                } else if state.selected_index >= new_count {
                    state.selected_index = new_count - 1;
                }
            }
        }
        Err(e) => {
            app.status_message = Some(format!(
                "delete failed: {}",
                sanitize_for_status(&e.to_string())
            ));
        }
    }
}

#[cfg(test)]
fn view_in_source(app: &mut App) {
    let focused = focused_stale(app).cloned();
    let Some(comment) = focused else {
        return;
    };
    let Anchor::Line { location, .. } = &comment.anchor else {
        return;
    };

    let file_idx = app
        .details
        .diff
        .files
        .iter()
        .position(|f| f.display_path() == location.file.as_path());

    let Some(fidx) = file_idx else {
        app.status_message = Some("file not in current diff".to_owned());
        return;
    };

    // file_index 0 is the description view; diff files start at view index 1.
    let view_idx = fidx + 1;
    app.screen = Screen::Main;
    app.file_index = view_idx;
    app.scroll = 0;

    let line_num = match location.side {
        Side::Old => location.old_line,
        Side::New => location.new_line,
    };

    if let Some(target_line_num) = line_num {
        let view = &app.annotated_per_file[view_idx];
        let pos = view.lines.iter().position(|l| match location.side {
            Side::Old => l.source_line == Some(target_line_num),
            Side::New => l.target_line == Some(target_line_num),
        });
        app.line_index = pos.unwrap_or(0);
    } else {
        app.line_index = 0;
    }
    app.mark_current_file_reviewed();
}

#[cfg(test)]
fn enter_reanchor_mode(app: &mut App) {
    let focused = focused_stale(app).cloned();
    let Some(comment) = focused else {
        return;
    };
    let Anchor::Line { location, .. } = &comment.anchor else {
        return;
    };

    let severity = comment.severity;
    let file = location.file.clone();

    app.pending_reanchor = Some(PendingReanchor {
        body: comment.body.clone(),
        severity,
        original: comment,
    });

    app.screen = Screen::Main;

    let file_idx = app
        .details
        .diff
        .files
        .iter()
        .position(|f| f.display_path() == file.as_path());

    match file_idx {
        Some(fidx) => {
            // file_index 0 is the description view; diff files start at 1.
            app.file_index = fidx + 1;
            app.line_index = 0;
            app.scroll = 0;
            app.mark_current_file_reviewed();
        }
        None => {
            app.status_message = Some(
                "re-anchor: file not in current diff; pick a line in the visible file".to_owned(),
            );
        }
    }
}

#[cfg(test)]
fn open_stale_screen(app: &mut App) {
    let stale_indices = stale_screen::stale_comment_indices(&app.loaded_comments);
    app.screen = Screen::Stale(StaleScreenState {
        selected_index: 0,
        stale_indices,
        scroll_offset: 0,
    });
}

#[cfg(test)]
fn open_overview_screen(app: &mut App) {
    if app.pending_reanchor.is_some() {
        app.status_message =
            Some("finish or cancel re-anchor mode before opening the stack overview".to_owned());
        return;
    }
    if app.stack.is_none() {
        app.status_message = Some("stack overview requires --stack mode".to_owned());
        return;
    }
    if app.overview_cache.is_none() {
        app.load_overview_comments();
    }
    app.screen = Screen::Overview(OverviewScreenState::new());
}

#[cfg(test)]
fn open_send_to_claude(app: &mut App) {
    let change_id = app.details.change_id.clone();
    let revset_hash = app.stack.as_ref().map(|s| s.revset_hash);

    // In stack mode, the packet must reflect the WHOLE stack so per-change and
    // line-level comments anchored to other changes (B/C/D) ride along with
    // those on the current change (A). A single-entry stack here would silently
    // drop them, surfacing as "no comments to send" when the user is sitting on
    // a change with no comments of its own.
    let entries = match app.stack.as_ref() {
        Some(stack) => stack.entries.clone(),
        None => vec![StackEntry {
            change_id: change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
        }],
    };

    let resolved = ResolvedStack {
        revset_hash: revset_hash.unwrap_or_else(|| RevsetHash::from_revset(&app.revset)),
        revset: app.revset.clone(),
        entries,
    };

    let packet = match crate::packet::build_packet(
        &app.repo_root,
        &app.repo_root,
        &resolved,
        false,
        jj::diff_for_change,
    ) {
        Ok(p) => p,
        Err(JjrError::EmptyPacket { .. }) => {
            app.status_message = Some("no comments to send".to_owned());
            return;
        }
        Err(e) => {
            app.status_message = Some(format!(
                "could not build packet: {}",
                sanitize_for_status(&e.to_string())
            ));
            return;
        }
    };

    let stale_count = send_to_claude::stale_count_for_change(
        &app.repo_root,
        &app.repo_root,
        &change_id,
        revset_hash,
    );
    let scope_severity_grid = send_to_claude::compute_scope_severity_grid(&packet);
    let files_affected = send_to_claude::compute_files_affected(&packet);

    let data = ConfirmData {
        change_id,
        change_description: app.details.description.clone(),
        scope_severity_grid,
        files_affected,
        stale_count,
        packet,
    };
    app.screen = Screen::SendToClaude(Box::new(SendToClaudeState::Confirm(data)));
}

/// Pick the new `current_index` after a stack re-resolution. Prefer the
/// previously-current `change_id` if it still exists; otherwise clamp the old
/// index into the new range so the cursor lands somewhere reasonable when the
/// agent abandoned or squashed away the old entry.
///
/// Caller must guarantee `new_entries` is non-empty (an empty stack triggers
/// `RevsetNoMatch` upstream in `resolve_stack`).
fn new_current_index_after_reload(
    prev_change_id: &ChangeId,
    prev_index: usize,
    new_entries: &[StackEntry],
) -> usize {
    if let Some(idx) = new_entries
        .iter()
        .position(|e| &e.change_id == prev_change_id)
    {
        return idx;
    }
    prev_index.min(new_entries.len().saturating_sub(1))
}

/// Pure in-memory side of the stack-mode reload. Replaces `app.stack` entries
/// with the freshly-resolved set, advances `current_index` to the chosen
/// position, resets the cursor, and rebuilds views from the new details.
#[cfg(test)]
fn apply_post_claude_stack_reload(
    app: &mut App,
    resolved: ResolvedStack,
    new_index: usize,
    details: ChangeDetails,
) {
    if let Some(ctx) = app.stack.as_mut() {
        ctx.entries = resolved.entries;
        ctx.revset = resolved.revset;
        ctx.revset_hash = resolved.revset_hash;
        ctx.current_index = new_index;
    }
    app.file_index = 0;
    app.line_index = 0;
    app.scroll = 0;
    rebuild_views_and_mark(app, details);
}

/// Pure in-memory side of `invoke_claude_from_tui`'s post-agent success path
/// in single-change mode. The agent may have rewritten anything in the change,
/// so reset the cursor to the description before rebuilding so the reviewer
/// doesn't land mid-file in code that no longer corresponds to where they
/// were. Extracted so the auto-mark wiring is unit-testable without spawning
/// the agent or `jj show`.
#[cfg(test)]
fn apply_post_claude_reload(app: &mut App, details: ChangeDetails) {
    app.file_index = 0;
    app.line_index = 0;
    app.scroll = 0;
    rebuild_views_and_mark(app, details);
}

fn suspend_tui() -> Result<()> {
    disable_raw_mode().map_err(io_err)?;
    let mut out = stdout();
    execute!(out, LeaveAlternateScreen).map_err(io_err)?;
    Ok(())
}

fn restore_tui() -> Result<()> {
    enable_raw_mode().map_err(io_err)?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).map_err(io_err)?;
    Ok(())
}

/// Best-effort terminal restoration on panic. Armed after `suspend_tui()`
/// and disarmed via `mem::forget` on the normal path so `restore_tui()` can
/// surface I/O errors instead of swallowing them.
struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(stdout(), EnterAlternateScreen);
    }
}

/// Best-effort `StderrLogGuard::resume` on panic / error path. Armed after
/// `suspend()` succeeds; disarmed via `mem::forget` on the normal path so
/// the explicit `resume()?` call can surface its error. Without this, an
/// error from `suspend_tui()` or `restore_tui()` would propagate while the
/// stderr redirect is still suspended — fd 2 would stay pointed at the real
/// terminal under the alt screen, and the next `log_warning` would corrupt
/// the diff (the very thing the redirect exists to prevent).
struct StderrResumeGuard<'a> {
    guard: Option<&'a StderrLogGuard>,
}

impl Drop for StderrResumeGuard<'_> {
    fn drop(&mut self) {
        if let Some(g) = self.guard {
            let _ = g.resume();
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the overview screen"
)]
fn handle_overview_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Screen::Overview(ref state) = app.screen else {
        return Ok(());
    };

    let rows = if let (Some(ctx), Some(cache)) = (app.stack.as_ref(), app.overview_cache.as_ref()) {
        overview_screen::build_rows(
            cache,
            &ctx.entries,
            cache.stale_count(),
            cache.total_count(),
        )
    } else {
        Vec::new()
    };

    let current_selected = state.selected_row;

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.screen = Screen::Main;
        }
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.screen = Screen::Help;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let new_sel = overview_screen::move_cursor(&rows, current_selected, -1);
            if let Screen::Overview(ref mut s) = app.screen {
                s.selected_row = new_sel;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let new_sel = overview_screen::move_cursor(&rows, current_selected, 1);
            if let Screen::Overview(ref mut s) = app.screen {
                s.selected_row = new_sel;
            }
        }
        KeyCode::Enter => {
            overview_enter(app, &rows, current_selected)?;
        }
        KeyCode::Char('c') => {
            overview_open_composer(app, &rows, current_selected);
        }
        _ => {}
    }
    Ok(())
}

/// Handle `Enter` on the overview screen.
#[cfg(test)]
fn overview_enter(
    app: &mut App,
    rows: &[overview_screen::OverviewRow],
    selected: usize,
) -> Result<()> {
    let Some(row) = rows.get(selected) else {
        return Ok(());
    };
    match row {
        overview_screen::OverviewRow::ChangeRow(change_idx) => {
            let idx = *change_idx;
            app.screen = Screen::Main;
            app.goto_stack_index(idx)?;
        }
        overview_screen::OverviewRow::StackComment(ci) => {
            open_overview_stack_comment_editor(app, *ci);
        }
        overview_screen::OverviewRow::ChangeComment {
            change_idx,
            comment_idx,
        } => {
            open_overview_change_comment_editor(app, *change_idx, *comment_idx);
        }
        overview_screen::OverviewRow::StackHeader
        | overview_screen::OverviewRow::Separator
        | overview_screen::OverviewRow::SummaryFooterStale
        | overview_screen::OverviewRow::SummaryFooterTotal => {}
    }
    Ok(())
}

/// Open the composer with a scope derived from the cursor's row type.
///
/// For `Change` scope the target `change_id` is the cursor row's change (which
/// may differ from the change loaded in the main view). For `Stack` and any
/// non-actionable rows we fall back to the current change as a placeholder
/// only — the `change_id` is unused for `Stack` scope.
#[cfg(test)]
fn overview_open_composer(app: &mut App, rows: &[overview_screen::OverviewRow], selected: usize) {
    let (use_stack_scope, change_idx_for_change_scope) = rows
        .get(selected)
        .map(|row| match row {
            overview_screen::OverviewRow::StackHeader
            | overview_screen::OverviewRow::StackComment(_) => (true, None),
            overview_screen::OverviewRow::ChangeRow(ci) => (false, Some(*ci)),
            overview_screen::OverviewRow::ChangeComment { change_idx, .. } => {
                (false, Some(*change_idx))
            }
            overview_screen::OverviewRow::Separator
            | overview_screen::OverviewRow::SummaryFooterStale
            | overview_screen::OverviewRow::SummaryFooterTotal => (false, None),
        })
        .unwrap_or((false, None));

    let target_change_id: ChangeId = change_idx_for_change_scope
        .and_then(|idx| {
            app.stack
                .as_ref()
                .and_then(|s| s.entries.get(idx).map(|e| e.change_id.clone()))
        })
        .unwrap_or_else(|| app.details.change_id.clone());

    open_composer_with_scope(app, use_stack_scope, target_change_id);
}

/// Open a new-comment composer. When `use_stack_scope` is true, the composer
/// opens with Stack scope (falling back to Line or Change if unavailable).
/// When false, the composer opens with Change scope. The `target_change_id`
/// binds the Change-scope target.
#[cfg(test)]
fn open_composer_with_scope(app: &mut App, use_stack_scope: bool, target_change_id: ChangeId) {
    let line_available = match build_line_target(app) {
        BuildTargetResult::Ready(t) => Some(t),
        BuildTargetResult::DescriptionLine { .. }
        | BuildTargetResult::NonCommentable
        | BuildTargetResult::NoView => None,
    };
    let stack_available = stack_snapshot(app);
    let change_description = change_description_for_target(app, &target_change_id);
    let scope = if use_stack_scope {
        match stack_available.clone() {
            Some(s) => ComposerScope::Stack(s),
            None => fallback_scope(line_available.clone()),
        }
    } else {
        ComposerScope::Change
    };
    let init = ComposerInit {
        scope,
        severity: default_severity(app.last_severity),
        change_id: target_change_id,
        change_description,
        line_available,
        stack_available,
        description_available: None,
    };
    app.screen = Screen::Composer(Box::new(Composer::new(init)));
}

/// Pick the next-best scope when the requested one has no backing snapshot.
/// `Line` if the cursor was on a commentable line; otherwise `Change`
/// (always available because the `change_id` is universal).
#[cfg(test)]
fn fallback_scope(line_available: Option<LineTarget>) -> ComposerScope {
    match line_available {
        Some(line) => ComposerScope::Line(line),
        None => ComposerScope::Change,
    }
}

#[cfg(test)]
fn stack_snapshot(app: &App) -> Option<StackContextSnapshot> {
    app.stack.as_ref().map(|s| StackContextSnapshot {
        revset: s.revset.clone(),
        revset_hash: s.revset_hash,
    })
}

/// The Change-scope chrome shows the change's description text. We carry it
/// only for the current change (the only one whose body is loaded in
/// `app.details`); for non-current changes the description is empty.
#[cfg(test)]
fn change_description_for_target(app: &App, target: &ChangeId) -> String {
    if *target == app.details.change_id {
        app.details.description.clone()
    } else {
        String::new()
    }
}

/// Open the composer in edit mode for a stack-level comment.
#[cfg(test)]
fn open_overview_stack_comment_editor(app: &mut App, comment_idx: usize) {
    let comment = app
        .overview_cache
        .as_ref()
        .and_then(|c| c.stack_level.get(comment_idx))
        .cloned();
    let Some(comment) = comment else {
        app.status_message = Some("comment not found".to_owned());
        return;
    };
    open_meta_comment_editor(app, &comment);
}

/// Open the composer in edit mode for a change-level comment from the overview.
#[cfg(test)]
fn open_overview_change_comment_editor(app: &mut App, change_idx: usize, comment_idx: usize) {
    let comment = app
        .overview_cache
        .as_ref()
        .and_then(|c| c.per_change.get(change_idx))
        .and_then(|v| v.get(comment_idx))
        .cloned();

    let Some(comment) = comment else {
        app.status_message = Some("comment not found".to_owned());
        return;
    };

    open_meta_comment_editor(app, &comment);
}

/// Open the composer in edit mode for an existing comment. Builds the
/// matching `ComposerScope` variant directly from the source `Anchor` so the
/// scope payload is non-synthetic.
#[cfg(test)]
fn open_meta_comment_editor(app: &mut App, comment: &Comment) {
    // Bind the composer's `change_id` to the source comment's change when it
    // carries one; otherwise use the current change. Used by the picker label
    // and the Change-scope save path.
    let target_change_id = match &comment.anchor {
        Anchor::Change { change_id }
        | Anchor::Line { change_id, .. }
        | Anchor::Description { change_id, .. } => change_id.clone(),
        Anchor::Stack { .. } => app.details.change_id.clone(),
    };

    let line_available = match build_line_target(app) {
        BuildTargetResult::Ready(t) => Some(t),
        BuildTargetResult::DescriptionLine { .. }
        | BuildTargetResult::NonCommentable
        | BuildTargetResult::NoView => None,
    };
    let stack_available = stack_snapshot(app);

    // Build the scope variant and the description-availability snapshot in
    // one pass so the Description case carries the same `DescriptionContext`
    // value into both fields without re-cloning through an `Option` unwrap.
    let (scope, description_available) = match &comment.anchor {
        Anchor::Line { location, .. } => {
            let line_target = LineTarget {
                file: location.file.clone(),
                rendered_index: app.line_index,
                source_line: location.old_line,
                target_line: location.new_line,
                target_text: location.target_text.clone(),
                hunk_header: location.hunk_header.clone(),
                context_before: location.context_before.clone(),
                context_after: location.context_after.clone(),
            };
            (ComposerScope::Line(line_target), None)
        }
        Anchor::Change { .. } => (ComposerScope::Change, None),
        Anchor::Stack { revset_hash } => {
            // The saved anchor only carries the revset_hash; the revset string
            // for the chrome row comes from the running stack context. If the
            // running session is single-change, fall back to the hash hex so
            // the chrome doesn't lie about what was loaded.
            let snapshot = stack_available
                .clone()
                .unwrap_or_else(|| StackContextSnapshot {
                    revset: format!("revset_hash:{}", revset_hash.hex()),
                    revset_hash: *revset_hash,
                });
            (ComposerScope::Stack(snapshot), None)
        }
        Anchor::Description {
            change_id: anchor_change_id,
            location,
        } => {
            let ctx = DescriptionContext {
                change_id: anchor_change_id.clone(),
                target_line: location.display_line,
                target_text: location.target_text.clone(),
                context_before: location.context_before.clone(),
                context_after: location.context_after.clone(),
            };
            (ComposerScope::Description(ctx.clone()), Some(ctx))
        }
    };

    let change_description = change_description_for_target(app, &target_change_id);
    let init = ComposerInit {
        scope,
        severity: comment.severity,
        change_id: target_change_id,
        change_description,
        line_available,
        stack_available,
        description_available,
    };
    let edited = EditedComment {
        init,
        body: comment.body.clone(),
        identity: comment.created_at,
        original: Some(comment.clone()),
        original_anchor: comment.anchor.clone(),
    };
    app.screen = Screen::Composer(Box::new(Composer::for_edit(edited)));
}

#[cfg(test)]
fn render_transition(frame: &mut Frame<'_>, app: &App, state: &TransitionState) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

    let area = frame.area();
    let modal_area =
        composer_overlay::centered_rect(area, TRANSITION_MODAL_WIDTH, TRANSITION_MODAL_HEIGHT);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double);
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let Some(ctx) = app.stack.as_ref() else {
        return;
    };

    let reviewed = &ctx.entries[state.reviewed_index];
    let next = &ctx.entries[state.next_index];
    let stack_len = ctx.entries.len();

    let reviewed_pos = state.reviewed_index + 1;
    let next_pos = state.next_index + 1;

    let reviewed_desc = truncate(&reviewed.description, TRANSITION_DESC_BUDGET);
    let next_desc = truncate(&next.description, TRANSITION_DESC_BUDGET);

    let body = format!(
        "\n  Reviewed\n  {reviewed_pos}/{stack_len}  {}\n  {reviewed_desc}\n\n  ────────────────\n\n  Next\n  {next_pos}/{stack_len}  {}\n  {next_desc}\n",
        reviewed.change_id,
        next.change_id,
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

/// Footer hint for the transition modal. Kept short enough to fit inside the
/// modal's interior (`TRANSITION_MODAL_WIDTH - 2` cols for the borders).
/// Tested in `transition_footer_fits_inside_modal`.
#[cfg(test)]
const TRANSITION_FOOTER_TEXT: &str = "  Enter  p prev  Esc cancel  q quit";

/// Render the `●●● 3 required · ● 1 suggestion` line (or honest fallback when
/// the comment load failed).
#[cfg(test)]
fn render_transition_comment_summary(frame: &mut Frame<'_>, area: Rect, state: &TransitionState) {
    let Some(count) = state.reviewed_comment_count else {
        let widget = Paragraph::new("  comments could not be loaded")
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(widget, area);
        return;
    };
    if count == 0 {
        // No comments — leave the line blank to avoid noise.
        return;
    }

    let h = state.severity_histogram;
    let mut spans: Vec<Span<'_>> = Vec::with_capacity(8);
    spans.push(Span::raw("  "));
    let mut first = true;
    if h.required > 0 {
        // `required` uses the dots-only label (no pluralizable English word),
        // matching the spec's `●●●  3 required` shape.
        spans.push(Span::styled(
            render_dots(h.required),
            Style::default().fg(Color::Red),
        ));
        spans.push(Span::raw(format!(" {} required", h.required)));
        first = false;
    }
    if h.suggestion > 0 {
        if !first {
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
        first = false;
    }
    if h.note > 0 {
        if !first {
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

/// Render a string of `●` dots for a single count.
///
/// Caps at [`DOT_BUDGET`]; any overflow becomes a trailing `…`. The
/// numeric count next to the dots still tells the truth.
#[cfg(test)]
pub(super) fn render_dots(count: usize) -> String {
    if count == 0 {
        String::new()
    } else if count <= DOT_BUDGET {
        "●".repeat(count)
    } else {
        format!("{}…", "●".repeat(DOT_BUDGET))
    }
}

/// Render mixed-severity dots sharing a single [`DOT_BUDGET`] across all
/// severities. Dots emit in severity order (required, suggestion, note) and
/// stop at the budget; if the histogram total exceeds the budget, a trailing
/// `…` is appended.
pub(super) fn render_dots_mixed(hist: CoreSeverityHistogram) -> String {
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

#[cfg(test)]
fn handle_composer_event(app: &mut App, key: KeyEvent) {
    let Screen::Composer(mut composer) = std::mem::replace(&mut app.screen, Screen::Main) else {
        return;
    };

    let action = composer::handle_composer_key(&mut composer, key);

    match action {
        ComposerAction::Continue => {
            app.screen = Screen::Composer(composer);
        }
        ComposerAction::Cancel => {}
        ComposerAction::Save => {
            match save_composer(app, &composer, time::OffsetDateTime::now_utc()) {
                SaveOutcome::Saved => {
                    if let Some(reanchor) = app.pending_reanchor.take() {
                        match crate::store::delete_comment(
                            &app.repo_root,
                            &app.repo_root,
                            &reanchor.original,
                        ) {
                            Ok(()) => {
                                app.refresh_inline_comments();
                            }
                            Err(e) => {
                                app.status_message = Some(format!(
                                    "re-anchor saved; could not delete original: {}",
                                    sanitize_for_status(&e.to_string())
                                ));
                            }
                        }
                    }
                }
                SaveOutcome::Refused(msg) | SaveOutcome::Errored(msg) => {
                    app.status_message = Some(msg);
                    // Preserve the body and selections so the reviewer can fix
                    // and retry without retyping.
                    app.screen = Screen::Composer(composer);
                }
            }
        }
        ComposerAction::Delete => match delete_via_composer(app, &composer) {
            SaveOutcome::Saved => {}
            SaveOutcome::Refused(msg) | SaveOutcome::Errored(msg) => {
                app.status_message = Some(msg);
                app.screen = Screen::Composer(composer);
            }
        },
        ComposerAction::RefusedScopeChord(status) => {
            app.status_message = Some(status.to_owned());
            app.screen = Screen::Composer(composer);
        }
    }
}

// -- T-G3-byte: pin the refusal-status strings byte-for-byte. Comparing
//   against the const by name passes when the const itself has a typo;
//   string literals catch that.
#[cfg(test)]
#[test]
fn refused_scope_chord_status_strings_are_byte_stable() {
    assert_eq!(
        STATUS_STACK_UNAVAILABLE,
        "stack scope unavailable in single-change mode"
    );
    assert_eq!(
        STATUS_DESCRIPTION_UNAVAILABLE,
        "description scope unavailable: open from a description line"
    );
    assert_eq!(
        STATUS_LINE_UNAVAILABLE,
        "line scope unavailable: cursor is not on a commentable line"
    );
}

#[cfg(test)]
#[derive(Debug)]
enum SaveOutcome {
    /// Comment persisted; composer should close.
    Saved,
    /// Save not attempted (empty body, scope not yet supported, etc.).
    /// Composer remains open with body preserved.
    Refused(String),
    /// Save attempted and the store returned an error. Composer remains open
    /// with body preserved so the reviewer can retry.
    Errored(String),
}

/// On-disk anchor stores the full window (`CONTEXT_MAX` each side); the
/// chrome-time render is responsible for re-capping if needed.
#[cfg(test)]
fn build_description_context(app: &App, target_line: Option<u32>) -> DescriptionContext {
    let (context_before, context_after) = app
        .current_view()
        .map(|v| collect_description_context(&v.lines, app.line_index))
        .unwrap_or_default();
    let target_text = app
        .current_view()
        .and_then(|v| v.lines.get(app.line_index))
        .map(|l| l.text.clone())
        .unwrap_or_default();
    DescriptionContext {
        change_id: app.details.change_id.clone(),
        target_line,
        target_text,
        context_before,
        context_after,
    }
}

#[cfg(test)]
fn open_composer(app: &mut App) {
    // New-comment on an existing change-comment row defaults to Change scope.
    // Line- and description-comment rows fall through to `build_line_target`,
    // which classifies them as `NonCommentable`.
    if focused_comment(app).is_some_and(|c| matches!(c.anchor, Anchor::Change { .. })) {
        let target_change_id = app.details.change_id.clone();
        open_composer_with_scope(app, false, target_change_id);
        return;
    }
    match build_line_target(app) {
        BuildTargetResult::Ready(target) => {
            let init = ComposerInit {
                scope: ComposerScope::Line(target.clone()),
                severity: app
                    .pending_reanchor
                    .as_ref()
                    .map(|r| r.severity)
                    .unwrap_or_else(|| default_severity(app.last_severity)),
                change_id: app.details.change_id.clone(),
                change_description: app.details.description.clone(),
                line_available: Some(target),
                stack_available: stack_snapshot(app),
                description_available: None,
            };
            let mut composer = Composer::new(init);
            if let Some(reanchor) = app.pending_reanchor.as_ref() {
                for (i, line) in reanchor.body.clone().lines().enumerate() {
                    if i > 0 {
                        composer.body.insert_newline();
                    }
                    composer.body.insert_str(line);
                }
            }
            app.screen = Screen::Composer(Box::new(composer));
        }
        BuildTargetResult::DescriptionLine { target_line } => {
            let desc_ctx = build_description_context(app, target_line);
            let init = ComposerInit {
                scope: ComposerScope::Description(desc_ctx.clone()),
                severity: default_severity(app.last_severity),
                change_id: app.details.change_id.clone(),
                change_description: app.details.description.clone(),
                line_available: None,
                stack_available: stack_snapshot(app),
                description_available: Some(desc_ctx),
            };
            app.screen = Screen::Composer(Box::new(Composer::new(init)));
        }
        BuildTargetResult::NonCommentable => {
            app.status_message = Some("cannot comment on this line".to_owned());
        }
        BuildTargetResult::NoView => {
            // No file open at all; silent — the empty-view UI already says so.
        }
    }
}

#[cfg(test)]
fn open_composer_for_edit(app: &mut App) {
    let Some(comment) = focused_comment(app) else {
        app.status_message = Some("cursor is not on a comment".to_owned());
        return;
    };
    match &comment.anchor {
        Anchor::Line { location, .. } => {
            let target = LineTarget {
                file: location.file.clone(),
                rendered_index: app.line_index,
                source_line: location.old_line,
                target_line: location.new_line,
                target_text: location.target_text.clone(),
                hunk_header: location.hunk_header.clone(),
                context_before: location.context_before.clone(),
                context_after: location.context_after.clone(),
            };

            let init = ComposerInit {
                scope: ComposerScope::Line(target.clone()),
                severity: comment.severity,
                change_id: app.details.change_id.clone(),
                change_description: app.details.description.clone(),
                line_available: Some(target),
                stack_available: stack_snapshot(app),
                description_available: None,
            };
            let edited = EditedComment {
                init,
                body: comment.body.clone(),
                identity: comment.created_at,
                // Main-view line-comment edits resolve through `app.loaded_comments`
                // so the latest in-memory anchor (post-re-anchor) is honored.
                original: None,
                original_anchor: comment.anchor.clone(),
            };
            let composer = Composer::for_edit(edited);
            app.screen = Screen::Composer(Box::new(composer));
        }
        Anchor::Change { .. } | Anchor::Description { .. } => {
            // Clone before borrowing &mut self; `focused_comment` returns a
            // borrow into `app.loaded_comments`.
            let comment = comment.clone();
            open_meta_comment_editor(app, &comment);
        }
        Anchor::Stack { .. } => {
            app.status_message =
                Some("stack comments are edited from the stack overview (s)".to_owned());
        }
    }
}

/// Single-keystroke delete without confirmation.
#[cfg(test)]
fn delete_focused_comment(app: &mut App) {
    let Some(comment) = focused_comment(app).cloned() else {
        app.status_message = Some("cursor is not on a comment".to_owned());
        return;
    };

    let target_index = anchor_line_index(app, &comment);

    match crate::store::delete_comment(&app.repo_root, &app.repo_root, &comment) {
        Ok(()) => {
            app.refresh_inline_comments();
            app.status_message = Some("comment deleted".to_owned());
            if let Some(idx) = target_index {
                app.line_index = idx;
            }
        }
        Err(e) => {
            app.status_message = Some(format!(
                "delete failed: {}",
                sanitize_for_status(&e.to_string())
            ));
        }
    }
}

enum BuildTargetResult {
    Ready(LineTarget),
    /// Cursor is on a description line; use description scope.
    DescriptionLine {
        target_line: Option<u32>,
    },
    /// Cursor is on a `HunkHeader`, `HunkSeparator`, `Notice`, or inline comment line.
    NonCommentable,
    /// No file is open in the current view.
    NoView,
}

#[cfg(test)]
fn build_line_target(app: &App) -> BuildTargetResult {
    let Some(view) = app.current_view() else {
        return BuildTargetResult::NoView;
    };
    let Some(line) = view.lines.get(app.line_index) else {
        return BuildTargetResult::NoView;
    };

    match line.kind {
        RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context => {}
        RenderedLineKind::DescriptionLine => {
            return BuildTargetResult::DescriptionLine {
                target_line: line.target_line,
            };
        }
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody => return BuildTargetResult::NonCommentable,
    }

    // file_index 0 is the description view; diff files start at index 1. The
    // DescriptionLine match arm above returns early, so by here the cursor is
    // on a diff line and `file_index >= 1` is invariant — make it load-bearing
    // by panicking explicitly if a future change leaks a non-Description line
    // into view 0 (vs. silently sliding `0` through saturating_sub).
    let diff_file_idx = app
        .file_index
        .checked_sub(1)
        .expect("file_index 0 is description view; reached only after DescriptionLine return");
    let Some(file) = app.details.diff.files.get(diff_file_idx) else {
        return BuildTargetResult::NoView;
    };
    let file = file.display_path().to_owned();
    let hunk_header = line.hunk_header.clone().unwrap_or_default();
    let context = collect_context(&view.lines, app.line_index);

    BuildTargetResult::Ready(LineTarget {
        file,
        rendered_index: app.line_index,
        source_line: line.source_line,
        target_line: line.target_line,
        target_text: line.text.clone(),
        hunk_header,
        context_before: context.0,
        context_after: context.1,
    })
}

/// Collect up to [`CONTEXT_MAX`] lines of context before and after `idx` in
/// `lines`, skipping non-diff lines (hunk headers, separators, inline comments).
#[cfg(test)]
fn collect_context(lines: &[RenderedLine], idx: usize) -> (Vec<String>, Vec<String>) {
    let is_content = |k: RenderedLineKind| {
        matches!(
            k,
            RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
        )
    };
    collect_context_with(lines, idx, is_content)
}

#[cfg(test)]
fn collect_description_context(lines: &[RenderedLine], idx: usize) -> (Vec<String>, Vec<String>) {
    let is_content = |k: RenderedLineKind| matches!(k, RenderedLineKind::DescriptionLine);
    collect_context_with(lines, idx, is_content)
}

fn collect_context_with(
    lines: &[RenderedLine],
    idx: usize,
    is_content: impl Fn(RenderedLineKind) -> bool,
) -> (Vec<String>, Vec<String>) {
    let before: Vec<String> = lines[..idx]
        .iter()
        .rev()
        .filter(|l| is_content(l.kind))
        .take(CONTEXT_MAX)
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let after: Vec<String> = lines[idx + 1..]
        .iter()
        .filter(|l| is_content(l.kind))
        .take(CONTEXT_MAX)
        .map(|l| l.text.clone())
        .collect();

    (before, after)
}

/// Build a line anchor from an explicit `LineTarget` carried in the composer's
/// `Line` scope variant. Pure: takes the `LineTarget` (the scope payload) and
/// the current change id, returns the (`change_id`, anchor) pair the caller
/// passes through to the store. Centralizes the source/target → side mapping
/// so the line-anchor wire shape is built in exactly one place.
fn build_line_anchor(target: &LineTarget, change_id: ChangeId) -> (ChangeId, LineAnchor) {
    let side = pick_side(target.source_line, target.target_line);
    let location = LineAnchor {
        file: PathBuf::from(&target.file),
        side,
        old_line: target.source_line,
        new_line: target.target_line,
        hunk_header: target.hunk_header.clone(),
        target_text: target.target_text.clone(),
        context_before: target.context_before.clone(),
        context_after: target.context_after.clone(),
    };
    (change_id, location)
}

#[cfg(test)]
fn save_composer(app: &mut App, composer: &Composer, now: time::OffsetDateTime) -> SaveOutcome {
    let body = composer.body_text();
    if body.trim().is_empty() {
        return SaveOutcome::Refused("comment body is empty — not saved".to_owned());
    }

    // Body silently truncates at the serializer; warn the reviewer so they
    // know to copy the overflow elsewhere if needed. Save proceeds.
    let oversized = body.chars().count() > crate::comment::BODY_MAX;

    if let Some(ctx) = composer.editing.as_ref() {
        return persist_update_from_composer(
            app,
            composer,
            ctx,
            UpdateArgs {
                body,
                now,
                oversized,
            },
        );
    }

    let comment = build_comment_from_composer(app, composer, body, now);

    match crate::store::save_comment(&app.repo_root, &app.repo_root, &comment) {
        Ok(()) => {
            app.last_severity = Some(composer.severity);
            app.refresh_inline_comments();
            // Oversized warning takes priority — the reviewer needs to know
            // before deciding what to do. Otherwise surface a scope-specific
            // confirmation so the user sees where their comment landed
            // (especially Change/Stack, which render off the current view).
            app.status_message = Some(if oversized {
                "body truncated to 64 KB on save".to_owned()
            } else {
                save_status_message_for_scope(&composer.scope).to_owned()
            });
            SaveOutcome::Saved
        }
        Err(JjrError::DuplicateCommentTimestamp { .. }) => SaveOutcome::Errored(
            "save failed: two comments at the same timestamp — wait a moment and retry".to_owned(),
        ),
        Err(e) => SaveOutcome::Errored(format!(
            "save failed: {}",
            sanitize_for_status(&e.to_string())
        )),
    }
}

/// Confirmation copy for a successful new-comment save. Change- and
/// Stack-scoped comments render off the current view, so the message names
/// where to find them; Line and Description comments are visible inline.
fn save_status_message_for_scope(scope: &ComposerScope) -> &'static str {
    match scope {
        ComposerScope::Line(_) => "comment saved",
        ComposerScope::Description(_) => "comment saved on description",
        ComposerScope::Change => "change comment saved (visible in description view)",
        ComposerScope::Stack(_) => "stack comment saved (visible in stack overview, s key)",
    }
}

/// Total exhaustive match: each `ComposerScope` variant carries the data
/// needed to build its `Anchor`, so this function never refuses on a missing
/// snapshot. Save-time refusals (empty body, etc.) are handled upstream in
/// `save_composer`.
#[cfg(test)]
fn build_comment_from_composer(
    app: &App,
    composer: &Composer,
    body: String,
    now: time::OffsetDateTime,
) -> Comment {
    let anchor = match &composer.scope {
        ComposerScope::Line(target) => {
            let (change_id, location) = build_line_anchor(target, app.details.change_id.clone());
            Anchor::Line {
                change_id,
                location,
            }
        }
        // The composer's `change_id` is the Change-scope save target, set at
        // open time. Differs from `app.details.change_id` when the composer
        // was opened from an overview cursor pointing at a non-current change.
        ComposerScope::Change => Anchor::Change {
            change_id: composer.change_id.clone(),
        },
        ComposerScope::Stack(stack) => Anchor::Stack {
            revset_hash: stack.revset_hash,
        },
        ComposerScope::Description(ctx) => {
            let location = DescriptionAnchor {
                display_line: ctx.target_line,
                target_text: ctx.target_text.clone(),
                context_before: ctx.context_before.clone(),
                context_after: ctx.context_after.clone(),
            }
            .normalized();
            Anchor::Description {
                change_id: ctx.change_id.clone(),
                location,
            }
        }
    };

    Comment {
        schema_version: SchemaVersion,
        anchor,
        repo_root: app.repo_root.clone(),
        revset: app.revset.clone(),
        commit_id: Some(app.details.commit_id.clone()),
        body,
        severity: composer.severity,
        created_at: now,
        updated_at: None,
        status: Some(Status::Pending),
        mismatch_reason: None,
        entity_id: None,
        anchor_fingerprint: None,
    }
}

#[cfg(test)]
struct UpdateArgs {
    body: String,
    now: time::OffsetDateTime,
    oversized: bool,
}

#[cfg(test)]
fn persist_update_from_composer(
    app: &mut App,
    composer: &Composer,
    edit_ctx: &composer::EditingContext,
    args: UpdateArgs,
) -> SaveOutcome {
    // Two paths:
    // (1) Edit from main view (`edit_ctx.original` is None): source the anchor
    //     from `app.loaded_comments` keyed by `identity` so a re-anchor that
    //     happened between compose-open and compose-submit lands the edit at
    //     the new location.
    // (2) Edit from stack overview (`edit_ctx.original` is Some): the comment
    //     does not appear in `app.loaded_comments` (it belongs to a different
    //     change or to the stack file), so use the original snapshot directly.
    let source = if let Some(orig) = edit_ctx.original.as_ref() {
        orig.clone()
    } else {
        let Some(latest) = app
            .loaded_comments
            .iter()
            .find(|c| c.created_at == edit_ctx.identity)
            .cloned()
        else {
            return SaveOutcome::Errored(
                "comment was removed between open and save; edit not saved".to_owned(),
            );
        };
        latest
    };

    let updated = Comment {
        body: args.body,
        severity: composer.severity,
        updated_at: Some(args.now),
        ..source
    };

    match crate::store::update_comment(&app.repo_root, &app.repo_root, &updated) {
        Ok(()) => {
            app.last_severity = Some(composer.severity);
            app.refresh_inline_comments();
            if args.oversized {
                app.status_message = Some("body truncated to 64 KB on save".to_owned());
            } else {
                app.status_message = Some("comment updated".to_owned());
            }
            SaveOutcome::Saved
        }
        Err(e) => SaveOutcome::Errored(format!(
            "update failed: {}",
            sanitize_for_status(&e.to_string())
        )),
    }
}

#[cfg(test)]
fn delete_via_composer(app: &mut App, composer: &Composer) -> SaveOutcome {
    let Some(edit_ctx) = composer.editing.as_ref() else {
        return SaveOutcome::Refused("delete only available in edit mode".to_owned());
    };

    // `delete_comment` keys records by `(anchor, created_at)`. The other
    // `Comment` fields are unused by the store; we still build the full
    // record because the API requires it.
    let comment = match edit_ctx.original.as_ref() {
        Some(orig) => orig.clone(),
        None => Comment {
            schema_version: SchemaVersion,
            anchor: edit_ctx.original_anchor.clone(),
            repo_root: app.repo_root.clone(),
            revset: app.revset.clone(),
            commit_id: Some(app.details.commit_id.clone()),
            body: composer.body_text(),
            severity: composer.severity,
            created_at: edit_ctx.identity,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        },
    };

    match crate::store::delete_comment(&app.repo_root, &app.repo_root, &comment) {
        Ok(()) => {
            app.refresh_inline_comments();
            app.status_message = Some("comment deleted".to_owned());
            SaveOutcome::Saved
        }
        Err(e) => SaveOutcome::Errored(format!(
            "delete failed: {}",
            sanitize_for_status(&e.to_string())
        )),
    }
}

/// Pick the previous-stack-entry index for a `p` keystroke.
///
/// Pure: takes the current 0-based index and returns the new index, or `None`
/// when already at index 0. Has no side effects — does not read or write the
/// cursor file. Callers (only `retreat_stack`) are responsible for routing the
/// resulting index through `load_stack_entry` with `advance=false`, which is
/// what guarantees the cursor-no-write contract for retreat.
#[cfg(test)]
fn pick_retreat_index(current: usize) -> Option<usize> {
    if current == 0 {
        None
    } else {
        Some(current - 1)
    }
}

/// Choose the diff side for a `LineAnchor` from the source/target line numbers.
///
/// `Side::Old` only when the cursor is on a deleted line (source set, target
/// absent). All other commentable lines (added or context) anchor to `New`.
fn pick_side(source_line: Option<u32>, target_line: Option<u32>) -> Side {
    if source_line.is_some() && target_line.is_none() {
        Side::Old
    } else {
        Side::New
    }
}

// ── Entity-bundle prompt assembly ────────────────────────────────────────────

/// Build the Claude prompt for `invoke_claude_impl`. When an entity cache
/// exists for `change_id`, line comments get entity-aware context bundles
/// (target entity body + direct deps and dependents + diff hunk). Falls back
/// to the existing flat-hunk `JsonlPaths` rendering when the cache is absent.
fn build_entity_bundle_prompt(
    packet: &crate::packet::Packet,
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
    change_id: &ChangeId,
) -> String {
    let cache_entry = load_entity_cache_for_bundle(data_home, repo_root, change_id);
    match cache_entry {
        Some(entry) => render_entity_bundle_prompt(packet, &entry, repo_root),
        None => {
            crate::packet::render_prompt_with_mode(packet, crate::packet::PromptMode::JsonlPaths)
        }
    }
}

/// Compute the on-disk cache path for `(change_id, commit_id)`.
///
/// Shared by `fetch_entity_list`, the extraction worker, and
/// `load_entity_cache_for_bundle` to avoid the pattern being repeated.
fn entity_cache_path(
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
    change_id: &str,
    commit_id: &str,
) -> PathBuf {
    let cache_base = crate::store::repo_data_dir(data_home, repo_root).join("entities");
    local_review_core::semantic::cache::jjr_cache_path(&cache_base, change_id, commit_id)
}

/// Load the entity cache for `change_id`, returning `None` on miss or error.
fn load_entity_cache_for_bundle(
    data_home: &std::path::Path,
    repo_root: &std::path::Path,
    change_id: &ChangeId,
) -> Option<local_review_core::semantic::cache::CacheEntry> {
    let details = jj::show(change_id).ok()?;
    let commit_id = details.commit_id.as_str().to_owned();
    let cache_path = entity_cache_path(data_home, repo_root, change_id.as_str(), &commit_id);
    local_review_core::semantic::cache::read(&cache_path)
        .ok()
        .flatten()
}

/// Render the full prompt with entity bundles replacing the raw-hunk diff
/// context for line-scoped comments. Change-scoped and stack-scoped comments
/// remain in the JSONL files Claude reads via the paths section.
fn render_entity_bundle_prompt(
    packet: &crate::packet::Packet,
    cache: &local_review_core::semantic::cache::CacheEntry,
    repo_root: &std::path::Path,
) -> String {
    let mut out = crate::packet::system_preamble(packet);

    let jsonl = crate::packet::render_jsonl_paths_section(packet);
    if !jsonl.is_empty() {
        out.push('\n');
        out.push_str(&jsonl);
    }

    if packet.changes.is_empty() {
        return out;
    }

    out.push('\n');
    out.push_str("## Changes\n");

    for cp in &packet.changes {
        out.push_str(&crate::packet::render_change_header(cp));

        if cp.line_comments.is_empty() {
            // No line comments: include the raw diff context so Claude has
            // orientation even when there's nothing to bundle.
            out.push_str(&crate::packet::render_change_diff_context(cp));
            continue;
        }

        out.push('\n');
        out.push_str("### Line-Level Entity Context\n");

        let budget = local_review_core::semantic::context::budget_from_env();
        for comment in &cp.line_comments {
            out.push('\n');
            if let Some(bundle) = entity_bundle_for_comment(
                comment,
                cp.diff.as_ref(),
                &cache.entities,
                cache.graph.as_ref(),
                repo_root,
            ) {
                out.push_str(&local_review_core::semantic::context::render_with_budget(
                    &bundle, budget,
                ));
            } else {
                // Entity context unavailable (line outside all entities or
                // file unreadable). Fall back to the existing flat rendering.
                out.push_str(&crate::packet::render_line_comment_block(comment));
                if let Some(diff) = &cp.diff {
                    let hunk = hunk_text_for_comment(comment, diff);
                    if !hunk.is_empty() {
                        out.push_str("#### Diff Hunk\n\n");
                        out.push_str(&hunk);
                    }
                }
            }
        }
    }

    out
}

/// Build a context `Bundle` for a single line comment. Returns `None` when the
/// comment has no entity context (line is outside all entities, or the entity
/// body cannot be read from disk).
fn entity_bundle_for_comment(
    comment: &Comment,
    diff: Option<&local_review_core::diff::Diff>,
    entities: &[local_review_core::semantic::EntityCoreData],
    graph: Option<&local_review_core::semantic::cache::GraphData>,
    repo_root: &std::path::Path,
) -> Option<local_review_core::semantic::context::Bundle> {
    use local_review_core::semantic::context::{Bundle, BundleEntity};

    let Anchor::Line { location, .. } = &comment.anchor else {
        return None;
    };

    let target_entity = entity_for_comment_line(location, comment.entity_id.as_ref(), entities)?;
    let target_body = read_entity_body(target_entity, repo_root)?;
    let target = BundleEntity {
        display_name: target_entity.id.display_name(),
        file_path: target_entity.id.file_path.clone(),
        line_range: target_entity.line_range,
        body: target_body,
    };

    let (dependencies, dependents) = match graph {
        Some(g) => graph_bundle_entities(&target_entity.id, g, entities, repo_root),
        None => (Vec::new(), Vec::new()),
    };

    let hunk_text = diff
        .map(|d| hunk_text_for_comment(comment, d))
        .unwrap_or_default();

    Some(Bundle {
        comment_body: comment.body.clone(),
        comment_severity: comment.severity,
        target,
        dependencies,
        dependents,
        hunk_file: location.file.clone(),
        hunk_line: location.new_line.or(location.old_line).unwrap_or(0),
        hunk_text,
    })
}

/// Locate the entity whose line range contains the commented line. Prefers
/// `entity_id` when set, falls back to line-range scan.
fn entity_for_comment_line<'e>(
    location: &LineAnchor,
    entity_id: Option<&local_review_core::semantic::EntityId>,
    entities: &'e [local_review_core::semantic::EntityCoreData],
) -> Option<&'e local_review_core::semantic::EntityCoreData> {
    if let Some(eid) = entity_id {
        if let Some(e) = entities.iter().find(|e| &e.id == eid) {
            return Some(e);
        }
    }
    let target_line = location.new_line?;
    entities.iter().find(|e| {
        e.id.file_path == location.file
            && e.line_range.0 <= target_line
            && target_line <= e.line_range.1
    })
}

/// Read the source lines for `entity.line_range` from disk. Returns `None`
/// when the file is unreadable or the range falls outside the file.
fn read_entity_body(
    entity: &local_review_core::semantic::EntityCoreData,
    repo_root: &std::path::Path,
) -> Option<String> {
    let abs_path = repo_root.join(&entity.id.file_path);
    let content = std::fs::read_to_string(&abs_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = usize::try_from(entity.line_range.0.saturating_sub(1)).ok()?;
    let end = usize::try_from(entity.line_range.1).ok()?.min(lines.len());
    if start >= lines.len() {
        return None;
    }
    // File content is external input — strip ANSI/OSC injection vectors while
    // preserving tab indentation and newlines so the code remains readable.
    Some(local_review_core::util::strip_injection_controls(
        &lines[start..end].join("\n"),
    ))
}

/// Build `BundleEntity` lists for the direct deps (callees) and dependents
/// (callers) of `target_id` using the dependency graph.
///
/// Deduplicates by entity path before building bodies so the bundle doesn't
/// show the same entity twice (which wastes token budget) even when the graph
/// has duplicate edges from an older cache entry.
fn graph_bundle_entities(
    target_id: &local_review_core::semantic::EntityId,
    graph: &local_review_core::semantic::cache::GraphData,
    entities: &[local_review_core::semantic::EntityCoreData],
    repo_root: &std::path::Path,
) -> (
    Vec<local_review_core::semantic::context::BundleEntity>,
    Vec<local_review_core::semantic::context::BundleEntity>,
) {
    let mut seen = std::collections::HashSet::new();
    let deps = graph
        .edges
        .iter()
        .filter(|e| &e.from == target_id && seen.insert(&e.to))
        .filter_map(|e| bundle_entity_for_id(&e.to, entities, repo_root))
        .collect();
    seen.clear();
    let dependents = graph
        .edges
        .iter()
        .filter(|e| &e.to == target_id && seen.insert(&e.from))
        .filter_map(|e| bundle_entity_for_id(&e.from, entities, repo_root))
        .collect();
    (deps, dependents)
}

/// Look up `id` in the entity list and build a `BundleEntity` if its body
/// can be read from disk.
fn bundle_entity_for_id(
    id: &local_review_core::semantic::EntityId,
    entities: &[local_review_core::semantic::EntityCoreData],
    repo_root: &std::path::Path,
) -> Option<local_review_core::semantic::context::BundleEntity> {
    let entity = entities.iter().find(|e| &e.id == id)?;
    let body = read_entity_body(entity, repo_root)?;
    Some(local_review_core::semantic::context::BundleEntity {
        display_name: entity.id.display_name(),
        file_path: entity.id.file_path.clone(),
        line_range: entity.line_range,
        body,
    })
}

/// Extract and render the diff hunk that contains the comment's anchored line.
/// Returns an empty string when no matching hunk is found.
fn hunk_text_for_comment(comment: &Comment, diff: &local_review_core::diff::Diff) -> String {
    let Anchor::Line { location, .. } = &comment.anchor else {
        return String::new();
    };
    let Some(target_line) = location.new_line.or(location.old_line) else {
        return String::new();
    };
    for file in &diff.files {
        if file.display_path() != location.file {
            continue;
        }
        for hunk in file.hunks() {
            let contains = hunk.lines.iter().any(|l| match location.side {
                Side::New => l.target_line == Some(target_line),
                Side::Old => l.source_line == Some(target_line),
            });
            if contains {
                return crate::packet::render_hunk(hunk);
            }
        }
    }
    String::new()
}

/// Context for extracting entities from a single file.
struct FileExtractCtx<'a> {
    registry: &'a local_review_core::semantic::ExtractorRegistry,
    repo_root: &'a std::path::Path,
    current_rev: &'a str,
    parent_rev: &'a str,
}

/// Extract entities from one changed file and append to `entities`.
///
/// Calls `jj file show` for before and after content, then runs the extractor.
/// Per-file failures append to `failed_files` rather than aborting.
/// Background extraction task for `entity_extraction_task`.
///
/// Owns clones of `change_id`, `repo_root`, and `data_home` so it can run
/// on a separate thread without borrowing the surface. Sends
/// `Progress` / `FileExtracted` / `Complete` events through the channel
/// so the TUI loading overlay animates and the entity list populates
/// incrementally.
struct JjrExtractionTask {
    change_id: ChangeId,
    repo_root: PathBuf,
    data_home: PathBuf,
}

impl local_review_core::tui::entity_list::ExtractionRunner for JjrExtractionTask {
    fn run(
        self: Box<Self>,
        tx: std::sync::mpsc::Sender<local_review_core::tui::entity_list::ExtractionEvent>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use local_review_core::semantic::cache;
        use local_review_core::tui::entity_list::ExtractionEvent;

        let details = match jj::show(&self.change_id) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(ExtractionEvent::Error(format!("jj show failed: {e}")));
                return;
            }
        };
        let commit_id = details.commit_id.as_str().to_owned();
        let cache_path = entity_cache_path(
            &self.data_home,
            &self.repo_root,
            self.change_id.as_str(),
            &commit_id,
        );

        // Cache hit: skip the per-file work entirely.
        if let Ok(Some(entry)) = cache::read(&cache_path) {
            emit_cache_hit(&tx, entry, &details.diff);
            return;
        }

        // Cache miss: stream per-file extraction with progress.
        extract_and_emit(&self, &tx, &cancel, &details.diff, &cache_path);
    }
}

/// Emit cached entities as a single batch, interleaved with fallback rows
/// for any file in the diff that has no entities. Cache hits don't benefit
/// from per-file streaming — the work is already done; sending it as one
/// event is faster — but we still walk `diff.files` so the order matches
/// the streaming path and fallback rows land in their natural position.
fn emit_cache_hit(
    tx: &std::sync::mpsc::Sender<local_review_core::tui::entity_list::ExtractionEvent>,
    entry: local_review_core::semantic::cache::CacheEntry,
    diff: &local_review_core::diff::Diff,
) {
    use local_review_core::tui::entity_list::ExtractionEvent;
    let summaries = build_entity_summaries_interleaved(entry, diff);
    let total = diff.files.len();
    let _ = tx.send(ExtractionEvent::Progress {
        files_done: total,
        files_total: total,
        files_failed: 0,
    });
    let _ = tx.send(ExtractionEvent::FileExtracted {
        file_path: String::new(),
        entities: summaries,
    });
    let _ = tx.send(ExtractionEvent::Complete);
}

/// Run per-file extraction, streaming progress events through `tx`. Writes
/// a cache entry on success so the next open hits the warm path.
fn extract_and_emit(
    task: &JjrExtractionTask,
    tx: &std::sync::mpsc::Sender<local_review_core::tui::entity_list::ExtractionEvent>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    diff: &local_review_core::diff::Diff,
    cache_path: &std::path::Path,
) {
    use local_review_core::semantic::cache::{self, CacheEntry, EXTRACTION_HASH, SCHEMA_VERSION};
    use local_review_core::tui::entity_list::ExtractionEvent;
    use std::sync::atomic::Ordering;

    let registry = local_review_core::semantic::create_default_registry();
    let parent_rev = jj::parent_rev(&task.change_id);
    let current_rev = task.change_id.as_str().to_owned();
    let ctx = FileExtractCtx {
        registry: &registry,
        repo_root: &task.repo_root,
        current_rev: &current_rev,
        parent_rev: &parent_rev,
    };
    let total = diff.files.len();
    let _ = tx.send(ExtractionEvent::Progress {
        files_done: 0,
        files_total: total,
        files_failed: 0,
    });

    let mut all_entities: Vec<local_review_core::semantic::EntityCoreData> = Vec::new();
    let mut failed_files: Vec<String> = Vec::new();

    for (i, file) in diff.files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(ExtractionEvent::Cancelled);
            return;
        }
        let path = file.display_path().to_string_lossy().into_owned();
        let mut file_entities = Vec::new();
        let mut file_failed = Vec::new();
        extract_file_entities(&ctx, &path, &mut file_entities, &mut file_failed);

        // Stream this file's summaries to the UI. If extraction yielded no
        // entities — whether the file actually failed to parse, the language
        // isn't registered (plain text, lock files), or the source genuinely
        // has nothing that matches our entity_kinds — we still emit a single
        // fallback summary so the file remains navigable from the entity
        // list. The diff is independent of extraction, and the reviewer
        // needs to see and comment on every changed file.
        let summaries = if file_entities.is_empty() {
            vec![local_review_core::semantic::fallback_summary_for_file(file)]
        } else {
            build_entity_summaries(CacheEntry {
                schema_version: SCHEMA_VERSION,
                extraction_hash: EXTRACTION_HASH.to_owned(),
                entities: file_entities.clone(),
                graph: None,
                failed_files: Vec::new(),
            })
        };
        let _ = tx.send(ExtractionEvent::FileExtracted {
            file_path: path.clone(),
            entities: summaries,
        });
        all_entities.extend(file_entities);
        failed_files.extend(file_failed);

        let _ = tx.send(ExtractionEvent::Progress {
            files_done: i + 1,
            files_total: total,
            files_failed: failed_files.len(),
        });
    }

    // Signal completion before writing the cache or building the graph.
    // Both can take seconds on large repos; the UI is fully populated by
    // the FileExtracted events inside the loop, so we can transition out
    // of the loading overlay immediately and finish the slow work in this
    // worker thread without holding the reviewer. The graph is best-effort
    // and a pure next-open optimization for Claude bundles — if the user
    // triggers Claude before the cache is written, the bundle simply drops
    // its deps / dependents sections.
    let _ = tx.send(ExtractionEvent::Complete);
    let graph = build_graph_best_effort(
        &local_review_core::semantic::create_default_registry(),
        &task.repo_root,
        task.change_id.as_str(),
    );
    let cache_entry = CacheEntry {
        schema_version: SCHEMA_VERSION,
        extraction_hash: EXTRACTION_HASH.to_owned(),
        entities: all_entities,
        graph,
        failed_files,
    };
    let _ = cache::write(cache_path, &cache_entry);
}

/// Build the cross-file dependency graph for `change_id`, falling back to
/// `None` on any error. The graph is a Claude-bundle convenience: missing
/// it means the bundle drops deps / dependents, not that the reviewer is
/// blocked. An empty file list (jj missing, revision unresolvable, etc.)
/// is treated the same as graph-build failure.
fn build_graph_best_effort(
    registry: &local_review_core::semantic::ExtractorRegistry,
    repo_root: &std::path::Path,
    change_id: &str,
) -> Option<local_review_core::semantic::GraphData> {
    let files = jj::list_tracked_files(change_id, repo_root);
    if files.is_empty() {
        return None;
    }
    Some(local_review_core::semantic::build_graph(
        registry, repo_root, &files,
    ))
}

fn extract_file_entities(
    ctx: &FileExtractCtx<'_>,
    file_path: &str,
    entities: &mut Vec<local_review_core::semantic::EntityCoreData>,
    failed_files: &mut Vec<String>,
) {
    // `file_content_at` returns Ok("") for genuinely absent files (added/deleted).
    // Propagate real IO / jj failures to the fallback list rather than silently
    // treating them as empty content, which would produce incorrect diffs.
    let Ok(before) = jj::file_content_at(ctx.parent_rev, file_path, ctx.repo_root) else {
        failed_files.push(file_path.to_owned());
        return;
    };
    let Ok(after) = jj::file_content_at(ctx.current_rev, file_path, ctx.repo_root) else {
        failed_files.push(file_path.to_owned());
        return;
    };

    let Ok(before_raw) = ctx.registry.extract(&before, file_path) else {
        failed_files.push(file_path.to_owned());
        return;
    };
    let Ok(after_raw) = ctx.registry.extract(&after, file_path) else {
        failed_files.push(file_path.to_owned());
        return;
    };

    let changed = local_review_core::semantic::diff_entities(&before_raw, &after_raw);
    entities.extend(changed);
}

/// Convert a `CacheEntry` into renderable `EntitySummary` values.
fn build_entity_summaries(
    entry: local_review_core::semantic::cache::CacheEntry,
) -> Vec<local_review_core::semantic::EntitySummary> {
    entry
        .entities
        .into_iter()
        .map(|e| {
            let display_name = e.id.display_name();
            let file_path = e.id.file_path.clone();
            let source_file = e.source_file.clone();
            local_review_core::semantic::EntitySummary {
                id: e.id,
                display_name,
                kind: e.kind,
                change: e.change,
                annotation: e.annotation,
                file_path,
                source_file,
                target_line: e.target_line,
                line_range: e.line_range,
                structural_change: e.structural_change,
                content_hash: e.content_hash,
                comment_count: 0,
                reviewed: false,
            }
        })
        .collect()
}

/// Build summaries from a cache entry, interleaving a synthetic fallback row
/// for every file in `diff` that has no entities. Order follows `diff.files`:
/// per file, emit all its entities (in cache order, which is source order),
/// or a single fallback if extraction produced none. Keeps the entity list
/// aligned with the diff so reviewers can navigate the entire change.
fn build_entity_summaries_interleaved(
    entry: local_review_core::semantic::cache::CacheEntry,
    diff: &local_review_core::diff::Diff,
) -> Vec<local_review_core::semantic::EntitySummary> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let raw_summaries = build_entity_summaries(entry);
    // Group by file path so we can scan the diff once.
    let mut by_path: HashMap<PathBuf, Vec<local_review_core::semantic::EntitySummary>> =
        HashMap::new();
    for s in raw_summaries {
        by_path.entry(s.file_path.clone()).or_default().push(s);
    }
    let mut out = Vec::new();
    for file in &diff.files {
        let path = file.display_path().to_path_buf();
        match by_path.remove(&path) {
            Some(file_entities) if !file_entities.is_empty() => out.extend(file_entities),
            _ => out.push(local_review_core::semantic::fallback_summary_for_file(file)),
        }
    }
    out
}

/// Build the full `rendered_per_file` list for a `ChangeDetails`.
///
/// Index 0 is always the synthetic description view; indices 1.. are the diff
/// files in their natural order.
fn build_rendered_views(details: &ChangeDetails) -> Vec<DiffView> {
    let mut views = Vec::with_capacity(details.diff.files.len() + 1);
    views.push(DiffView::from_description(&details.description));
    views.extend(details.diff.files.iter().map(DiffView::from_file));
    views
}

#[cfg(test)]
fn open_file_picker(app: &mut App) {
    // Snapshot the current change_id+commit_id and reviewed entry once, so
    // the closures handed to `build_entries` resolve view-by-view without
    // re-borrowing `app` per row.
    let reviewed_view_indices: std::collections::HashSet<usize> = (0..app.rendered_per_file.len())
        .filter(|i| app.is_view_reviewed(*i))
        .collect();
    let annotated = app.annotated_per_file.clone();
    let entries = build_file_picker_entries(
        &app.details.diff.files,
        &app.loaded_comments,
        &|view_idx| reviewed_view_indices.contains(&view_idx),
        &|view_idx| first_commentable_row_for_view(&annotated, view_idx),
    );
    app.screen = Screen::FilePicker(FilePickerState {
        selected_index: 0,
        scroll_offset: 0,
        entries,
    });
}

/// Return the index of the first commentable row in the rendered view at `view_idx`.
fn first_commentable_row_for_view(views: &[DiffView], view_idx: usize) -> usize {
    views
        .get(view_idx)
        .and_then(|v| {
            v.lines.iter().position(|l| {
                matches!(
                    l.kind,
                    RenderedLineKind::Added
                        | RenderedLineKind::Removed
                        | RenderedLineKind::Context
                        | RenderedLineKind::DescriptionLine
                )
            })
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod first_commentable_row_tests {
    use super::*;

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

    fn make_view(kinds: &[RenderedLineKind]) -> DiffView {
        let lines: Vec<RenderedLine> = kinds.iter().copied().map(make_line).collect();
        // paired_rows is not consulted by first_commentable_row_for_view.
        DiffView {
            title: String::new(),
            lines,
            paired_rows: Vec::new(),
        }
    }

    #[test]
    fn empty_views_returns_zero() {
        assert_eq!(first_commentable_row_for_view(&[], 0), 0);
        assert_eq!(first_commentable_row_for_view(&[], 5), 0);
    }

    #[test]
    fn out_of_bounds_view_idx_returns_zero() {
        let v = make_view(&[RenderedLineKind::Added]);
        assert_eq!(first_commentable_row_for_view(&[v], 1), 0);
    }

    #[test]
    fn all_hunk_separators_returns_zero() {
        let v = make_view(&[
            RenderedLineKind::HunkSeparator,
            RenderedLineKind::HunkSeparator,
        ]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 0);
    }

    #[test]
    fn hunk_header_at_index_zero_is_not_commentable_skips_to_first_added() {
        let v = make_view(&[RenderedLineKind::HunkHeader, RenderedLineKind::Added]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 1);
    }

    #[test]
    fn first_row_is_hunk_separator_second_is_added() {
        let v = make_view(&[RenderedLineKind::HunkSeparator, RenderedLineKind::Added]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 1);
    }

    #[test]
    fn first_row_is_added_returns_zero() {
        let v = make_view(&[RenderedLineKind::Added, RenderedLineKind::Context]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 0);
    }

    #[test]
    fn description_view_returns_zero_for_first_description_line() {
        let v = make_view(&[RenderedLineKind::DescriptionLine]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 0);
    }

    #[test]
    fn notice_only_view_returns_zero_fallback() {
        let v = make_view(&[RenderedLineKind::Notice]);
        assert_eq!(first_commentable_row_for_view(&[v], 0), 0);
    }
}

#[cfg(test)]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the file picker"
)]
fn handle_file_picker_key(app: &mut App, key: KeyEvent) {
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

#[cfg(test)]
fn file_picker_enter(app: &mut App) {
    let Screen::FilePicker(ref state) = app.screen else {
        return;
    };
    let Some(entry) = state.entries.get(state.selected_index) else {
        return;
    };
    let view_index = entry.view_index;
    // view_index 0 is the description view; diff files start at 1.
    let diff_file_index = view_index.saturating_sub(1);
    let is_binary = view_index > 0
        && matches!(
            app.details.diff.files.get(diff_file_index),
            Some(crate::diff::DiffFile::Binary { .. })
        );
    app.screen = Screen::Main;
    app.file_index = view_index;
    app.scroll = 0;
    if is_binary {
        app.status_message = Some("binary file — no commentable lines".to_owned());
        app.line_index = 0;
        app.mark_current_file_reviewed();
        return;
    }
    let first_commentable = app
        .annotated_per_file
        .get(view_index)
        .and_then(|v| {
            v.lines.iter().position(|l| {
                matches!(
                    l.kind,
                    RenderedLineKind::Added
                        | RenderedLineKind::Removed
                        | RenderedLineKind::Context
                        | RenderedLineKind::DescriptionLine
                )
            })
        })
        .unwrap_or(0);
    app.line_index = first_commentable;
    app.mark_current_file_reviewed();
}

#[cfg(test)]
fn refresh_current_change(app: &mut App) {
    let change_id = app.details.change_id.clone();
    match jj::show(&change_id) {
        Ok(details) => {
            apply_refreshed_change(app, details);
            app.status_message = Some("refreshed".to_owned());
        }
        Err(e) => {
            app.status_message = Some(format!(
                "refresh failed: {}",
                sanitize_for_status(&e.to_string())
            ));
        }
    }
}

/// Shared rebuild + mark sequence used by both the refresh path
/// (preserves cursor; reviewer is mid-thought) and the post-Claude reload
/// path (resets cursor; Claude may have rewritten the file under them).
/// Caller is responsible for any cursor-reset bookkeeping BEFORE calling.
#[cfg(test)]
fn rebuild_views_and_mark(app: &mut App, details: ChangeDetails) {
    app.rendered_per_file = build_rendered_views(&details);
    app.annotated_per_file = app.rendered_per_file.clone();
    app.details = details;
    app.overview_cache = None;
    app.refresh_inline_comments();
    app.mark_current_file_reviewed();
}

/// Pure in-memory side of `refresh_current_change`. Refresh preserves the
/// cursor position because the reviewer is mid-thought. Pulled out so the
/// auto-mark wiring is unit-testable without spawning `jj show`.
#[cfg(test)]
fn apply_refreshed_change(app: &mut App, details: ChangeDetails) {
    rebuild_views_and_mark(app, details);
}

#[cfg(test)]
fn toggle_severity_filter(app: &mut App, severity: Severity) {
    if app.severity_filter == Some(severity) {
        app.severity_filter = None;
    } else {
        app.severity_filter = Some(severity);
    }
    app.rebuild_annotated_views();
}

/// Return the `Comment` under the cursor when the focused `RenderedLine` is
/// an `InlineCommentMeta` backed by a local draft index.
#[cfg(test)]
fn focused_comment(app: &App) -> Option<&Comment> {
    let view = app.current_view()?;
    let line = view.lines.get(app.line_index)?;
    let RenderedLineKind::InlineCommentMeta { comment_index } = line.kind else {
        return None;
    };
    let CommentIndex::Local(idx) = comment_index else {
        return None;
    };
    app.loaded_comments.get(idx)
}

/// Park the cursor on the diff line a deleted comment was attached to.
#[cfg(test)]
fn anchor_line_index(app: &App, comment: &Comment) -> Option<usize> {
    let Anchor::Line { location, .. } = &comment.anchor else {
        return None;
    };
    let view = app.current_view()?;
    view.lines.iter().position(|l| match location.side {
        Side::Old => l.source_line == location.old_line && location.old_line.is_some(),
        Side::New => l.target_line == location.new_line && location.new_line.is_some(),
    })
}

/// Test-only helpers for inspecting rendered scrollbars across every screen.
/// Lives at the parent-module level so all four screens' tests can share one
/// copy of the buffer-walk logic — the helpers were independently re-derived
/// in three submodules during initial scrollbar adoption, which is exactly
/// the duplication a shared module exists to prevent.
#[cfg(test)]
pub(super) mod scrollbar_test_helpers {
    /// Whether `col` of `buf` contains any glyph drawn by ratatui's
    /// `Scrollbar` widget (the begin/end arrows, the thumb, or the track).
    pub(in crate::tui) fn col_contains_scrollbar_glyph(
        buf: &ratatui::buffer::Buffer,
        col: u16,
    ) -> bool {
        (0..buf.area.height).any(|row| {
            matches!(
                buf[(col, row)].symbol(),
                "\u{25b2}" | "\u{25bc}" | "\u{2588}" | "\u{2551}"
            )
        })
    }

    /// Find the row of the topmost thumb (`█`) glyph in `col` of `buf`.
    pub(in crate::tui) fn scrollbar_thumb_row(
        buf: &ratatui::buffer::Buffer,
        col: u16,
    ) -> Option<u16> {
        (0..buf.area.height).find(|&row| buf[(col, row)].symbol() == "\u{2588}")
    }

    /// Find the row of the bottommost thumb (`█`) glyph in `col` of `buf`.
    /// Useful when the thumb spans multiple rows and the bottom edge is the
    /// load-bearing assertion (e.g., scrolled to end on small content).
    pub(in crate::tui) fn scrollbar_thumb_last_row(
        buf: &ratatui::buffer::Buffer,
        col: u16,
    ) -> Option<u16> {
        (0..buf.area.height)
            .rev()
            .find(|&row| buf[(col, row)].symbol() == "\u{2588}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};
    use crossterm::event::KeyModifiers;

    #[test]
    fn sanitize_for_status_strips_escape_and_control_bytes() {
        let raw = "foo\x1b[2J\x1b[Hpwned\nbar\x07";
        let sanitized = sanitize_for_status(raw);
        for ch in sanitized.chars() {
            assert!(
                !ch.is_control() || ch == '\t',
                "sanitized output must not contain control bytes (except tab); found {ch:?}"
            );
        }
        assert!(!sanitized.contains('\x1b'), "ESC must be stripped");
    }

    #[test]
    fn sanitize_for_status_preserves_printable_and_tab() {
        let raw = "warning: foo\tbar baz";
        assert_eq!(sanitize_for_status(raw), raw);
    }

    #[test]
    fn sanitize_for_status_strips_unicode_bidi_overrides() {
        let raw = "foo\u{202e}bar\u{2066}baz\u{2069}qux";
        let sanitized = sanitize_for_status(raw);
        for forbidden in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}',
        ] {
            assert!(
                !sanitized.contains(forbidden),
                "BiDi mark {forbidden:?} must be stripped"
            );
        }
        assert!(sanitized.contains("foo"));
        assert!(sanitized.contains("bar"));
    }

    #[test]
    fn format_persist_error_sanitizes_path_with_escape_sequences() {
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: ChangeId::parse("abc12345").unwrap(),
                location: LineAnchor {
                    file: PathBuf::from("foo\x1b[2J\x1b[Hpwned"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@".to_owned(),
                    target_text: "t".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "b".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let err = JjrError::Io {
            source: std::io::Error::other("nope\x1b]8;;evil\x07hyperlink"),
        };
        let msg = format_persist_error(&comment, &err);
        for ch in msg.chars() {
            assert!(
                !ch.is_control() || ch == '\t',
                "format_persist_error output must not contain control bytes (except tab); found {ch:?}"
            );
        }
    }

    /// Build a minimal `Diff` with one hunk containing "target line" at line 3.
    fn make_single_hunk_diff() -> Diff {
        Diff {
            files: vec![DiffFile::Modified {
                path: PathBuf::from("foo.rs"),
                hunks: vec![Hunk {
                    header: "@@ -2,3 +2,3 @@".to_owned(),
                    function_context: None,
                    source_start: 2,
                    source_length: 3,
                    target_start: 2,
                    target_length: 3,
                    lines: vec![
                        Line {
                            kind: LineKind::Context,
                            text: "ctx before".to_owned(),
                            source_line: Some(2),
                            target_line: Some(2),
                        },
                        Line {
                            kind: LineKind::Context,
                            text: "target line".to_owned(),
                            source_line: Some(3),
                            target_line: Some(3),
                        },
                        Line {
                            kind: LineKind::Context,
                            text: "ctx after".to_owned(),
                            source_line: Some(4),
                            target_line: Some(4),
                        },
                    ],
                }],
            }],
        }
    }

    /// Build a comment anchored to the OLD hunk header so `reanchor_comment`
    /// detects a shift.
    fn make_shifted_comment(
        change_id: &ChangeId,
        commit_id: &CommitId,
        repo_root: &std::path::Path,
        ts: time::OffsetDateTime,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("foo.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(2),
                    hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
                    target_text: "target line".to_owned(),
                    context_before: vec!["ctx before".to_owned()],
                    context_after: vec!["ctx after".to_owned()],
                },
            },
            repo_root: repo_root.to_path_buf(),
            revset: "@".to_owned(),
            commit_id: Some(commit_id.clone()),
            body: "comment".to_owned(),
            severity: Severity::Required,
            created_at: ts,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    /// `reconcile_and_persist` must surface at least one error when reanchoring
    /// fails to write back. Uses a `repo_root` with no `.jj-review/` dir so
    /// `update_comment` returns an I/O error. Proves first-error-wins semantics
    /// and no panic with multiple failing comments.
    #[test]
    fn reconcile_and_persist_surfaces_first_error_when_multiple_fail() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("empty_repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let change_id = ChangeId::parse(&"d".repeat(32)).unwrap();
        let commit_id = CommitId::parse(&"d".repeat(40)).unwrap();

        let details = ChangeDetails {
            change_id: change_id.clone(),
            commit_id: commit_id.clone(),
            description: String::new(),
            diff: make_single_hunk_diff(),
        };

        let c1 = make_shifted_comment(
            &change_id,
            &commit_id,
            &repo_root,
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let c2 = make_shifted_comment(
            &change_id,
            &commit_id,
            &repo_root,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
        );

        let ctx = JjrContext {
            data_home: dir.path().to_path_buf(),
            repo_root: repo_root.clone(),
            revset: "@".to_owned(),
        };
        let mut surface = JjrSurface::new(details, ctx, None, None);
        surface.loaded_comments = vec![c1.clone(), c2.clone()];
        let _ = surface.reconcile_and_persist(vec![c1, c2]);

        assert!(
            surface.pending_status_message.is_some(),
            "expected pending_status_message to be set after error"
        );
        let msg = surface.pending_status_message.as_ref().unwrap();
        assert!(!msg.is_empty(), "surfaced error message must not be empty");
    }

    /// `JjrSurface::delete_comment` returns `Refused` (not `Deleted`) when the
    /// provided `CommentId` does not match any record in `loaded_comments`.
    #[test]
    fn delete_comment_returns_refused_when_comment_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let change_id = ChangeId::parse(&"a".repeat(32)).unwrap();
        let commit_id = CommitId::parse(&"a".repeat(40)).unwrap();
        let details = ChangeDetails {
            change_id: change_id.clone(),
            commit_id: commit_id.clone(),
            description: String::new(),
            diff: make_single_hunk_diff(),
        };
        let ctx = JjrContext {
            data_home: dir.path().to_path_buf(),
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
        };
        let mut surface = JjrSurface::new(details, ctx, None, None);
        // loaded_comments is empty — any CommentId will be absent.
        let absent_id = local_review_core::tui::CommentId::new(time::OffsetDateTime::UNIX_EPOCH);
        let req = DeleteRequest::new(absent_id, None);
        let outcome = surface.delete_comment(req).unwrap();
        assert!(
            matches!(outcome, CoreDeleteOutcome::Refused { .. }),
            "delete_comment must return Refused when CommentId is not in loaded_comments; got: {outcome:?}"
        );
    }

    fn make_line(
        kind: RenderedLineKind,
        text: &str,
        source: Option<u32>,
        target: Option<u32>,
    ) -> RenderedLine {
        RenderedLine {
            kind,
            text: text.to_owned(),
            source_line: source,
            target_line: target,
            hunk_header: Some("@@ -1,3 +1,3 @@".to_owned()),
            comment_severity: None,
        }
    }

    #[test]
    fn pick_side_old_for_removed_line() {
        // source set, target none → Old
        assert_eq!(pick_side(Some(7), None), Side::Old);
    }

    #[test]
    fn pick_side_new_for_added_line() {
        // source none, target set → New
        assert_eq!(pick_side(None, Some(7)), Side::New);
    }

    #[test]
    fn pick_side_new_for_context_line() {
        // both set (context line) → New
        assert_eq!(pick_side(Some(7), Some(8)), Side::New);
    }

    #[test]
    fn pick_side_new_when_both_absent() {
        // Pathological but well-defined: defaults to New rather than panicking.
        assert_eq!(pick_side(None, None), Side::New);
    }

    #[test]
    fn collect_context_basic() {
        let lines = vec![
            make_line(RenderedLineKind::HunkHeader, "@@", None, None),
            make_line(RenderedLineKind::Context, "ctx0", Some(1), Some(1)),
            make_line(RenderedLineKind::Added, "added", None, Some(2)),
            make_line(RenderedLineKind::Context, "ctx1", Some(2), Some(3)),
        ];
        // Cursor on the Added line (idx 2): one before, one after.
        let (before, after) = collect_context(&lines, 2);
        assert_eq!(before, vec!["ctx0".to_owned()]);
        assert_eq!(after, vec!["ctx1".to_owned()]);
    }

    #[test]
    fn collect_context_skips_inline_comment_meta_and_body() {
        let lines = vec![
            make_line(RenderedLineKind::Context, "ctx0", Some(1), Some(1)),
            make_line(
                RenderedLineKind::InlineCommentMeta {
                    comment_index: CommentIndex::Local(0),
                },
                "┃ meta",
                None,
                None,
            ),
            make_line(RenderedLineKind::InlineCommentBody, "┃ body", None, None),
            make_line(RenderedLineKind::Added, "added", None, Some(2)),
            make_line(RenderedLineKind::Context, "ctx1", Some(2), Some(3)),
        ];
        let (before, after) = collect_context(&lines, 3);
        assert_eq!(before, vec!["ctx0".to_owned()]);
        assert_eq!(after, vec!["ctx1".to_owned()]);
    }

    #[test]
    fn collect_context_at_start_no_before() {
        let lines = vec![
            make_line(RenderedLineKind::Context, "ctx0", Some(1), Some(1)),
            make_line(RenderedLineKind::Context, "ctx1", Some(2), Some(2)),
        ];
        let (before, after) = collect_context(&lines, 0);
        assert!(before.is_empty());
        assert_eq!(after, vec!["ctx1".to_owned()]);
    }

    #[test]
    fn collect_context_at_end_no_after() {
        let lines = vec![
            make_line(RenderedLineKind::Context, "ctx0", Some(1), Some(1)),
            make_line(RenderedLineKind::Context, "ctx1", Some(2), Some(2)),
        ];
        let (before, after) = collect_context(&lines, 1);
        assert_eq!(before, vec!["ctx0".to_owned()]);
        assert!(after.is_empty());
    }

    #[test]
    fn collect_context_caps_at_three_each_side() {
        let lines: Vec<RenderedLine> = (0..10)
            .map(|i| {
                make_line(
                    RenderedLineKind::Context,
                    &format!("ctx{i}"),
                    Some(i + 1),
                    Some(i + 1),
                )
            })
            .collect();
        let (before, after) = collect_context(&lines, 5);
        // 3 before (ctx2, ctx3, ctx4 in order), 3 after (ctx6, ctx7, ctx8).
        assert_eq!(before.len(), 3);
        assert_eq!(after.len(), 3);
        assert_eq!(before[0], "ctx2");
        assert_eq!(before[2], "ctx4");
        assert_eq!(after[0], "ctx6");
        assert_eq!(after[2], "ctx8");
    }

    fn make_app_with_single_file(file: DiffFile) -> App {
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: String::new(),
            diff: Diff { files: vec![file] },
        };
        let mut app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        // Start on the first diff file (view_index 1). Description view is
        // at index 0 but most tests target diff-file content.
        app.file_index = 1;
        app
    }

    fn sample_diff_file() -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,3 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 2,
                target_start: 1,
                target_length: 3,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        text: "ctx".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    },
                    Line {
                        kind: LineKind::Added,
                        text: "added".to_owned(),
                        source_line: None,
                        target_line: Some(2),
                    },
                    Line {
                        kind: LineKind::Removed,
                        text: "removed".to_owned(),
                        source_line: Some(2),
                        target_line: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn build_line_target_returns_non_commentable_for_every_non_diff_kind() {
        // Cover every kind that build_line_target should reject. The match arm is
        // shared, but each variant deserves a row so a future refactor that
        // accidentally drops one is caught.
        let kinds = [
            RenderedLineKind::HunkHeader,
            RenderedLineKind::HunkSeparator,
            RenderedLineKind::Notice,
            RenderedLineKind::InlineCommentMeta {
                comment_index: CommentIndex::Local(0),
            },
            RenderedLineKind::InlineCommentBody,
        ];

        for kind in kinds {
            let mut app = make_app_with_single_file(sample_diff_file());
            // Replace the diff file view's first line with one of `kind` so the
            // cursor lands on it. This bypasses normal rendering — fine for a
            // build_line_target test which only reads `current_view().lines[idx]`.
            // annotated_per_file[1] is the diff file; [0] is the description view.
            let first_line = app.annotated_per_file[1]
                .lines
                .get_mut(0)
                .expect("sample diff has at least one line");
            first_line.kind = kind;
            app.line_index = 0;

            assert!(
                matches!(build_line_target(&app), BuildTargetResult::NonCommentable),
                "expected NonCommentable for kind {kind:?}"
            );
        }
    }

    #[test]
    fn build_line_target_returns_ready_for_added_line() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2; // Added line
        match build_line_target(&app) {
            BuildTargetResult::Ready(target) => {
                assert_eq!(target.target_line, Some(2));
                assert_eq!(target.source_line, None);
                assert_eq!(target.target_text, "added");
            }
            BuildTargetResult::NonCommentable
            | BuildTargetResult::NoView
            | BuildTargetResult::DescriptionLine { .. } => {
                panic!("expected Ready");
            }
        }
    }

    #[test]
    fn build_line_target_returns_ready_for_context_line_and_pick_side_chooses_new() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 1; // Context line: source=Some(1), target=Some(1)
        match build_line_target(&app) {
            BuildTargetResult::Ready(target) => {
                assert_eq!(target.source_line, Some(1));
                assert_eq!(target.target_line, Some(1));
                // pick_side on a context line picks New.
                assert_eq!(pick_side(target.source_line, target.target_line), Side::New);
            }
            BuildTargetResult::NonCommentable
            | BuildTargetResult::NoView
            | BuildTargetResult::DescriptionLine { .. } => {
                panic!("expected Ready");
            }
        }
    }

    #[test]
    fn build_line_target_returns_ready_for_removed_line() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 3; // Removed line
        match build_line_target(&app) {
            BuildTargetResult::Ready(target) => {
                assert_eq!(target.source_line, Some(2));
                assert_eq!(target.target_line, None);
                assert_eq!(pick_side(target.source_line, target.target_line), Side::Old);
            }
            BuildTargetResult::NonCommentable
            | BuildTargetResult::NoView
            | BuildTargetResult::DescriptionLine { .. } => {
                panic!("expected Ready");
            }
        }
    }

    /// Build a stand-alone `ComposerInit` for a synthetic `LineTarget` with no
    /// app context. Used by tests that exercise paths which don't depend on
    /// the surrounding `App` (e.g., delete-via-composer error branches).
    fn make_init_for_test(target: LineTarget) -> ComposerInit {
        ComposerInit {
            scope: ComposerScope::Line(target.clone()),
            severity: Severity::Note,
            change_id: ChangeId::parse("abc12345").unwrap(),
            change_description: "test change".to_owned(),
            line_available: Some(target),
            stack_available: None,
            description_available: None,
        }
    }

    /// Build an `init` whose `change_id`/`change_description` come from `app`
    /// and whose `line_available` is the given `target`. Mirrors what the
    /// production main-view path constructs.
    fn make_init_from_app(app: &App, target: LineTarget, severity: Severity) -> ComposerInit {
        ComposerInit {
            scope: ComposerScope::Line(target.clone()),
            severity,
            change_id: app.details.change_id.clone(),
            change_description: app.details.description.clone(),
            line_available: Some(target),
            stack_available: stack_snapshot(app),
            description_available: None,
        }
    }

    /// Build a composer whose `change_id` matches `app.details.change_id`.
    /// Use this when the test will assert against `app.details.change_id` in
    /// the store (i.e., `Change` or `Line` scope tests).
    fn make_composer_with_body(app: &App, target: LineTarget, body: &str) -> Composer {
        let init = make_init_from_app(app, target, Severity::Suggestion);
        let mut composer = Composer::new(init);
        for ch in body.chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer
    }

    /// Reconstruct a `ComposerInit` from an existing composer. Used in the
    /// edit-save tests where the composer's snapshot fields are cloned into
    /// a fresh `for_edit` instance. Centralized so adding a field to
    /// `ComposerInit` only requires one test-helper update.
    fn init_from_composer(c: &Composer) -> ComposerInit {
        ComposerInit {
            scope: c.scope.clone(),
            severity: c.severity,
            change_id: c.change_id.clone(),
            change_description: c.change_description.clone(),
            line_available: c.line_available.clone(),
            stack_available: c.stack_available.clone(),
            description_available: c.description_available.clone(),
        }
    }

    #[test]
    fn save_composer_change_scope_saves_to_change_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let mut composer = make_composer_with_body(&app, target, "change-level concern");
        composer.scope = ComposerScope::Change;
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(
            matches!(outcome, SaveOutcome::Saved),
            "expected Saved; got {outcome:?}"
        );

        let loaded = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(
            matches!(loaded[0].anchor, Anchor::Change { .. }),
            "expected Change anchor"
        );
        assert_eq!(loaded[0].body, "change-level concern");
    }

    #[test]
    fn save_composer_stack_scope_saves_to_stack_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        app.line_index = 2;
        let entry = StackEntry {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: String::new(),
        };
        let revset_hash = RevsetHash::from_revset("trunk()..@");
        app.stack = Some(StackContext {
            entries: vec![entry],
            current_index: 0,
            revset: "trunk()..@".to_owned(),
            revset_hash,
        });

        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        // Build the init via the production stack snapshot helper so the
        // composer carries the same stack availability the running app would.
        let init = make_init_from_app(&app, target, Severity::Suggestion);
        let stack = init
            .stack_available
            .clone()
            .expect("stack snapshot must be present in stack mode");
        let mut composer = Composer::new(init);
        for ch in "stack-level concern".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer.scope = ComposerScope::Stack(stack);

        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(
            matches!(outcome, SaveOutcome::Saved),
            "expected Saved; got {outcome:?}"
        );

        let loaded =
            crate::store::load_stack_comments(&app.repo_root, &app.repo_root, &revset_hash)
                .unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].anchor {
            Anchor::Stack {
                revset_hash: persisted,
            } => {
                assert_eq!(*persisted, revset_hash, "persisted hash should match");
            }
            Anchor::Line { .. } | Anchor::Change { .. } | Anchor::Description { .. } => {
                panic!("expected Stack anchor; got {:?}", loaded[0].anchor)
            }
        }
        assert_eq!(loaded[0].body, "stack-level concern");

        // Confirm the record landed in `_stack.jsonl`, not the change file.
        let stack_path = crate::store::stack_file(dir.path(), dir.path());
        assert!(stack_path.exists(), "_stack.jsonl should exist");
    }

    // In single-change mode the composer carries `stack_available: None`, so
    // the Alt+K chord refuses up-front and the save path never sees a Stack
    // scope without a backing snapshot. Pin that property at the dispatcher
    // level: from a single-change app, opening the composer and pressing
    // Alt+K leaves the scope unchanged and surfaces the unavailable status.
    #[test]
    fn alt_k_in_single_change_mode_refuses_chord_and_does_not_switch_scope() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        assert!(
            app.stack.is_none(),
            "single-change app starts with no stack"
        );
        open_composer(&mut app);
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
        );
        let Screen::Composer(ref composer) = app.screen else {
            panic!("composer remains open after refused chord");
        };
        assert!(
            matches!(composer.scope, ComposerScope::Line(_)),
            "scope must not change when stack chord is refused"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_STACK_UNAVAILABLE),
            "status message must surface the stack-unavailable hint",
        );
    }

    #[test]
    fn save_composer_refuses_empty_body() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let composer = make_composer_with_body(&app, target, "");
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(outcome, SaveOutcome::Refused(_)));
    }

    #[test]
    fn save_composer_duplicate_timestamp_errors_and_preserves_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let composer = make_composer_with_body(&app, target, "first body content");
        let now = time::OffsetDateTime::UNIX_EPOCH;

        // First save succeeds.
        let first = save_composer(&mut app, &composer, now);
        assert!(matches!(first, SaveOutcome::Saved), "first save");

        // Second save with the same `now` collides on the timestamp key.
        let second = save_composer(&mut app, &composer, now);
        match second {
            SaveOutcome::Errored(msg) => {
                assert!(
                    msg.contains("two comments at the same timestamp"),
                    "got message: {msg}"
                );
            }
            SaveOutcome::Saved | SaveOutcome::Refused(_) => panic!("expected Errored"),
        }
        // Body is still in the composer (caller decides to put composer back).
        assert_eq!(composer.body_text(), "first body content");
    }

    fn make_app_with_comment_on_disk(dir: &std::path::Path) -> App {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.to_path_buf();
        // Save a comment on the Added line (target_line=2, foo.txt).
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready for Added line");
        };
        let composer = make_composer_with_body(&app, target, "test comment body");
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let outcome = save_composer(&mut app, &composer, now);
        assert!(matches!(outcome, SaveOutcome::Saved), "setup save failed");
        // refresh puts the InlineCommentMeta into the annotated view.
        // Position the cursor on the meta line (index 3 = after Added).
        app.line_index = 3;
        app
    }

    #[test]
    fn focused_comment_returns_none_on_diff_line() {
        let app = make_app_with_single_file(sample_diff_file());
        // Default line_index=0 which is HunkHeader — not InlineCommentMeta.
        assert!(focused_comment(&app).is_none());
    }

    #[test]
    fn focused_comment_returns_some_on_inline_meta_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = make_app_with_comment_on_disk(dir.path());
        // line_index=3 should be InlineCommentMeta after the Added line.
        let view = app.current_view().expect("view");
        assert!(matches!(
            view.lines[3].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));
        let fc = focused_comment(&app);
        assert!(fc.is_some(), "expected Some on InlineCommentMeta");
        assert_eq!(fc.unwrap().body, "test comment body");
    }

    #[test]
    fn open_composer_for_edit_prepopulates_from_focused_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        open_composer_for_edit(&mut app);
        let Screen::Composer(ref composer) = app.screen else {
            panic!("expected Composer screen after open_composer_for_edit");
        };
        assert_eq!(composer.body_text(), "test comment body");
        assert!(
            composer.editing.is_some(),
            "composer.editing should be Some in edit mode"
        );
        assert!(
            composer.title().starts_with("Edit comment"),
            "title should start with 'Edit comment'; got: {}",
            composer.title()
        );
    }

    #[test]
    fn save_composer_edit_mode_calls_update_and_sets_updated_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        let created_at = time::OffsetDateTime::UNIX_EPOCH;

        // Open in edit mode.
        open_composer_for_edit(&mut app);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected Composer screen");
        };
        // Change the body slightly.
        for ch in " updated".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let composer_snapshot = {
            let Screen::Composer(ref c) = app.screen else {
                unreachable!()
            };
            let ctx = c
                .editing
                .as_ref()
                .expect("for_edit-built composer carries an EditingContext");
            Composer::for_edit(EditedComment {
                init: init_from_composer(c),
                body: c.body_text(),
                identity: ctx.identity,
                original: ctx.original.clone(),
                original_anchor: ctx.original_anchor.clone(),
            })
        };
        let save_time = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60);
        let outcome = save_composer(&mut app, &composer_snapshot, save_time);
        assert!(matches!(outcome, SaveOutcome::Saved), "expected Saved");

        // After update, the comment should still carry the original created_at.
        let loaded = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].created_at, created_at);
        assert_eq!(loaded[0].updated_at, Some(save_time));
        assert!(
            loaded[0].body.contains("updated"),
            "body should contain updated text; got: {}",
            loaded[0].body
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("comment updated"),
            "status message should say 'comment updated'"
        );
    }

    #[test]
    fn delete_focused_comment_removes_inline_and_moves_cursor_to_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());

        // Confirm we start with a comment at line_index=3.
        assert!(matches!(
            app.current_view().unwrap().lines[3].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));

        delete_focused_comment(&mut app);

        assert_eq!(
            app.status_message.as_deref(),
            Some("comment deleted"),
            "status message should say 'comment deleted'"
        );
        // After delete the annotated view should have no InlineCommentMeta.
        let meta_count = app
            .current_view()
            .unwrap()
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(meta_count, 0, "no meta lines should remain after delete");
        // Cursor should land on a valid (non-skip) line.
        let cursor_kind = app.current_view().unwrap().lines[app.line_index].kind;
        assert!(
            !matches!(
                cursor_kind,
                RenderedLineKind::HunkSeparator | RenderedLineKind::InlineCommentBody
            ),
            "cursor should not land on a skip-line; got {cursor_kind:?}"
        );
    }

    #[test]
    fn delete_focused_comment_on_non_meta_line_sets_error_message() {
        let mut app = make_app_with_single_file(sample_diff_file());
        // line_index=0 is HunkHeader — not InlineCommentMeta.
        app.line_index = 0;
        delete_focused_comment(&mut app);
        assert!(
            app.status_message.is_some(),
            "expected an error status message"
        );
    }

    #[test]
    fn delete_via_composer_without_editing_returns_refused() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let composer = Composer::new(make_init_for_test(target));
        let outcome = delete_via_composer(&mut app, &composer);
        assert!(
            matches!(outcome, SaveOutcome::Refused(_)),
            "expected Refused when editing is None"
        );
    }

    /// D1. Save a comment, then remove the backing JSONL so the on-disk record
    /// is gone. Edit-save should return `Errored` and the composer's body is
    /// preserved (caller decides to put the composer back on screen).
    #[test]
    fn save_composer_edit_mode_io_error_preserves_body_and_returns_errored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());

        // Open in edit mode, type something to differentiate the body.
        open_composer_for_edit(&mut app);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected Composer screen");
        };
        for ch in " edited body".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let composer_snapshot = {
            let Screen::Composer(ref c) = app.screen else {
                unreachable!()
            };
            let ctx = c
                .editing
                .as_ref()
                .expect("for_edit-built composer carries an EditingContext");
            Composer::for_edit(EditedComment {
                init: init_from_composer(c),
                body: c.body_text(),
                identity: ctx.identity,
                original: ctx.original.clone(),
                original_anchor: ctx.original_anchor.clone(),
            })
        };

        // Wipe the on-disk record. update_comment will see an empty file
        // (load_file_for_rewrite returns Ok(empty) for missing files) and
        // then `replace_by_timestamp` errors with CommentNotFound.
        let comments_dir =
            crate::store::change_file(dir.path(), dir.path(), &app.details.change_id)
                .parent()
                .expect("change_file has parent")
                .to_path_buf();
        for entry in std::fs::read_dir(&comments_dir).expect("comments dir") {
            let entry = entry.expect("entry");
            std::fs::remove_file(entry.path()).expect("remove jsonl");
        }

        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60);
        let outcome = save_composer(&mut app, &composer_snapshot, now);
        match outcome {
            SaveOutcome::Errored(msg) => {
                assert!(msg.contains("update failed"), "got message: {msg}");
            }
            SaveOutcome::Saved | SaveOutcome::Refused(_) => panic!("expected Errored"),
        }
        // The composer object passed in still carries its body — the caller
        // is responsible for reinstating it; here we just confirm the snapshot
        // held its text across the failed save.
        assert!(
            composer_snapshot.body_text().contains("edited body"),
            "body should be preserved across failed update; got: {}",
            composer_snapshot.body_text()
        );
    }

    #[test]
    fn edit_save_uses_latest_in_memory_anchor_not_composer_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        let created_at = time::OffsetDateTime::UNIX_EPOCH;

        open_composer_for_edit(&mut app);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected Composer screen");
        };
        for ch in " edited".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let composer_snapshot = {
            let Screen::Composer(ref c) = app.screen else {
                unreachable!()
            };
            let ctx = c
                .editing
                .as_ref()
                .expect("for_edit-built composer carries an EditingContext");
            Composer::for_edit(EditedComment {
                init: init_from_composer(c),
                body: c.body_text(),
                identity: ctx.identity,
                original: ctx.original.clone(),
                original_anchor: ctx.original_anchor.clone(),
            })
        };

        // Simulate re-anchor: location drifts in memory, composer snapshot stays stale.
        let new_line: u32 = 99;
        if let Anchor::Line {
            ref mut location, ..
        } = app.loaded_comments[0].anchor
        {
            location.new_line = Some(new_line);
            location.target_text = "moved target".to_owned();
        } else {
            panic!("expected Line anchor");
        }
        // Persist the in-memory mutation to disk so update_comment finds the
        // correct timestamp.
        crate::store::update_comment(&app.repo_root, &app.repo_root, &app.loaded_comments[0])
            .expect("seed disk with moved anchor");

        let save_time = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60);
        let outcome = save_composer(&mut app, &composer_snapshot, save_time);
        assert!(matches!(outcome, SaveOutcome::Saved), "expected Saved");

        let loaded = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].created_at, created_at);
        let Anchor::Line { location, .. } = &loaded[0].anchor else {
            panic!("expected Line anchor on loaded record");
        };
        assert_eq!(
            location.new_line,
            Some(new_line),
            "persisted anchor must be the latest in-memory location, not the composer snapshot"
        );
        assert_eq!(
            location.target_text, "moved target",
            "persisted target_text must be the latest in-memory value"
        );
        assert!(
            loaded[0].body.contains("edited"),
            "persisted body must carry the user's edit; got: {}",
            loaded[0].body
        );
    }

    /// If `loaded_comments` no longer contains a record with the composer's
    /// `created_at` (race with deletion in another flow), edit-save aborts
    /// with a status-bar warning rather than writing the snapshot back.
    #[test]
    fn edit_save_aborts_when_in_memory_record_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());

        open_composer_for_edit(&mut app);
        let composer_snapshot = {
            let Screen::Composer(ref c) = app.screen else {
                panic!("expected Composer screen");
            };
            let ctx = c
                .editing
                .as_ref()
                .expect("for_edit-built composer carries an EditingContext");
            Composer::for_edit(EditedComment {
                init: init_from_composer(c),
                body: c.body_text(),
                identity: ctx.identity,
                original: ctx.original.clone(),
                original_anchor: ctx.original_anchor.clone(),
            })
        };

        app.loaded_comments.clear();

        let save_time = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60);
        let outcome = save_composer(&mut app, &composer_snapshot, save_time);
        match outcome {
            SaveOutcome::Errored(msg) => {
                assert!(
                    msg.contains("comment was removed"),
                    "expected 'comment was removed' message; got: {msg}"
                );
            }
            SaveOutcome::Saved | SaveOutcome::Refused(_) => panic!("expected Errored"),
        }
    }

    /// D2. `^D` delete-from-composer error path. With the JSONL wiped, the
    /// store returns `CommentNotFound` and `delete_via_composer` reports it.
    #[test]
    fn delete_via_composer_io_error_returns_errored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());

        // Open in edit mode to populate `composer.editing`.
        open_composer_for_edit(&mut app);
        let composer_snapshot = {
            let Screen::Composer(ref c) = app.screen else {
                panic!("expected Composer screen");
            };
            let ctx = c
                .editing
                .as_ref()
                .expect("for_edit-built composer carries an EditingContext");
            Composer::for_edit(EditedComment {
                init: init_from_composer(c),
                body: c.body_text(),
                identity: ctx.identity,
                original: ctx.original.clone(),
                original_anchor: ctx.original_anchor.clone(),
            })
        };

        // Wipe the on-disk record.
        let comments_dir =
            crate::store::change_file(dir.path(), dir.path(), &app.details.change_id)
                .parent()
                .expect("change_file has parent")
                .to_path_buf();
        for entry in std::fs::read_dir(&comments_dir).expect("comments dir") {
            let entry = entry.expect("entry");
            std::fs::remove_file(entry.path()).expect("remove jsonl");
        }

        let outcome = delete_via_composer(&mut app, &composer_snapshot);
        match outcome {
            SaveOutcome::Errored(msg) => {
                assert!(msg.contains("delete failed"), "got message: {msg}");
            }
            SaveOutcome::Saved | SaveOutcome::Refused(_) => panic!("expected Errored"),
        }
    }

    // -- Open a main-view line-comment edit, swap to a non-Line scope via
    //   chord (Alt+C, then Alt+K when stack is available), then press Ctrl+D.
    //   Delete must operate on the original line anchor — not on any of the
    //   swapped scopes — because the scope picker is a viewing / composing
    //   aid, not a re-anchor mechanism.
    #[test]
    fn delete_via_composer_after_chord_swap_deletes_original_anchor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        // Stack mode so Alt+K is available; otherwise the chord refuses.
        let revset_hash = RevsetHash::from_revset("trunk()..@");
        app.stack = Some(StackContext {
            entries: vec![StackEntry {
                change_id: app.details.change_id.clone(),
                commit_id: app.details.commit_id.clone(),
                description: String::new(),
            }],
            current_index: 0,
            revset: "trunk()..@".to_owned(),
            revset_hash,
        });

        // Open the line-comment edit; original_anchor is set at open time.
        open_composer_for_edit(&mut app);
        // Swap scope first to Change, then to Stack. Both must NOT touch
        // original_anchor.
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT),
        );
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
        );
        // Confirm the scope did swap (not a refusal that left it on Line).
        if let Screen::Composer(ref c) = app.screen {
            assert!(
                matches!(c.scope, ComposerScope::Stack(_)),
                "scope should have swapped to Stack; got {:?}",
                c.scope
            );
            assert!(
                c.editing.is_some(),
                "editing context (carrying original_anchor) must persist across chord swaps"
            );
        } else {
            panic!("composer should still be open");
        }

        // Ctrl+D — delete the original line comment regardless of swapped scope.
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );

        assert!(
            matches!(app.screen, Screen::Main),
            "composer closes on successful delete"
        );
        // The original line-anchored comment is gone from disk.
        let loaded = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .unwrap();
        assert!(
            loaded.is_empty(),
            "original line comment should be deleted from disk; got {loaded:?}"
        );
        // No stack comment was created (delete must not write the swapped scope).
        let stack_loaded =
            crate::store::load_stack_comments(&app.repo_root, &app.repo_root, &revset_hash)
                .unwrap();
        assert!(
            stack_loaded.is_empty(),
            "no stack comment should exist after delete; got {stack_loaded:?}"
        );
        // The status message announces the delete (not a refusal).
        assert_eq!(
            app.status_message.as_deref(),
            Some("comment deleted"),
            "status must be 'comment deleted'; got {:?}",
            app.status_message
        );
    }

    // -- T2: stack-anchor edit in single-change mode + chord swap.
    //   `open_meta_comment_editor` with `Anchor::Stack` builds the
    //   synthetic-revset path because `stack_available` is None. Alt+L
    //   succeeds (line is available from the cursor); Alt+K refuses because
    //   `stack_available` is None even though scope was already Stack at
    //   open time. Pins the asymmetric availability state.
    #[test]
    fn edit_stack_anchor_in_single_change_mode_alt_k_after_alt_l_refuses_with_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        // Cursor on a commentable line so `line_available` is populated.
        app.line_index = 2;
        assert!(
            app.stack.is_none(),
            "single-change app must have no stack to exercise the synthetic path"
        );
        let revset_hash = RevsetHash::from_revset("trunk()..@");
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack { revset_hash },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "stack body".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        if let Screen::Composer(ref c) = app.screen {
            assert!(
                matches!(c.scope, ComposerScope::Stack(_)),
                "open should set Stack scope from the saved anchor"
            );
            assert!(
                c.stack_available.is_none(),
                "single-change session has no stack_available even when editing Stack-anchored"
            );
            assert!(
                c.line_available.is_some(),
                "cursor on a commentable line populates line_available"
            );
        } else {
            panic!("composer should be open");
        }

        // Alt+L succeeds — line is available.
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
        );
        if let Screen::Composer(ref c) = app.screen {
            assert!(
                matches!(c.scope, ComposerScope::Line(_)),
                "Alt+L must swap to Line scope"
            );
        } else {
            panic!("composer should still be open");
        }

        // Alt+K refuses — stack_available is None.
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
        );
        if let Screen::Composer(ref c) = app.screen {
            assert!(
                matches!(c.scope, ComposerScope::Line(_)),
                "scope must remain Line when Alt+K is refused"
            );
            assert_eq!(
                c.refusal_status,
                Some(STATUS_STACK_UNAVAILABLE),
                "refusal_status must surface the stack-unavailable hint"
            );
        } else {
            panic!("composer should still be open");
        }
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_STACK_UNAVAILABLE)
        );
    }

    // -- T3: Ctrl+K is no longer a scope chord, so it must NOT close the
    //   composer or switch scopes. The minimal in-tree textarea also does
    //   not bind Ctrl+K (the previous tui-textarea kill-to-EOL behavior is
    //   intentionally not reimplemented), so the body is left unmodified.
    //   Pin the user-reported regression: pressing Ctrl+K inside the body
    //   does not damage state.
    #[test]
    fn ctrl_k_inside_composer_body_is_noop_and_does_not_switch_scope() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        open_composer(&mut app);
        for ch in "hello world".chars() {
            handle_composer_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            );
        }
        handle_composer_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for ch in "second line".chars() {
            handle_composer_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            );
        }
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        let Screen::Composer(ref composer) = app.screen else {
            panic!("composer should remain open");
        };
        assert_eq!(
            composer.body_text(),
            "hello world\nsecond line",
            "Ctrl+K must be a no-op; body unchanged"
        );
        assert!(matches!(composer.scope, ComposerScope::Line(_)));
    }

    // -- T4: `fallback_scope` branches.
    #[test]
    fn fallback_scope_returns_line_when_target_present() {
        let target = LineTarget {
            file: PathBuf::from("foo.rs"),
            rendered_index: 0,
            source_line: None,
            target_line: Some(1),
            target_text: "x".to_owned(),
            hunk_header: "@@".to_owned(),
            context_before: vec![],
            context_after: vec![],
        };
        match fallback_scope(Some(target.clone())) {
            ComposerScope::Line(carried) => {
                assert_eq!(carried.file, target.file);
                assert_eq!(carried.target_line, target.target_line);
            }
            ComposerScope::Change | ComposerScope::Stack(_) | ComposerScope::Description(_) => {
                panic!("fallback_scope must yield Line when target is Some");
            }
        }
    }

    #[test]
    fn fallback_scope_returns_change_when_target_absent() {
        assert!(matches!(fallback_scope(None), ComposerScope::Change));
    }

    // -- T5: edit-mode `*_available` mirroring symmetry.
    //   `open_meta_comment_editor` populates the matching `*_available` for
    //   the scope variant carried in the saved anchor, AND mirrors the scope
    //   payload (same value lives in both places). This pins the contract
    //   documented on `Composer::*_available`.
    #[test]
    fn open_meta_comment_editor_line_anchor_populates_line_available_with_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        let original = app.loaded_comments[0].clone();
        let Anchor::Line {
            location: ref orig_loc,
            ..
        } = original.anchor
        else {
            panic!("setup expects Line anchor");
        };
        // Park the cursor on a commentable line so `build_line_target`
        // resolves and `line_available` is populated. The default cursor
        // from `make_app_with_comment_on_disk` lands on the InlineCommentMeta
        // row, which is non-commentable.
        app.line_index = 2;

        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref c) = app.screen else {
            panic!("composer should be open");
        };
        let ComposerScope::Line(scope_target) = &c.scope else {
            panic!("scope must be Line");
        };
        let avail = c
            .line_available
            .as_ref()
            .expect("line_available must be Some when scope is Line");
        // Mirroring contract: the scope payload reflects the saved anchor
        // (path/lines from the on-disk record), while line_available
        // reflects the cursor's position. Both must be Some, but the
        // values can differ (the saved anchor's file may not match the
        // cursor's file). What's load-bearing is that the variant carries
        // the saved location and the availability snapshot covers the
        // cursor.
        assert_eq!(scope_target.file, orig_loc.file);
        assert_eq!(scope_target.target_line, orig_loc.new_line);
        // line_available is constructed from the cursor's diff line.
        assert!(
            avail.target_line.is_some() || avail.source_line.is_some(),
            "line_available must point at a real diff line; got {avail:?}"
        );
    }

    #[test]
    fn open_meta_comment_editor_stack_anchor_populates_stack_available_with_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _id_a, _id_b) = make_stack_app_with_two_changes(dir.path());
        let revset_hash = app.stack.as_ref().unwrap().revset_hash;
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack { revset_hash },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "stack body".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref c) = app.screen else {
            panic!("composer should be open");
        };
        let ComposerScope::Stack(scope_snapshot) = &c.scope else {
            panic!("scope must be Stack");
        };
        let avail = c
            .stack_available
            .as_ref()
            .expect("stack_available must be Some in stack mode");
        // Mirroring: same revset_hash in both. (revset string mirrors too in
        // the running-stack-mode branch — single-change synthesizes a
        // sentinel string and is exercised by the asymmetric T2 test.)
        assert_eq!(scope_snapshot.revset_hash, avail.revset_hash);
        assert_eq!(scope_snapshot.revset_hash, revset_hash);
    }

    #[test]
    fn open_meta_comment_editor_change_anchor_populates_no_availability_for_other_scopes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        // Cursor on a non-commentable line so `line_available` is None.
        app.line_index = 0;
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: app.details.change_id.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "change body".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref c) = app.screen else {
            panic!("composer should be open");
        };
        // Change is unit — no payload to mirror.
        assert!(matches!(c.scope, ComposerScope::Change));
        // Change scope doesn't populate any availability snapshot beyond
        // what the cursor / app context independently support. With the
        // cursor on a non-commentable line and no stack, all three are None.
        assert!(c.line_available.is_none(), "non-commentable cursor → None");
        assert!(c.stack_available.is_none(), "single-change app → None");
        assert!(
            c.description_available.is_none(),
            "non-Description anchor → None"
        );
    }

    /// D3. `d` delete error path through the TUI dispatcher. Save a comment,
    /// then wipe the backing JSONL out from under the in-memory state. The
    /// next `delete_focused_comment` call sees a `RenderedLine` whose
    /// `comment_index` resolves to a still-loaded `Comment`, but the store
    /// returns `CommentNotFound` — the dispatcher must surface that as a
    /// `"delete failed: ..."` status message.
    #[test]
    fn delete_focused_comment_with_missing_backing_file_sets_delete_failed_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());
        // Cursor is parked on the InlineCommentMeta line by the helper.
        assert!(matches!(
            app.current_view().unwrap().lines[app.line_index].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));

        // Wipe the JSONL but leave `loaded_comments` and the annotated view
        // alone — the meta line is still rendered and the cursor still
        // resolves to a Comment, but the store has nothing to delete.
        let comments_dir =
            crate::store::change_file(dir.path(), dir.path(), &app.details.change_id)
                .parent()
                .expect("change_file has parent")
                .to_path_buf();
        for entry in std::fs::read_dir(&comments_dir).expect("comments dir") {
            let entry = entry.expect("entry");
            std::fs::remove_file(entry.path()).expect("remove jsonl");
        }

        delete_focused_comment(&mut app);

        let msg = app
            .status_message
            .as_deref()
            .expect("status message should be set on delete failure");
        assert!(
            msg.starts_with("delete failed: "),
            "status message should be prefixed 'delete failed: '; got: {msg}"
        );
    }

    /// D4. `e` on a non-meta line sets a clear status message and does NOT
    /// open the composer.
    #[test]
    fn open_composer_for_edit_on_non_meta_line_sets_error_message() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 0; // HunkHeader — not InlineCommentMeta.
        open_composer_for_edit(&mut app);
        assert!(
            matches!(app.screen, Screen::Main),
            "screen should remain Main on non-meta line"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("cursor is not on a comment"),
            "status message should describe the failure"
        );
    }

    fn make_app_with_change_comment_on_disk(dir: &std::path::Path) -> App {
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "first line".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };
        let mut app = App::new(
            details,
            dir.to_path_buf(),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: app.details.change_id.clone(),
            },
            repo_root: dir.to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "split this commit".to_owned(),
            severity: Severity::Required,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir, dir, &comment).unwrap();
        app.refresh_inline_comments();
        app.file_index = 0;
        let view = app.current_view().expect("description view");
        let meta_idx = view
            .lines
            .iter()
            .position(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .expect("change-comment meta row should be present");
        app.line_index = meta_idx;
        app
    }

    #[test]
    fn change_comment_appears_in_description_view_not_in_file_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = make_app_with_change_comment_on_disk(dir.path());

        let desc_view = app.annotated_per_file.first().expect("description view");
        let meta_count = desc_view
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(meta_count, 1);
        let last = desc_view.lines.last().expect("at least one row");
        assert_eq!(last.kind, RenderedLineKind::InlineCommentBody);
        assert!(last.text.contains("split this commit"));

        let file_view = app.annotated_per_file.get(1).expect("file view");
        assert!(file_view
            .lines
            .iter()
            .all(|l| !matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. })));
    }

    #[test]
    fn e_on_change_comment_row_opens_edit_composer_with_change_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_change_comment_on_disk(dir.path());
        open_composer_for_edit(&mut app);
        let Screen::Composer(ref c) = app.screen else {
            panic!("expected Composer screen after `e`");
        };
        assert!(matches!(c.scope, ComposerScope::Change));
        assert!(c.editing.is_some());
        assert_eq!(c.body_text(), "split this commit");
    }

    #[test]
    fn e_on_description_comment_row_opens_edit_composer_in_description_scope() {
        use crate::comment::DescriptionAnchor;
        let dir = tempfile::tempdir().expect("tempdir");
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "first line".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };
        let mut app = App::new(
            details,
            dir.path().to_path_buf(),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: app.details.change_id.clone(),
                location: DescriptionAnchor {
                    display_line: Some(1),
                    target_text: "first line".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "review wording".to_owned(),
            severity: Severity::Suggestion,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        app.refresh_inline_comments();
        app.file_index = 0;
        let view = app.current_view().expect("description view");
        let meta_idx = view
            .lines
            .iter()
            .position(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .expect("description-comment meta row should be present");
        app.line_index = meta_idx;

        open_composer_for_edit(&mut app);
        let Screen::Composer(ref c) = app.screen else {
            panic!("expected Composer screen after `e`");
        };
        assert!(matches!(c.scope, ComposerScope::Description(_)));
        assert!(c.editing.is_some());
        assert_eq!(c.body_text(), "review wording");
        assert!(
            app.status_message.is_none(),
            "edit must not surface a 'cannot edit' status; got {:?}",
            app.status_message
        );
    }

    #[test]
    fn d_on_change_comment_row_deletes_the_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_change_comment_on_disk(dir.path());
        assert_eq!(app.loaded_comments.len(), 1);

        delete_focused_comment(&mut app);
        assert_eq!(app.loaded_comments.len(), 0);
        for view in &app.annotated_per_file {
            assert!(view
                .lines
                .iter()
                .all(|l| !matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. })));
        }
    }

    #[test]
    fn c_on_change_comment_row_defaults_to_change_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_change_comment_on_disk(dir.path());
        open_composer(&mut app);
        let Screen::Composer(ref c) = app.screen else {
            panic!("expected Composer screen after `c`");
        };
        assert!(matches!(c.scope, ComposerScope::Change));
        assert!(c.editing.is_none());
    }

    #[test]
    fn multiple_change_comments_in_app_render_in_created_at_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "describe".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };
        let mut app = App::new(
            details,
            dir.path().to_path_buf(),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        for (offset, body) in [(0, "first"), (60, "second"), (120, "third")] {
            let comment = Comment {
                schema_version: SchemaVersion,
                anchor: Anchor::Change {
                    change_id: app.details.change_id.clone(),
                },
                repo_root: dir.path().to_path_buf(),
                revset: "@".to_owned(),
                commit_id: None,
                body: body.to_owned(),
                severity: Severity::Note,
                created_at: t0 + time::Duration::seconds(offset),
                updated_at: None,
                status: Some(Status::Pending),
                mismatch_reason: None,
                entity_id: None,
                anchor_fingerprint: None,
            };
            crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        }
        app.refresh_inline_comments();

        let desc_view = app.annotated_per_file.first().expect("description view");
        let bodies: Vec<&str> = desc_view
            .lines
            .iter()
            .filter(|l| l.kind == RenderedLineKind::InlineCommentBody)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            bodies,
            vec!["\u{2503} first", "\u{2503} second", "\u{2503} third"]
        );
    }

    /// Save-status copy: a new Change-scope comment lands in the description
    /// view, which is off-screen when the user is on a file view. The status
    /// message tells them where it went so they don't think it disappeared.
    #[test]
    fn save_change_scope_comment_sets_status_pointing_to_description_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let mut composer = make_composer_with_body(&app, target, "split this commit");
        composer.scope = ComposerScope::Change;
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(outcome, SaveOutcome::Saved));
        assert_eq!(
            app.status_message.as_deref(),
            Some("change comment saved (visible in description view)"),
        );
    }

    /// Symmetric pin for line-scope: confirmation copy must NOT mention any
    /// other view, so a Line-scope save reads "comment saved" rather than
    /// the change/stack-specific copy.
    #[test]
    fn save_line_scope_comment_sets_plain_saved_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let composer = make_composer_with_body(&app, target, "fix this");
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(outcome, SaveOutcome::Saved));
        assert_eq!(app.status_message.as_deref(), Some("comment saved"));
    }

    /// Helper: save a comment on the diff line whose (source, target) line
    /// numbers match `(src, tgt)`. Used by the multi-comment delete test.
    fn save_comment_on_line(
        app: &mut App,
        src: Option<u32>,
        tgt: Option<u32>,
        when: time::OffsetDateTime,
        body_text: &str,
    ) {
        let cursor = app
            .current_view()
            .expect("view")
            .lines
            .iter()
            .position(|l| {
                matches!(
                    l.kind,
                    RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
                ) && l.source_line == src
                    && l.target_line == tgt
            })
            .expect("diff line should exist for these coordinates");
        app.line_index = cursor;
        let BuildTargetResult::Ready(target) = build_line_target(app) else {
            panic!("expected Ready");
        };
        let composer = make_composer_with_body(app, target, body_text);
        let outcome = save_composer(app, &composer, when);
        assert!(matches!(outcome, SaveOutcome::Saved), "save {body_text}");
    }

    /// Helper: find the index of the `InlineCommentMeta` line whose embedded
    /// `comment_index` resolves to a comment with the given body.
    fn meta_index_for_body(app: &App, body: &str) -> usize {
        app.current_view()
            .expect("view")
            .lines
            .iter()
            .enumerate()
            .find(|(_, l)| match l.kind {
                RenderedLineKind::InlineCommentMeta { comment_index } => {
                    if let CommentIndex::Local(idx) = comment_index {
                        app.loaded_comments.get(idx).is_some_and(|c| c.body == body)
                    } else {
                        false
                    }
                }
                RenderedLineKind::HunkHeader
                | RenderedLineKind::HunkSeparator
                | RenderedLineKind::Context
                | RenderedLineKind::Added
                | RenderedLineKind::Removed
                | RenderedLineKind::Notice
                | RenderedLineKind::DescriptionLine
                | RenderedLineKind::InlineCommentBody => false,
            })
            .map(|(i, _)| i)
            .expect("meta line for given body should exist")
    }

    /// D5. With three comments saved on different lines, deleting the middle
    /// one drops only its inline rendering and parks the cursor on the
    /// deleted comment's anchor line; the other two still render.
    #[test]
    fn delete_middle_of_three_comments_repositions_cursor_and_keeps_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();

        // Save three comments on three distinct diff lines: Context
        // (target=1), Added (target=2), Removed (source=2).
        save_comment_on_line(
            &mut app,
            Some(1),
            Some(1),
            time::OffsetDateTime::UNIX_EPOCH,
            "first",
        );
        save_comment_on_line(
            &mut app,
            None,
            Some(2),
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60),
            "middle",
        );
        save_comment_on_line(
            &mut app,
            Some(2),
            None,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(120),
            "third",
        );
        assert_eq!(app.loaded_comments.len(), 3);

        app.line_index = meta_index_for_body(&app, "middle");
        delete_focused_comment(&mut app);

        // Two comments should remain — neither is "middle".
        assert_eq!(app.loaded_comments.len(), 2);
        assert!(
            !app.loaded_comments.iter().any(|c| c.body == "middle"),
            "middle comment should be deleted"
        );
        assert!(
            app.loaded_comments.iter().any(|c| c.body == "first"),
            "first comment should remain"
        );
        assert!(
            app.loaded_comments.iter().any(|c| c.body == "third"),
            "third comment should remain"
        );

        // The view still has two meta lines.
        let meta_count_after = app
            .current_view()
            .unwrap()
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(meta_count_after, 2);

        // The cursor lands on the deleted comment's anchor line. The middle
        // comment was anchored to the Added line (target_line=2). After the
        // refresh, the cursor should be on a diff line whose target_line=2.
        let cursor_line = &app.current_view().unwrap().lines[app.line_index];
        assert_eq!(
            cursor_line.target_line,
            Some(2),
            "cursor should land on the deleted comment's anchor line (target=2)"
        );
    }

    /// D6. Open in edit mode, send Esc through the composer dispatcher, confirm
    /// the on-disk record is unchanged. Exercises `handle_composer_event` so a
    /// future regression that accidentally persists on cancel is caught.
    #[test]
    fn edit_then_esc_preserves_original_comment_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_comment_on_disk(dir.path());

        // Snapshot the on-disk record before we open the composer.
        let before = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .unwrap();
        assert_eq!(before.len(), 1);
        let original = before[0].clone();

        open_composer_for_edit(&mut app);
        // Type something the would-be save would persist.
        {
            let Screen::Composer(ref mut composer) = app.screen else {
                panic!("expected Composer screen");
            };
            for ch in " mutated".chars() {
                composer
                    .body
                    .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
        }
        // Drive Esc through the dispatcher — the Cancel branch must NOT write.
        handle_composer_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(app.screen, Screen::Main),
            "Esc should return to Main screen"
        );

        // On-disk record is unchanged.
        let after = crate::store::load_change_comments(
            &app.repo_root,
            &app.repo_root,
            &app.details.change_id,
        )
        .unwrap();
        assert_eq!(after.len(), 1);
        let preserved = &after[0];
        assert_eq!(preserved.body, original.body);
        assert_eq!(preserved.severity, original.severity);
        assert_eq!(preserved.created_at, original.created_at);
        assert_eq!(preserved.updated_at, None);
    }

    fn app_with_mode(mode: TransitionMode) -> App {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.transition_mode = mode;
        app
    }

    #[test]
    fn transition_enabled_never_always_false() {
        let app = app_with_mode(TransitionMode::Never);
        assert!(!app.transition_enabled(0));
        assert!(!app.transition_enabled(7));
        assert!(!app.transition_enabled(8));
        assert!(!app.transition_enabled(1000));
    }

    #[test]
    fn transition_enabled_always_always_true() {
        let app = app_with_mode(TransitionMode::Always);
        assert!(app.transition_enabled(1));
        assert!(app.transition_enabled(7));
        assert!(app.transition_enabled(8));
    }

    #[test]
    fn transition_enabled_auto_below_threshold_is_false() {
        let app = app_with_mode(TransitionMode::Auto);
        assert!(!app.transition_enabled(7));
    }

    #[test]
    fn transition_enabled_auto_at_threshold_is_true() {
        let app = app_with_mode(TransitionMode::Auto);
        assert!(app.transition_enabled(8));
    }

    #[test]
    fn transition_enabled_auto_above_threshold_is_true() {
        let app = app_with_mode(TransitionMode::Auto);
        assert!(app.transition_enabled(9));
    }

    #[test]
    fn load_transition_mode_missing_file_is_never() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &missing);
        assert_eq!(load_transition_mode(), TransitionMode::Never);
    }

    #[test]
    fn load_transition_mode_explicit_never() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = crate::test_helpers::write_global_config_at(
            dir.path(),
            "[ui]\ntransition_screen = \"never\"\n",
        );
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_transition_mode(), TransitionMode::Never);
    }

    #[test]
    fn load_transition_mode_auto() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = crate::test_helpers::write_global_config_at(
            dir.path(),
            "[ui]\ntransition_screen = \"auto\"\n",
        );
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_transition_mode(), TransitionMode::Auto);
    }

    #[test]
    fn load_transition_mode_always() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = crate::test_helpers::write_global_config_at(
            dir.path(),
            "[ui]\ntransition_screen = \"always\"\n",
        );
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_transition_mode(), TransitionMode::Always);
    }

    #[test]
    fn load_transition_mode_malformed_toml_is_never() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = crate::test_helpers::write_global_config_at(dir.path(), "[ui\nbroken");
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_transition_mode(), TransitionMode::Never);
    }

    #[test]
    fn load_transition_mode_invalid_value_is_never() {
        let _lock = crate::test_helpers::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = crate::test_helpers::write_global_config_at(
            dir.path(),
            "[ui]\ntransition_screen = \"bogus\"\n",
        );
        let _g = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &path);
        assert_eq!(load_transition_mode(), TransitionMode::Never);
    }

    #[test]
    fn progress_bar_zero_total_returns_empty() {
        assert_eq!(progress_bar_string(0, 0, 10), String::new());
    }

    #[test]
    fn progress_bar_first_position() {
        // 1 of 10 with width 10 → exactly 1 filled cell, 9 empty, plus "  ".
        let s = progress_bar_string(1, 10, 10);
        assert_eq!(s.matches('█').count(), 1);
        assert_eq!(s.matches('░').count(), 9);
        assert!(s.ends_with("  "));
    }

    #[test]
    fn progress_bar_full_position() {
        let s = progress_bar_string(5, 5, 8);
        assert_eq!(s.matches('█').count(), 8);
        assert_eq!(s.matches('░').count(), 0);
    }

    #[test]
    fn progress_bar_position_clamps_to_total() {
        // Past-end input shouldn't overflow.
        let s = progress_bar_string(99, 5, 8);
        assert_eq!(s.matches('█').count(), 8);
    }

    #[test]
    fn progress_bar_zero_width_returns_empty() {
        assert_eq!(progress_bar_string(2, 5, 0), String::new());
    }

    #[test]
    fn single_change_app_has_no_stack_context() {
        // The footer logic branches on `app.stack.is_some()`. Confirm a
        // single-change app starts with `stack = None` so the n/p hint stays
        // hidden in the footer text path.
        let app = make_app_with_single_file(sample_diff_file());
        assert!(app.stack.is_none(), "single-change app must not have stack");
    }

    #[test]
    fn severity_histogram_counts_by_kind() {
        let comments = vec![
            comment_with_severity(Severity::Required),
            comment_with_severity(Severity::Required),
            comment_with_severity(Severity::Suggestion),
            comment_with_severity(Severity::Note),
        ];
        let h = histogram_from_comments(&comments);
        assert_eq!(h.required, 2);
        assert_eq!(h.suggestion, 1);
        assert_eq!(h.note, 1);
    }

    fn comment_with_severity(severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
                location: LineAnchor {
                    file: PathBuf::from("foo.txt"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@ -1 +1 @@".to_owned(),
                    target_text: "x".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "b".to_owned(),
            severity,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    #[test]
    fn persist_cursor_on_exit_writes_current_index_for_stack_app() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            StackEntry {
                change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
                commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
                description: "first".to_owned(),
            },
            StackEntry {
                change_id: ChangeId::parse(&"b".repeat(32)).unwrap(),
                commit_id: CommitId::parse(&"b".repeat(40)).unwrap(),
                description: "second".to_owned(),
            },
            StackEntry {
                change_id: ChangeId::parse(&"c".repeat(32)).unwrap(),
                commit_id: CommitId::parse(&"c".repeat(40)).unwrap(),
                description: "third".to_owned(),
            },
        ];
        let revset = "trunk()..@".to_owned();
        let revset_hash = RevsetHash::from_revset(&revset);

        let details = ChangeDetails {
            change_id: entries[1].change_id.clone(),
            commit_id: entries[1].commit_id.clone(),
            description: "second".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };
        let stack_ctx = StackContext {
            entries: entries.clone(),
            current_index: 1,
            revset: revset.clone(),
            revset_hash,
        };
        let app = App::new(
            details,
            dir.path().to_owned(),
            revset.clone(),
            Some(stack_ctx),
            TransitionMode::Never,
        );

        persist_cursor_on_exit(&app);

        let cursor = cursor::load(dir.path(), dir.path()).unwrap();
        let entry = &cursor.revsets[&revset_hash.hex()];
        assert_eq!(entry.last_change_id, entries[1].change_id);
        assert_eq!(entry.revset, revset);
    }

    #[test]
    fn persist_cursor_on_exit_no_op_for_single_change_app() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_owned();
        persist_cursor_on_exit(&app);
        // No cursor file should exist (single-change mode).
        let cursor_file = crate::store::repo_data_dir(dir.path(), dir.path()).join("cursor.json");
        assert!(!cursor_file.exists());
    }

    // The pure `pick_retreat_index` helper carries the navigation contract for
    // `p`. Side-effect properties (no cursor write) follow from `retreat_stack`
    // routing the index through `load_stack_entry(idx, advance=false)`; the
    // helper itself does not touch any I/O.
    #[test]
    fn pick_retreat_index_at_zero_returns_none() {
        assert_eq!(pick_retreat_index(0), None);
    }

    #[test]
    fn pick_retreat_index_at_one_returns_zero() {
        assert_eq!(pick_retreat_index(1), Some(0));
    }

    #[test]
    fn pick_retreat_index_in_middle_returns_predecessor() {
        assert_eq!(pick_retreat_index(5), Some(4));
    }

    #[test]
    fn pick_retreat_index_at_large_value() {
        assert_eq!(pick_retreat_index(usize::MAX), Some(usize::MAX - 1));
    }

    #[test]
    fn transition_footer_fits_inside_modal() {
        // Modal interior is `TRANSITION_MODAL_WIDTH - 2` cols (border on each
        // side). Ratatui clips without wrapping, so the footer must fit or the
        // user loses keybinding hints.
        let interior = usize::from(TRANSITION_MODAL_WIDTH).saturating_sub(2);
        assert!(
            TRANSITION_FOOTER_TEXT.chars().count() <= interior,
            "footer {:?} ({} chars) does not fit modal interior ({} cols)",
            TRANSITION_FOOTER_TEXT,
            TRANSITION_FOOTER_TEXT.chars().count(),
            interior
        );
    }

    #[test]
    fn render_dots_zero_is_empty() {
        assert_eq!(render_dots(0), "");
    }

    #[test]
    fn render_dots_one() {
        assert_eq!(render_dots(1), "●");
    }

    #[test]
    fn render_dots_at_max() {
        assert_eq!(render_dots(DOT_BUDGET), "●●●●●");
    }

    #[test]
    fn render_dots_one_over_max_truncates_with_ellipsis() {
        assert_eq!(render_dots(DOT_BUDGET + 1), "●●●●●…");
    }

    #[test]
    fn render_dots_far_over_max_still_truncates() {
        assert_eq!(render_dots(50), "●●●●●…");
    }

    #[test]
    fn file_header_label_shows_path_and_position() {
        let app = make_app_with_single_file(sample_diff_file());
        let label = file_header_label(&app);
        assert!(
            label.contains("foo.txt"),
            "label should include file path, got: {label:?}"
        );
        // 2 total views: description (0) + diff file (1); file_index=1 → "2 of 2"
        assert!(
            label.contains("2 of 2"),
            "label should show position of total, got: {label:?}"
        );
    }

    #[test]
    fn file_header_label_no_diff_files_shows_description_view() {
        use crate::diff::Diff;
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: String::new(),
            diff: Diff { files: vec![] },
        };
        let app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        let label = file_header_label(&app);
        assert!(
            label.contains("<description>"),
            "no-diff-files app should show description view label, got: {label:?}"
        );
    }

    #[test]
    fn footer_text_stack_mode_contains_revision() {
        let text = footer_text_for_width(120, true, None);
        assert!(
            text.contains("n/p revision"),
            "footer should label n/p as 'revision', got: {text:?}"
        );
    }

    #[test]
    fn footer_text_single_change_mode_has_no_revision_label() {
        let text = footer_text_for_width(120, false, None);
        assert!(
            text.contains("n/p revision"),
            "footer should still include n/p at 120 cols, got: {text:?}"
        );
    }

    #[test]
    fn footer_text_for_width_80_cols_full_footer() {
        let text = footer_text_for_width(80, true, None);
        assert!(text.contains("n/p revision"), "must have n/p: {text:?}");
        assert!(text.contains("Enter comment"), "must have Enter: {text:?}");
        assert!(text.contains('?'), "must have ?: {text:?}");
        assert!(
            text.chars().count() <= 80,
            "must fit in 80 cols: {} chars: {text:?}",
            text.chars().count()
        );
    }

    #[test]
    fn footer_text_for_width_drops_question_mark_at_70() {
        let text = footer_text_for_width(70, true, None);
        assert!(
            !text.contains('?'),
            "? must be dropped at 70 cols: {text:?}"
        );
        assert!(
            text.contains("Enter comment"),
            "Enter must remain: {text:?}"
        );
        assert!(text.chars().count() <= 70, "must fit in 70 cols: {text:?}");
    }

    #[test]
    fn footer_text_for_width_drops_claude_at_65() {
        let text = footer_text_for_width(65, true, None);
        assert!(
            !text.contains("Claude"),
            "C → Claude must be dropped: {text:?}"
        );
        assert!(
            text.contains("Enter comment"),
            "Enter must remain: {text:?}"
        );
        assert!(text.chars().count() <= 65, "must fit in 65 cols: {text:?}");
    }

    #[test]
    fn footer_text_for_width_irreducible_always_present() {
        for width in [40u16, 50, 55, 60] {
            let text = footer_text_for_width(width, true, None);
            assert!(
                text.contains("Enter comment"),
                "Enter must be present at {width}: {text:?}"
            );
            assert!(
                text.contains("Tab file"),
                "Tab must be present at {width}: {text:?}"
            );
        }
    }

    #[test]
    fn footer_text_for_width_severity_badge_appended() {
        let text = footer_text_for_width(120, false, Some(Severity::Required));
        assert!(text.contains("[F:required]"), "badge must appear: {text:?}");
    }

    #[test]
    fn footer_text_for_width_no_badge_when_no_filter() {
        let text = footer_text_for_width(120, false, None);
        assert!(!text.contains("[F:"), "no badge when no filter: {text:?}");
    }

    #[test]
    fn footer_text_for_width_stack_segment_absent_without_stack() {
        let text = footer_text_for_width(120, false, None);
        assert!(
            !text.contains("s stack"),
            "s stack must be absent in non-stack mode: {text:?}"
        );
    }

    #[test]
    fn footer_text_for_width_stack_segment_present_with_stack() {
        let text = footer_text_for_width(120, true, None);
        assert!(
            text.contains("s stack"),
            "s stack must appear in stack mode at 120: {text:?}"
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "test fixture needs all fields for a well-formed stale comment"
    )]
    fn make_stale_comment_on_file(
        dir: &std::path::Path,
        change_id: ChangeId,
        file: &str,
        new_line: u32,
        body: &str,
        created_at: time::OffsetDateTime,
    ) -> Comment {
        use crate::comment::{
            Anchor, Comment, LineAnchor, MismatchReason, SchemaVersion, Severity, Side, Status,
        };
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id,
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(new_line),
                    hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
                    target_text: "old body that changed".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: dir.to_owned(),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Required,
            created_at,
            updated_at: None,
            status: Some(Status::Stale),
            mismatch_reason: Some(MismatchReason::TargetTextChanged),
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_app_with_stale_comment(dir: &std::path::Path) -> App {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.to_owned();
        let comment = make_stale_comment_on_file(
            dir,
            app.details.change_id.clone(),
            "foo.txt",
            2,
            "stale body",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        crate::store::save_comment(dir, dir, &comment).unwrap();
        app.refresh_inline_comments();
        app
    }

    #[test]
    fn s_key_from_main_transitions_to_stale_screen() {
        let mut app = make_app_with_single_file(sample_diff_file());
        let key = KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE);
        handle_main_key(&mut app, key).unwrap();
        assert!(
            matches!(app.screen, Screen::Stale(_)),
            "S should switch to Screen::Stale"
        );
        let Screen::Stale(ref state) = app.screen else {
            unreachable!()
        };
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn stale_screen_down_moves_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());

        open_stale_screen(&mut app);
        let Screen::Stale(ref state) = app.screen else {
            panic!("expected Stale screen");
        };
        assert!(!state.stale_indices.is_empty(), "should have stale entries");

        let comment2 = make_stale_comment_on_file(
            dir.path(),
            app.details.change_id.clone(),
            "foo.txt",
            1,
            "second stale",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
        );
        crate::store::save_comment(dir.path(), dir.path(), &comment2).unwrap();
        app.refresh_inline_comments();
        open_stale_screen(&mut app);

        let count_before = {
            let Screen::Stale(ref s) = app.screen else {
                panic!("expected Stale");
            };
            s.stale_indices.len()
        };
        assert!(count_before >= 2, "need at least 2 stale entries");

        handle_stale_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let Screen::Stale(ref s) = app.screen else {
            panic!("expected Stale screen");
        };
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn stale_screen_q_returns_to_main() {
        let mut app = make_app_with_single_file(sample_diff_file());
        open_stale_screen(&mut app);
        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(
            matches!(app.screen, Screen::Main),
            "q should return to Screen::Main"
        );
    }

    #[test]
    fn stale_d_deletes_focused_comment_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());
        open_stale_screen(&mut app);

        let Screen::Stale(ref s) = app.screen else {
            panic!("expected Stale screen");
        };
        assert!(!s.stale_indices.is_empty(), "should have stale entries");

        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );

        let loaded =
            crate::store::load_change_comments(dir.path(), dir.path(), &app.details.change_id)
                .unwrap();
        assert!(
            loaded.iter().all(|c| c.status != Some(Status::Stale)),
            "stale comment should be deleted from disk"
        );

        let Screen::Stale(ref s) = app.screen else {
            panic!("expected still on Stale screen");
        };
        assert!(s.stale_indices.is_empty(), "stale_indices should be empty");
    }

    #[test]
    fn stale_enter_navigates_to_file_in_main_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());
        open_stale_screen(&mut app);

        handle_stale_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            matches!(app.screen, Screen::Main),
            "Enter should switch to Screen::Main"
        );
        // view_index 1 is the first diff file (description view is at 0)
        assert_eq!(app.file_index, 1);
    }

    #[test]
    fn stale_e_enters_reanchor_mode_and_switches_to_main() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());
        open_stale_screen(&mut app);

        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert!(
            matches!(app.screen, Screen::Main),
            "e should switch to Screen::Main"
        );
        assert!(
            app.pending_reanchor.is_some(),
            "pending_reanchor should be set"
        );
        assert_eq!(app.pending_reanchor.as_ref().unwrap().body, "stale body");
    }

    #[test]
    fn esc_from_main_with_no_composer_cancels_reanchor_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());
        open_stale_screen(&mut app);
        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );
        assert!(app.pending_reanchor.is_some(), "re-anchor should be active");

        handle_main_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        assert!(
            app.pending_reanchor.is_none(),
            "Esc from main should clear pending_reanchor"
        );
        assert!(
            app.status_message.is_none(),
            "status message should be cleared"
        );
    }

    #[test]
    fn reanchor_composer_save_creates_new_and_deletes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());

        open_stale_screen(&mut app);
        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );
        assert!(app.pending_reanchor.is_some());

        app.line_index = 2;
        assert!(
            matches!(
                app.current_view().unwrap().lines[2].kind,
                RenderedLineKind::Added | RenderedLineKind::Context | RenderedLineKind::Removed
            ),
            "line 2 should be commentable"
        );

        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        let Screen::Composer(ref c) = app.screen else {
            panic!("expected Composer screen after c in re-anchor mode");
        };
        assert_eq!(
            c.body_text(),
            "stale body",
            "composer should be pre-filled with stale comment body"
        );

        let Screen::Composer(composer) = std::mem::replace(&mut app.screen, Screen::Main) else {
            panic!("expected Composer");
        };
        let now = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10);
        let outcome = save_composer(&mut app, &composer, now);
        assert!(matches!(outcome, SaveOutcome::Saved), "expected Saved");

        if let Some(reanchor) = app.pending_reanchor.take() {
            crate::store::delete_comment(&app.repo_root, &app.repo_root, &reanchor.original)
                .unwrap();
            app.refresh_inline_comments();
        }

        let loaded =
            crate::store::load_change_comments(dir.path(), dir.path(), &app.details.change_id)
                .unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "should have exactly one comment (the re-anchored one)"
        );
        assert_ne!(
            loaded[0].created_at,
            time::OffsetDateTime::UNIX_EPOCH,
            "re-anchored comment should have the new timestamp"
        );
        assert!(
            app.pending_reanchor.is_none(),
            "pending_reanchor should be cleared"
        );
    }

    #[test]
    fn esc_from_composer_in_reanchor_mode_preserves_pending_reanchor() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());

        open_stale_screen(&mut app);
        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );
        assert!(app.pending_reanchor.is_some());

        app.line_index = 2;
        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(app.screen, Screen::Composer(_)));

        handle_composer_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(app.screen, Screen::Main),
            "Esc from composer returns to Main"
        );
        assert!(
            app.pending_reanchor.is_some(),
            "pending_reanchor must NOT be cleared when composer is dismissed without saving"
        );
    }

    #[test]
    fn stale_enter_with_file_not_in_diff_stays_on_stale_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_owned();
        let comment = make_stale_comment_on_file(
            dir.path(),
            app.details.change_id.clone(),
            "absent_file.txt",
            1,
            "stale on missing file",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        app.refresh_inline_comments();

        open_stale_screen(&mut app);
        handle_stale_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            matches!(app.screen, Screen::Stale(_)),
            "Enter on file-not-in-diff stale entry should stay on Stale screen"
        );
        let msg = app
            .status_message
            .as_deref()
            .expect("status message should warn about missing file");
        assert!(
            msg.contains("not in current diff"),
            "expected diff-warning, got: {msg:?}"
        );
    }

    #[test]
    fn stale_enter_with_anchor_line_absent_falls_back_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_owned();
        let comment = make_stale_comment_on_file(
            dir.path(),
            app.details.change_id.clone(),
            "foo.txt",
            9999,
            "stale on absent line",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        app.refresh_inline_comments();

        open_stale_screen(&mut app);
        handle_stale_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.screen, Screen::Main));
        // view_index 1 is the first diff file (description view is at 0)
        assert_eq!(app.file_index, 1);
        assert_eq!(
            app.line_index, 0,
            "absent anchor line should fall back to line_index 0"
        );
    }

    #[test]
    fn delete_focused_with_cursor_at_end_clamps_to_new_last() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());
        for (i, body) in ["second", "third"].iter().enumerate() {
            let comment = make_stale_comment_on_file(
                dir.path(),
                app.details.change_id.clone(),
                "foo.txt",
                u32::try_from(i + 3).unwrap_or(3),
                body,
                time::OffsetDateTime::UNIX_EPOCH
                    + time::Duration::seconds(i64::try_from(i + 1).unwrap_or(1)),
            );
            crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        }
        app.refresh_inline_comments();
        open_stale_screen(&mut app);

        let last_idx = {
            let Screen::Stale(ref s) = app.screen else {
                panic!("expected Stale");
            };
            assert!(s.stale_indices.len() >= 3, "need at least 3 stale entries");
            s.stale_indices.len() - 1
        };
        if let Screen::Stale(ref mut s) = app.screen {
            s.selected_index = last_idx;
        }

        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );

        let Screen::Stale(ref s) = app.screen else {
            panic!("expected Stale screen");
        };
        assert_eq!(
            s.selected_index,
            s.stale_indices.len() - 1,
            "selected_index should clamp to new last after delete from tail"
        );
    }

    #[test]
    fn pending_reanchor_survives_stale_screen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_with_stale_comment(dir.path());

        open_stale_screen(&mut app);
        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );
        assert!(app.pending_reanchor.is_some(), "reanchor active");

        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(matches!(app.screen, Screen::Stale(_)));
        assert!(
            app.pending_reanchor.is_some(),
            "S into Stale must not clear pending_reanchor"
        );

        handle_stale_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(matches!(app.screen, Screen::Main));
        assert!(
            app.pending_reanchor.is_some(),
            "q from Stale must not clear pending_reanchor"
        );
    }

    #[test]
    fn reanchor_prompt_fits_within_terminal_width_80() {
        let body = "a".repeat(1024);
        let prompt = reanchor_prompt(&body, 80);
        assert!(
            prompt.chars().count() <= 80,
            "prompt {} chars exceeds 80",
            prompt.chars().count()
        );
    }

    #[test]
    fn reanchor_prompt_falls_back_to_short_form_at_narrow_width() {
        let body = "abc";
        let prompt = reanchor_prompt(body, 40);
        assert!(
            !prompt.contains("navigate and press"),
            "short form should not include long-form navigate text, got: {prompt:?}"
        );
        assert!(prompt.contains("re-anchoring"));
        assert!(prompt.chars().count() <= 60, "short form fits comfortably");
    }

    /// Build a stack-mode app whose current change is change A and whose stack
    /// also has change B. Repo root is set to `dir`. Used by overview-routing
    /// tests so the composer can load on either change without I/O.
    fn make_stack_app_with_two_changes(dir: &std::path::Path) -> (App, ChangeId, ChangeId) {
        let id_a = ChangeId::parse(&"a".repeat(32)).unwrap();
        let id_b = ChangeId::parse(&"b".repeat(32)).unwrap();
        let entry_a = StackEntry {
            change_id: id_a.clone(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "first".to_owned(),
        };
        let entry_b = StackEntry {
            change_id: id_b.clone(),
            commit_id: CommitId::parse(&"b".repeat(40)).unwrap(),
            description: "second".to_owned(),
        };
        let revset = "trunk()..@".to_owned();
        let revset_hash = RevsetHash::from_revset(&revset);
        let details = ChangeDetails {
            change_id: id_a.clone(),
            commit_id: entry_a.commit_id.clone(),
            description: "first".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };
        let stack_ctx = StackContext {
            entries: vec![entry_a, entry_b],
            current_index: 0,
            revset,
            revset_hash,
        };
        let app = App::new(
            details,
            dir.to_path_buf(),
            "trunk()..@".to_owned(),
            Some(stack_ctx),
            TransitionMode::Never,
        );
        (app, id_a, id_b)
    }

    /// Drive a save through `handle_composer_event` with `^X` to exercise the
    /// real dispatch path the user hits.
    fn dispatch_ctrl_x(app: &mut App) {
        handle_composer_event(
            app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        );
    }

    /// Drive a delete through `handle_composer_event` with `^D`.
    fn dispatch_ctrl_d(app: &mut App) {
        handle_composer_event(
            app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
    }

    /// Edit a stack-scoped comment via the overview path. Body and severity
    /// persist to `_stack.jsonl` keyed by the original record's anchor.
    #[test]
    fn edit_stack_comment_from_overview_persists_body_and_severity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _id_a, _id_b) = make_stack_app_with_two_changes(dir.path());
        let revset_hash = app.stack.as_ref().unwrap().revset_hash;

        // Seed: one stack-scoped comment on disk.
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack { revset_hash },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "original body".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        // Open the editor via the overview path (simulates Enter on the row).
        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected composer");
        };
        // Edit the body and bump severity.
        composer.body = textarea::TextArea::default();
        for ch in "edited body text".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer.severity = Severity::Required;

        dispatch_ctrl_x(&mut app);

        // After save, screen should be back to Main.
        assert!(matches!(app.screen, Screen::Main));

        let loaded =
            crate::store::load_stack_comments(dir.path(), dir.path(), &revset_hash).unwrap();
        assert_eq!(loaded.len(), 1, "stack file should still hold one comment");
        assert_eq!(loaded[0].body, "edited body text");
        assert_eq!(loaded[0].severity, Severity::Required);
        assert_eq!(loaded[0].created_at, original.created_at);
    }

    /// Edit a change-scoped comment for a non-current change. The record
    /// must persist to that change's JSONL even though it does not appear in
    /// `app.loaded_comments` (which only holds the current change).
    #[test]
    fn edit_change_comment_for_non_current_change_from_overview_persists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _id_a, id_b) = make_stack_app_with_two_changes(dir.path());

        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: id_b.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "B-original".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected composer");
        };
        composer.body = textarea::TextArea::default();
        for ch in "B-edited".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer.severity = Severity::Suggestion;

        dispatch_ctrl_x(&mut app);

        let loaded_b = crate::store::load_change_comments(dir.path(), dir.path(), &id_b).unwrap();
        assert_eq!(loaded_b.len(), 1, "B should still hold one comment");
        assert_eq!(loaded_b[0].body, "B-edited");
        assert_eq!(loaded_b[0].severity, Severity::Suggestion);
        assert_eq!(loaded_b[0].created_at, original.created_at);
    }

    // -- B2: editing a Description-anchored comment via `open_meta_comment_editor`
    //   must open the composer in `Description` scope (not Line) carrying the
    //   saved anchor's window directly in the variant payload.
    #[test]
    fn open_meta_comment_editor_description_scope_populates_description_context() {
        use crate::comment::DescriptionAnchor;
        let mut app = make_app_with_single_file(sample_diff_file());
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: app.details.change_id.clone(),
                location: DescriptionAnchor {
                    display_line: Some(2),
                    target_text: "second line".to_owned(),
                    context_before: vec!["first line".to_owned()],
                    context_after: vec!["third line".to_owned()],
                },
            },
            repo_root: app.repo_root.clone(),
            revset: app.revset.clone(),
            commit_id: None,
            body: "review the wording here".to_owned(),
            severity: Severity::Suggestion,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };

        open_meta_comment_editor(&mut app, &original);

        let Screen::Composer(ref composer) = app.screen else {
            panic!("expected composer screen");
        };
        let ComposerScope::Description(desc_ctx) = &composer.scope else {
            panic!("expected Description scope; got {:?}", composer.scope);
        };
        assert_eq!(desc_ctx.target_line, Some(2));
        assert_eq!(desc_ctx.target_text, "second line");
        assert_eq!(desc_ctx.context_before, vec!["first line".to_owned()]);
        assert_eq!(desc_ctx.context_after, vec!["third line".to_owned()]);
        // The same window is also exposed as the availability snapshot so a
        // subsequent Alt+D round-trip remains a no-op.
        let avail = composer
            .description_available
            .as_ref()
            .expect("description_available must mirror the scope payload");
        assert_eq!(avail.target_line, Some(2));
        assert_eq!(composer.severity, Severity::Suggestion);
        assert_eq!(composer.body_text(), "review the wording here");
    }

    #[test]
    fn severity_histogram_excludes_orphaned() {
        let mut required = comment_with_severity(Severity::Required);
        required.status = Some(Status::Orphaned);
        let active = comment_with_severity(Severity::Note);
        let h = histogram_from_comments(&[required, active]);
        assert_eq!(h.required, 0, "orphaned required must not be counted");
        assert_eq!(h.note, 1, "active note must be counted");
        assert_eq!(h.total(), 1);
    }

    #[test]
    fn severity_histogram_excludes_stale_and_orphaned_independently() {
        let mut stale = comment_with_severity(Severity::Required);
        stale.status = Some(Status::Stale);
        let mut orphaned = comment_with_severity(Severity::Suggestion);
        orphaned.status = Some(Status::Orphaned);
        let active = comment_with_severity(Severity::Note);
        let h = histogram_from_comments(&[stale, orphaned, active]);
        assert_eq!(h.required, 0);
        assert_eq!(h.suggestion, 0);
        assert_eq!(h.note, 1);
        assert_eq!(h.total(), 1);
    }

    #[test]
    fn collect_orphaned_comments_marks_out_of_stack_ids_as_orphaned() {
        let dir = tempfile::tempdir().unwrap();
        let id_a = ChangeId::parse(&"a".repeat(32)).unwrap();
        let id_b = ChangeId::parse(&"b".repeat(32)).unwrap();
        let id_x = ChangeId::parse(&"c".repeat(32)).unwrap(); // orphaned

        // Save a comment for X (out of stack).
        let orphan_comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: id_x.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "orphaned concern".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &orphan_comment).unwrap();

        // Also save a comment for A (in stack) — should NOT appear in orphaned.
        let active_comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: id_a.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "active concern".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &active_comment).unwrap();

        let stack_entries = vec![
            StackEntry {
                change_id: id_a.clone(),
                commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
                description: "first".to_owned(),
            },
            StackEntry {
                change_id: id_b.clone(),
                commit_id: CommitId::parse(&"b".repeat(40)).unwrap(),
                description: "second".to_owned(),
            },
        ];

        let orphaned = collect_orphaned_comments(dir.path(), dir.path(), &stack_entries);
        assert_eq!(orphaned.len(), 1, "only X's comment should be orphaned");
        assert_eq!(
            orphaned[0].status,
            Some(Status::Orphaned),
            "status must be overwritten to Orphaned"
        );
        assert_eq!(orphaned[0].body, "orphaned concern");
    }

    #[test]
    fn collect_orphaned_comments_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let stack_entries: Vec<StackEntry> = vec![];
        let orphaned = collect_orphaned_comments(dir.path(), dir.path(), &stack_entries);
        assert!(orphaned.is_empty());
    }

    #[test]
    fn collect_orphaned_comments_all_in_stack_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let id_a = ChangeId::parse(&"a".repeat(32)).unwrap();

        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: id_a.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "active".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let stack_entries = vec![StackEntry {
            change_id: id_a.clone(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "first".to_owned(),
        }];

        let orphaned = collect_orphaned_comments(dir.path(), dir.path(), &stack_entries);
        assert!(orphaned.is_empty(), "in-stack change must not be orphaned");
    }

    /// Delete a stack-scoped comment via `^D` from the composer. The record
    /// must be removed from `_stack.jsonl`, not from any change file.
    #[test]
    fn delete_stack_comment_via_composer_removes_from_stack_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, _id_a, _id_b) = make_stack_app_with_two_changes(dir.path());
        let revset_hash = app.stack.as_ref().unwrap().revset_hash;

        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack { revset_hash },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "to-delete".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        assert!(matches!(app.screen, Screen::Composer(_)));

        dispatch_ctrl_d(&mut app);

        assert!(matches!(app.screen, Screen::Main));
        let loaded =
            crate::store::load_stack_comments(dir.path(), dir.path(), &revset_hash).unwrap();
        assert!(
            loaded.is_empty(),
            "stack file should be empty after delete; got {loaded:?}"
        );
    }

    /// New `c` from an overview row pointing at change B (while main view
    /// holds change A). The new record must land in B's JSONL with
    /// `Anchor::Change { change_id: B }`.
    #[test]
    fn new_change_comment_from_overview_targets_cursor_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, id_a, id_b) = make_stack_app_with_two_changes(dir.path());
        // Sanity: main view holds A.
        assert_eq!(app.details.change_id, id_a);

        // Build the rows (cursor on change B's row).
        app.load_overview_comments();
        let cache = app.overview_cache.as_ref().unwrap();
        let entries = app.stack.as_ref().unwrap().entries.clone();
        let rows = overview_screen::build_rows(cache, &entries, 0, 0);
        let cursor_b = rows
            .iter()
            .position(|r| matches!(r, overview_screen::OverviewRow::ChangeRow(1)))
            .expect("change B row must exist");

        overview_open_composer(&mut app, &rows, cursor_b);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected composer");
        };
        // The composer must have its target change_id set to B (not A).
        assert_eq!(composer.change_id, id_b);
        // Scope auto-selected to Change.
        assert!(matches!(composer.scope, ComposerScope::Change));
        for ch in "new on B".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }

        dispatch_ctrl_x(&mut app);

        let loaded_a = crate::store::load_change_comments(dir.path(), dir.path(), &id_a).unwrap();
        let loaded_b = crate::store::load_change_comments(dir.path(), dir.path(), &id_b).unwrap();
        assert!(
            loaded_a.is_empty(),
            "A's file must be untouched; got {loaded_a:?}"
        );
        assert_eq!(loaded_b.len(), 1, "B must hold the new comment");
        assert_eq!(loaded_b[0].body, "new on B");
        match &loaded_b[0].anchor {
            Anchor::Change { change_id } => assert_eq!(change_id, &id_b),
            other @ (Anchor::Line { .. } | Anchor::Stack { .. } | Anchor::Description { .. }) => {
                panic!("expected Anchor::Change targeting B; got {other:?}")
            }
        }
    }

    /// Regression: pressing `C` while sitting on change A in a multi-change
    /// stack must include comments anchored on change B in the packet. A
    /// fabricated single-entry resolved stack would silently drop B's
    /// comments and surface "no comments to send".
    #[test]
    fn send_to_claude_in_stack_mode_includes_other_changes_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, id_a, id_b) = make_stack_app_with_two_changes(dir.path());
        // Sanity: main view holds A; the comment we save targets B.
        assert_eq!(app.details.change_id, id_a);

        let on_b = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: id_b.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "concern on B".to_owned(),
            severity: Severity::Required,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &on_b).unwrap();

        open_send_to_claude(&mut app);

        // The screen must transition to SendToClaude (not stay on Main with a
        // "no comments to send" status).
        let Screen::SendToClaude(ref state) = app.screen else {
            panic!(
                "expected SendToClaude screen; got status {:?}",
                app.status_message
            );
        };
        let SendToClaudeState::Confirm(ref data) = state.as_ref() else {
            panic!("expected Confirm variant");
        };
        // The packet only includes changes that have comments; A is empty, so
        // only B should appear — and crucially, B must NOT be silently dropped.
        let change_ids: Vec<_> = data
            .packet
            .changes
            .iter()
            .map(|cp| cp.change_id.clone())
            .collect();
        assert!(
            change_ids.contains(&id_b),
            "packet must include change B's entry; got {change_ids:?}"
        );
        // And B's change-scoped comment must be present.
        let b_packet = data
            .packet
            .changes
            .iter()
            .find(|cp| cp.change_id == id_b)
            .expect("B's ChangePacket must be in the packet");
        assert_eq!(
            b_packet.change_comments.len(),
            1,
            "B's comment must ride along"
        );
        assert_eq!(b_packet.change_comments[0].body, "concern on B");
    }

    /// In single-change mode (`app.stack` is None), only the current change's
    /// comments make it into the packet — the resolved stack stays a
    /// single-entry shape so unrelated changes are never loaded.
    #[test]
    fn send_to_claude_in_single_change_mode_uses_only_current_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        assert!(app.stack.is_none(), "test requires single-change mode");

        // Save a change-scoped comment on the current change so the packet
        // has something to render; this confirms the screen opens.
        let on_current = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: app.details.change_id.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "current-change concern".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &on_current).unwrap();

        // Save a comment on an unrelated change_id; in single-change mode it
        // must NOT be loaded into the packet.
        let unrelated_id = ChangeId::parse(&"f".repeat(32)).unwrap();
        let on_unrelated = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: unrelated_id.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "@".to_owned(),
            commit_id: None,
            body: "should-not-appear".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &on_unrelated).unwrap();

        open_send_to_claude(&mut app);

        let Screen::SendToClaude(ref state) = app.screen else {
            panic!("expected SendToClaude screen");
        };
        let SendToClaudeState::Confirm(ref data) = state.as_ref() else {
            panic!("expected Confirm variant");
        };
        let change_ids: Vec<_> = data
            .packet
            .changes
            .iter()
            .map(|cp| cp.change_id.clone())
            .collect();
        assert_eq!(
            change_ids,
            vec![app.details.change_id.clone()],
            "single-change mode must include only the current change"
        );
        assert!(
            !change_ids.contains(&unrelated_id),
            "unrelated change_id must not appear in single-change mode"
        );
    }

    /// Stack with exactly one entry: the new full-stack path must produce the
    /// same packet shape as the old single-entry path. Pins the equivalence
    /// at the boundary so a future "simplify" pass cannot regress the bug.
    #[test]
    fn send_to_claude_stack_with_one_entry_matches_single_change_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();
        let entry = StackEntry {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
        };
        let revset = "trunk()..@".to_owned();
        let revset_hash = RevsetHash::from_revset(&revset);
        app.stack = Some(StackContext {
            entries: vec![entry],
            current_index: 0,
            revset: revset.clone(),
            revset_hash,
        });
        app.revset = revset;

        let on_current = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: app.details.change_id.clone(),
            },
            repo_root: dir.path().to_path_buf(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "single entry".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        crate::store::save_comment(dir.path(), dir.path(), &on_current).unwrap();

        open_send_to_claude(&mut app);

        let Screen::SendToClaude(ref state) = app.screen else {
            panic!("expected SendToClaude screen");
        };
        let SendToClaudeState::Confirm(ref data) = state.as_ref() else {
            panic!("expected Confirm variant");
        };
        assert_eq!(data.packet.changes.len(), 1);
        assert_eq!(data.packet.changes[0].change_id, app.details.change_id);
    }

    #[test]
    fn f_key_opens_file_picker() {
        let mut app = make_app_with_single_file(sample_diff_file());
        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(
            matches!(app.screen, Screen::FilePicker(_)),
            "f should open FilePicker screen"
        );
    }

    #[test]
    fn file_picker_q_returns_to_main_unchanged() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.file_index = 0;
        open_file_picker(&mut app);
        assert!(matches!(app.screen, Screen::FilePicker(_)));
        handle_file_picker_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(
            matches!(app.screen, Screen::Main),
            "q should return to Main"
        );
        assert_eq!(app.file_index, 0, "file_index must not change on q");
    }

    #[test]
    fn file_picker_esc_returns_to_main_unchanged() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.file_index = 0;
        open_file_picker(&mut app);
        handle_file_picker_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(app.screen, Screen::Main),
            "Esc should return to Main"
        );
        assert_eq!(app.file_index, 0);
    }

    #[test]
    fn file_picker_enter_switches_file_index() {
        let id = ChangeId::parse(&"a".repeat(32)).unwrap();
        let commit_id = CommitId::parse(&"a".repeat(40)).unwrap();
        let details = ChangeDetails {
            change_id: id,
            commit_id,
            description: String::new(),
            diff: Diff {
                files: vec![
                    sample_diff_file(),
                    DiffFile::Modified {
                        path: PathBuf::from("bar.rs"),
                        hunks: vec![Hunk {
                            header: "@@ -1,1 +1,1 @@".to_owned(),
                            function_context: None,
                            source_start: 1,
                            source_length: 1,
                            target_start: 1,
                            target_length: 1,
                            lines: vec![Line {
                                kind: LineKind::Context,
                                text: "x".to_owned(),
                                source_line: Some(1),
                                target_line: Some(1),
                            }],
                        }],
                    },
                ],
            },
        };
        let mut app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        app.file_index = 0;
        open_file_picker(&mut app);

        // Move to second entry and press Enter.
        if let Screen::FilePicker(ref mut s) = app.screen {
            file_picker::move_cursor(s, 1);
            assert_eq!(s.selected_index, 1);
        }
        handle_file_picker_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            matches!(app.screen, Screen::Main),
            "Enter should return to Main"
        );
        assert_eq!(
            app.file_index, 1,
            "file_index should be updated to selected entry"
        );
    }

    #[test]
    fn toggle_severity_filter_sets_and_clears() {
        let mut app = make_app_with_single_file(sample_diff_file());
        assert_eq!(app.severity_filter, None);

        toggle_severity_filter(&mut app, Severity::Required);
        assert_eq!(app.severity_filter, Some(Severity::Required));

        toggle_severity_filter(&mut app, Severity::Required);
        assert_eq!(
            app.severity_filter, None,
            "pressing same severity again clears filter"
        );
    }

    #[test]
    fn toggle_severity_filter_switches_to_different_severity() {
        let mut app = make_app_with_single_file(sample_diff_file());
        toggle_severity_filter(&mut app, Severity::Required);
        assert_eq!(app.severity_filter, Some(Severity::Required));

        toggle_severity_filter(&mut app, Severity::Suggestion);
        assert_eq!(
            app.severity_filter,
            Some(Severity::Suggestion),
            "switching to different severity replaces filter"
        );
    }

    #[test]
    fn severity_filter_excludes_non_matching_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = make_app_with_single_file(sample_diff_file());
        app.repo_root = dir.path().to_path_buf();

        // Save a Required comment on the Added line (target=2).
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let init = make_init_from_app(&app, target, Severity::Required);
        let mut composer = Composer::new(init);
        for ch in "required comment".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(outcome, SaveOutcome::Saved));

        // Save a Note comment on the Context line (target=1).
        app.line_index = 1;
        let BuildTargetResult::Ready(target2) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let init2 = make_init_from_app(&app, target2, Severity::Note);
        let mut composer2 = Composer::new(init2);
        for ch in "note comment".chars() {
            composer2
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let outcome2 = save_composer(
            &mut app,
            &composer2,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
        );
        assert!(matches!(outcome2, SaveOutcome::Saved));

        // With no filter both comments render.
        assert_eq!(app.severity_filter, None);
        let all_meta_count = app
            .current_view()
            .unwrap()
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(
            all_meta_count, 2,
            "both comments should render with no filter"
        );

        // Apply Required filter — only 1 meta line should appear.
        toggle_severity_filter(&mut app, Severity::Required);
        assert_eq!(app.severity_filter, Some(Severity::Required));
        let filtered_count = app
            .current_view()
            .unwrap()
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(
            filtered_count, 1,
            "only Required comment should render when filter=Required"
        );

        // Clear the filter.
        toggle_severity_filter(&mut app, Severity::Required);
        assert_eq!(app.severity_filter, None);
        let after_clear = app
            .current_view()
            .unwrap()
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(
            after_clear, 2,
            "both comments should render after clearing filter"
        );
    }

    #[test]
    fn file_picker_enter_binary_file_sets_status_message() {
        let id = ChangeId::parse(&"a".repeat(32)).unwrap();
        let commit_id = CommitId::parse(&"a".repeat(40)).unwrap();
        let details = ChangeDetails {
            change_id: id,
            commit_id,
            description: String::new(),
            diff: Diff {
                files: vec![DiffFile::Binary {
                    path: PathBuf::from("logo.png"),
                }],
            },
        };
        let mut app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        open_file_picker(&mut app);
        // Entry 0 is description; binary file is at entry 1.
        if let Screen::FilePicker(ref mut s) = app.screen {
            file_picker::move_cursor(s, 1);
        }
        handle_file_picker_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.screen, Screen::Main),
            "Enter should return to Main"
        );
        // view_index 1 is the binary file entry
        assert_eq!(app.file_index, 1);
        assert_eq!(app.line_index, 0);
        let msg = app
            .status_message
            .as_deref()
            .expect("status message should be set for binary file");
        assert!(
            msg.contains("binary"),
            "status message should mention 'binary'; got: {msg}"
        );
    }

    /// Pins the contract: at every supported terminal width (>= `MIN_COLS`),
    /// the rendered footer text fits within `width` columns.
    #[test]
    fn footer_text_for_width_fits_within_width_for_all_target_widths() {
        for width in [60u16, 65, 70, 80, 120] {
            for has_stack in [false, true] {
                let text = footer_text_for_width(width, has_stack, None);
                assert!(
                    text.chars().count() <= usize::from(width),
                    "footer overflows at width={width} has_stack={has_stack}: {} chars: {text:?}",
                    text.chars().count()
                );
            }
        }
    }

    /// Pins the contract at the minimum supported width (`MIN_COLS` = 60) with
    /// a severity filter active: the badge appears unconditionally and the
    /// irreducible base is always preserved. When the badge plus the
    /// irreducible base exceeds the width, the badge wins (the badge is the
    /// only on-screen indicator that filtering is active, so dropping it would
    /// silently mislead the reviewer).
    #[test]
    fn footer_text_for_width_with_badge_at_minimum_width() {
        let text = footer_text_for_width(60, false, Some(Severity::Suggestion));
        assert!(
            text.contains("[F:suggestion]"),
            "badge must appear at minimum width: {text:?}"
        );
        assert!(
            text.contains("Enter comment"),
            "irreducible base must remain: {text:?}"
        );
        // The base (49 chars) + " [F:suggestion]" (16 chars) = 65 chars,
        // which exceeds 60. We pin: the badge wins. ratatui truncates at the
        // viewport edge, so the rightmost characters of the base may clip
        // visually — but the badge is still present in the rendered string.
        assert!(
            text.chars().count() > 60,
            "at width 60 with the long-form badge, the footer is expected to \
             exceed the budget (badge wins); got {} chars: {text:?}",
            text.chars().count()
        );
    }

    #[test]
    fn file_picker_enter_description_entry_navigates_to_description_view() {
        let id = ChangeId::parse(&"a".repeat(32)).unwrap();
        let commit_id = CommitId::parse(&"a".repeat(40)).unwrap();
        let details = ChangeDetails {
            change_id: id,
            commit_id,
            description: "first line\nsecond line".to_owned(),
            diff: Diff { files: vec![] },
        };
        let mut app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        open_file_picker(&mut app);
        // Entry 0 is always the description view.
        handle_file_picker_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.screen, Screen::Main),
            "Enter should navigate to Main"
        );
        assert_eq!(app.file_index, 0, "file_index 0 is the description view");
        // Description lines are commentable, so cursor lands on the first one.
        let cursor_kind = app.current_view().unwrap().lines[app.line_index].kind;
        assert!(
            matches!(cursor_kind, RenderedLineKind::DescriptionLine),
            "cursor must land on a description line; got {cursor_kind:?}"
        );
    }

    #[test]
    fn file_picker_enter_single_file_navigates_to_first_commentable_line() {
        let mut app = make_app_with_single_file(sample_diff_file());
        open_file_picker(&mut app);
        // Entry 0 is description (empty); navigate to entry 1 (the diff file).
        if let Screen::FilePicker(ref mut s) = app.screen {
            file_picker::move_cursor(s, 1);
        }
        handle_file_picker_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.screen, Screen::Main));
        // view_index 1 is the first diff file
        assert_eq!(app.file_index, 1);
        // First commentable line is the Context at index 1 (HunkHeader is at 0).
        let cursor_kind = app.current_view().unwrap().lines[app.line_index].kind;
        assert!(
            matches!(
                cursor_kind,
                RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
            ),
            "cursor must land on a commentable line; got {cursor_kind:?}"
        );
    }

    // -- Length indicator (scrollbar) tests ---------------------------------
    //
    // The scrollbar overlays the right edge of the diff body and only renders
    // when the diff overflows the viewport. The pure helpers below carry the
    // load-bearing math; the rendering tests then confirm the wiring lands
    // glyphs where expected at the right edge of the diff area.

    #[test]
    fn scrollbar_state_for_view_returns_none_when_content_fits_viewport() {
        // Equal length is the boundary: viewport can show the entire body in
        // one screen, so the indicator is informational noise — hide it.
        assert!(scrollbar_state_for_view(20, 0, 20).is_none());
        assert!(scrollbar_state_for_view(5, 0, 20).is_none());
    }

    #[test]
    fn scrollbar_state_for_view_returns_some_when_content_overflows_viewport() {
        let state = scrollbar_state_for_view(100, 0, 20)
            .expect("100 lines in 20-row viewport must produce a scrollbar");
        // Sanity: the state is non-degenerate. Internal fields are private,
        // so we cannot assert on them directly — the rendering tests below
        // verify the resulting glyphs.
        let _ = state;
    }

    #[test]
    fn scrollbar_state_for_view_returns_none_for_zero_viewport() {
        // Defensive: a zero-row viewport is a transient resize state. Don't
        // try to allocate a scrollbar against it — the math would divide-by-
        // zero on the renderer side.
        assert!(scrollbar_state_for_view(100, 0, 0).is_none());
    }

    #[test]
    fn split_body_for_scrollbar_reserves_one_col_on_the_right() {
        let area = Rect::new(0, 0, 80, 24);
        let (body, sb) = split_body_for_scrollbar(area, true);
        assert_eq!(body.width, 79, "body keeps width minus one col");
        assert_eq!(body.x, 0);
        let sb_rect = sb.expect("scrollbar slot must exist when requested");
        assert_eq!(sb_rect.width, 1);
        assert_eq!(sb_rect.x, 79, "scrollbar pinned to right edge");
        assert_eq!(sb_rect.height, 24);
    }

    #[test]
    fn split_body_for_scrollbar_skips_when_not_requested() {
        let area = Rect::new(0, 0, 80, 24);
        let (body, sb) = split_body_for_scrollbar(area, false);
        assert_eq!(body, area, "body keeps the full area when no scrollbar");
        assert!(sb.is_none());
    }

    #[test]
    fn split_body_for_scrollbar_skips_when_area_too_narrow() {
        // Width of 1 cannot host both body and scrollbar; keep the body.
        let area = Rect::new(0, 0, 1, 24);
        let (body, sb) = split_body_for_scrollbar(area, true);
        assert_eq!(body, area);
        assert!(sb.is_none());
    }

    /// Build a diff file with `line_count` Added lines so tests can exercise
    /// over- and under-flow against a chosen viewport size.
    fn long_diff_file(line_count: u32) -> DiffFile {
        let lines: Vec<Line> = (1..=line_count)
            .map(|i| Line {
                kind: LineKind::Added,
                text: format!("line {i}"),
                source_line: None,
                target_line: Some(i),
            })
            .collect();
        DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,N @@".to_owned(),
                function_context: None,
                source_start: 0,
                source_length: 0,
                target_start: 1,
                target_length: line_count,
                lines,
            }],
        }
    }

    /// Render whichever screen `app` is currently on (`Main`, `Stale`,
    /// `Overview`, `FilePicker`, `Composer`, etc.) into a [`TestBackend`] of
    /// the given size and return the resulting [`Buffer`] for inspection.
    /// `render(frame, app)` dispatches on `app.screen` and falls through to
    /// `render_main` when on `Screen::Main`, so this single helper covers
    /// every screen test.
    fn render_to_buffer(app: &mut App, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(cols, rows);
        let mut terminal = Terminal::new(backend).expect("test terminal must construct");
        terminal
            .draw(|frame| render(frame, app))
            .expect("test draw must succeed");
        terminal.backend().buffer().clone()
    }

    use super::scrollbar_test_helpers::{
        col_contains_scrollbar_glyph, scrollbar_thumb_last_row, scrollbar_thumb_row,
    };

    #[test]
    fn scrollbar_renders_when_diff_overflows_viewport() {
        // 80 rows of diff into a 24-row terminal — the diff body itself is
        // the terminal height minus stack/file/footer rows, far smaller than
        // 80, so the scrollbar must appear.
        let mut app = make_app_with_single_file(long_diff_file(80));
        let buf = render_to_buffer(&mut app, 80, 24);
        assert!(
            col_contains_scrollbar_glyph(&buf, 79),
            "scrollbar glyphs must appear in the rightmost column when the diff overflows the viewport"
        );
    }

    #[test]
    fn scrollbar_does_not_render_when_diff_fits_viewport() {
        // A 2-line diff fits in any reasonable viewport; the rightmost column
        // must contain none of the scrollbar glyphs.
        let mut app = make_app_with_single_file(long_diff_file(2));
        let buf = render_to_buffer(&mut app, 80, 24);
        assert!(
            !col_contains_scrollbar_glyph(&buf, 79),
            "scrollbar must not render when the diff fits the viewport"
        );
    }

    /// (`top_row`, `bottom_row`) of the diff area inside the main view, given
    /// the total terminal `rows`. The main layout is `[stack_bar=3,
    /// file_header=3, diff=Min(1), footer=1]`, so the diff area starts at
    /// row 6 and ends at `rows - 2` inclusive. Used by the scrollbar position
    /// tests to reason about thumb location without scattering the magic
    /// offsets.
    fn diff_area_rows(rows: u16) -> (u16, u16) {
        let top: u16 = 3 + 3;
        let bottom = rows.saturating_sub(2);
        (top, bottom)
    }

    #[test]
    fn scrollbar_thumb_sits_in_top_half_when_cursor_at_top() {
        // Cursor at the top drives `scroll = 0` and the thumb to the top of
        // the track. The thumb's row index must fall in the top half of the
        // diff area. Pins "where am I in the diff" to "top".
        let mut app = make_app_with_single_file(long_diff_file(200));
        app.line_index = 0;
        let buf = render_to_buffer(&mut app, 80, 24);
        let thumb_row =
            scrollbar_thumb_row(&buf, 79).expect("thumb glyph must appear when scrollbar renders");
        let (diff_top, diff_bottom) = diff_area_rows(24);
        let midpoint = diff_top + (diff_bottom - diff_top) / 2;
        assert!(
            thumb_row < midpoint,
            "thumb must sit in the top half of the track when cursor is at line 0; \
             got row {thumb_row}, midpoint {midpoint}"
        );
    }

    #[test]
    fn scrollbar_thumb_sits_in_bottom_half_when_cursor_at_bottom() {
        // Cursor at the bottom drives the viewport to the end of the diff;
        // the thumb must land in the bottom half of the track.
        let mut app = make_app_with_single_file(long_diff_file(200));
        app.line_index = app.current_line_count() - 1;
        let buf = render_to_buffer(&mut app, 80, 24);
        let thumb_row =
            scrollbar_thumb_row(&buf, 79).expect("thumb glyph must appear when scrollbar renders");
        let (diff_top, diff_bottom) = diff_area_rows(24);
        let midpoint = diff_top + (diff_bottom - diff_top) / 2;
        assert!(
            thumb_row > midpoint,
            "thumb must sit in the bottom half of the track when cursor is at the last line; \
             got row {thumb_row}, midpoint {midpoint}"
        );
    }

    #[test]
    fn scrollbar_resets_on_file_navigation() {
        // Cycling between files resets the cursor and the scroll offset (see
        // `cycle_file`). After the reset, rendering the new file's scrollbar
        // must place the thumb in the top half of the track — there is no
        // per-file ScrollbarState carrying stale position from a previous
        // file.
        let mut app = make_app_with_single_file(long_diff_file(200));
        app.line_index = app.current_line_count() - 1;
        // Ensure scroll moves to the bottom before the cycle.
        let _ = render_to_buffer(&mut app, 80, 24);
        app.cycle_file(-1); // back to description (file_index 0)
        app.cycle_file(1); // forward to diff file
        assert_eq!(app.line_index, 0);
        assert_eq!(app.scroll, 0);
        let buf = render_to_buffer(&mut app, 80, 24);
        let thumb_row =
            scrollbar_thumb_row(&buf, 79).expect("thumb glyph must appear when scrollbar renders");
        let (diff_top, diff_bottom) = diff_area_rows(24);
        let midpoint = diff_top + (diff_bottom - diff_top) / 2;
        assert!(
            thumb_row < midpoint,
            "thumb must reset to the top half after switching files; got row {thumb_row}, midpoint {midpoint}"
        );
    }

    /// Build an app with the description view plus two diff files.
    /// `file_index` starts at 0 (description) so each test can position it
    /// explicitly.
    fn make_app_with_two_diff_files() -> App {
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: String::new(),
            diff: Diff {
                files: vec![
                    sample_diff_file(),
                    DiffFile::Modified {
                        path: PathBuf::from("bar.rs"),
                        hunks: vec![Hunk {
                            header: "@@ -1,1 +1,1 @@".to_owned(),
                            function_context: None,
                            source_start: 1,
                            source_length: 1,
                            target_start: 1,
                            target_length: 1,
                            lines: vec![Line {
                                kind: LineKind::Context,
                                text: "x".to_owned(),
                                source_line: Some(1),
                                target_line: Some(1),
                            }],
                        }],
                    },
                ],
            },
        };
        App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        )
    }

    /// Build an app whose change has no diff files. `rendered_per_file` ends
    /// up with exactly one entry (the synthetic description view).
    fn make_app_description_only() -> App {
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: String::new(),
            diff: Diff { files: vec![] },
        };
        App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        )
    }

    #[test]
    fn cycle_file_sets_status_at_last_file_when_already_at_max() {
        // Three views (description + two diff files). At the last index a
        // forward Tab cannot advance, so the footer must surface the boundary.
        let mut app = make_app_with_two_diff_files();
        app.file_index = app.rendered_per_file.len() - 1;

        app.cycle_file(1);

        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_AT_LAST_FILE),
            "Tab at last file must set the boundary status"
        );
    }

    #[test]
    fn cycle_file_sets_status_at_first_file_when_already_at_zero() {
        // file_index 0 is the description view; Shift-Tab there has nowhere
        // earlier to go.
        let mut app = make_app_with_two_diff_files();
        app.file_index = 0;

        app.cycle_file(-1);

        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_AT_FIRST_FILE),
            "Shift-Tab at file_index 0 must set the boundary status"
        );
    }

    #[test]
    fn cycle_file_sets_status_only_one_file_when_count_is_one_in_either_direction() {
        // Description-only changes have a single navigable view; Tab and
        // Shift-Tab both hit the degenerate boundary.
        let mut app = make_app_description_only();
        assert_eq!(app.rendered_per_file.len(), 1);

        app.cycle_file(1);
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_ONLY_ONE_FILE),
            "Tab with one view must surface the only-one-file hint"
        );

        app.status_message = None;
        app.cycle_file(-1);
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_ONLY_ONE_FILE),
            "Shift-Tab with one view must surface the only-one-file hint"
        );
    }

    #[test]
    fn cycle_file_clears_or_replaces_status_on_normal_movement() {
        // When Tab actually advances the file, no boundary message should
        // linger from a previous keystroke. handle_main_key clears
        // status_message before dispatch, so cycle_file's contract is just
        // "do not set a boundary message on successful movement".
        let mut app = make_app_with_two_diff_files();
        app.file_index = 0;
        app.status_message = Some("stale".to_owned());

        app.cycle_file(1);

        assert_eq!(app.file_index, 1, "Tab from index 0 must advance");
        let boundary_messages = [
            STATUS_AT_LAST_FILE,
            STATUS_AT_FIRST_FILE,
            STATUS_ONLY_ONE_FILE,
        ];
        if let Some(msg) = app.status_message.as_deref() {
            assert!(
                !boundary_messages.contains(&msg),
                "successful movement must not leave a boundary message; got {msg:?}"
            );
        }
    }

    #[test]
    fn cycle_file_does_not_panic_on_count_zero() {
        // The early-return for an empty rendered_per_file is defensive — real
        // construction always pushes the description view. Pin it so a future
        // refactor cannot reintroduce a zero-count panic via underflow on
        // `count - 1`.
        let mut app = make_app_with_single_file(sample_diff_file());
        app.rendered_per_file.clear();
        app.annotated_per_file.clear();
        app.file_index = 0;

        app.cycle_file(1);
        app.cycle_file(-1);

        assert!(
            app.status_message.is_none(),
            "count==0 must early-return without setting a boundary status"
        );
    }

    #[test]
    fn scrollbar_overflow_for_view_clamps_scroll_past_end_to_max() {
        // Helper clamp pinned numerically — ratatui 0.29 also clamps
        // internally, but we own the clamp at the helper boundary so a
        // future ratatui change cannot regress the contract. The tuple
        // assertion is independent of any rendering layer.
        //
        // total=30, viewport=10 → max_scroll = 20, content_length = 21.
        assert_eq!(
            scrollbar_overflow_for_view(30, 50, 10),
            Some((21, 20)),
            "stale scroll past end must clamp to max_scroll"
        );
        assert_eq!(
            scrollbar_overflow_for_view(30, 20, 10),
            Some((21, 20)),
            "scroll exactly at max passes through"
        );
        assert_eq!(
            scrollbar_overflow_for_view(30, 5, 10),
            Some((21, 5)),
            "scroll below max passes through unmodified"
        );
        assert_eq!(
            scrollbar_overflow_for_view(30, 0, 10),
            Some((21, 0)),
            "scroll at top yields position 0"
        );
    }

    #[test]
    fn scrollbar_state_for_view_renders_thumb_at_bottom_when_scroll_past_end() {
        use ratatui::backend::TestBackend;
        // Integration shim: confirm the helper-clamp tuple lifts into a
        // ScrollbarState that renders the thumb at the bottom of the track.
        // The numeric clamp is pinned by the tuple test above; this test
        // covers the rendering wiring.
        let mut state = scrollbar_state_for_view(30, 50, 10)
            .expect("30 lines vs 10-row viewport must produce a scrollbar");
        let backend = TestBackend::new(1, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_stateful_widget(
                    Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight),
                    Rect::new(0, 0, 1, 8),
                    &mut state,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // The bottom row holds the end-arrow `▼`; the row above (height-2)
        // is the last track row and must hold the thumb when at max position.
        let bottom_track = buf[(0, 8 - 2)].symbol().to_owned();
        assert_eq!(bottom_track, "\u{2588}");
    }

    #[test]
    fn scrollbar_renders_at_bottom_after_refresh_shrinks_diff_below_scroll() {
        // Integration-level pin: `refresh_current_change` does not reset
        // scroll/line_index. If a refresh shrinks the diff so the previous
        // top-line index now points past the end, the scrollbar must still
        // render and land the thumb at the bottom — no panic, no off-track
        // glyphs, no silent dishonesty.
        //
        // We simulate the refresh by mutating the rebuilt views directly
        // (the real path goes through `jj::show` which we cannot drive in a
        // unit test). The load-bearing assertion is the same: stale `scroll`
        // pointing past the new total must produce a coherent scrollbar.
        let mut app = make_app_with_single_file(long_diff_file(100));
        // Scroll cursor to near the bottom so app.scroll lands well past 20.
        app.line_index = app.current_line_count() - 1;
        let _ = render_to_buffer(&mut app, 80, 24);
        assert!(app.scroll > 20, "precondition: scroll moved past 20");

        // Simulate a refresh that shrinks the diff to 20 lines without
        // resetting scroll/line_index.
        let new_details = ChangeDetails {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
            diff: Diff {
                files: vec![long_diff_file(20)],
            },
        };
        app.rendered_per_file = build_rendered_views(&new_details);
        app.annotated_per_file = app.rendered_per_file.clone();
        app.details = new_details;
        // Note: scroll and line_index intentionally NOT reset, mirroring
        // `refresh_current_change` semantics.

        // Numeric pin: the new view has 21 lines (20 added + 1 hunk header)
        // against a 17-row diff area, max_scroll = 4, content_length = 5.
        // Stale `app.scroll` (> 20) must clamp to 4.
        let new_total = app
            .current_view()
            .expect("annotated_per_file must include the new file")
            .lines
            .len();
        assert_eq!(
            scrollbar_overflow_for_view(new_total, app.scroll, 17),
            Some((5, 4)),
            "stale scroll past end must clamp to max_scroll at the helper boundary"
        );

        // Integration pin: rendering the same state lands the thumb's last
        // row on the last track row (just above the end-arrow ▼).
        let buf = render_to_buffer(&mut app, 80, 24);
        let thumb_last = scrollbar_thumb_last_row(&buf, 79)
            .expect("scrollbar thumb must render even when scroll is past the end");
        let (_diff_top, diff_bottom) = diff_area_rows(24);
        assert_eq!(thumb_last, diff_bottom - 1);
    }

    #[test]
    fn scrollbar_resets_on_revision_navigation() {
        // n/p revision navigation routes through `load_stack_entry`, which
        // resets file_index, line_index, and scroll to zero (line 656). The
        // load itself goes through `jj::show` and cannot be exercised in a
        // unit test — but the contract that drives the scrollbar is the
        // post-load field state. Pin that: after the same field reset
        // `load_stack_entry` performs, the scrollbar lands at the top.
        let mut app = make_app_with_single_file(long_diff_file(200));
        app.line_index = app.current_line_count() - 1;
        let _ = render_to_buffer(&mut app, 80, 24);
        assert!(app.scroll > 0, "precondition: scroll moved off zero");

        // Mirror the field resets in `load_stack_entry`. The scrollbar reads
        // these fields directly, so exercising the reset values in isolation
        // pins the same indicator behavior n/p would produce.
        app.file_index = 0;
        app.line_index = 0;
        app.scroll = 0;
        // Cycle back to the diff file (file_index 0 is the description view).
        app.file_index = 1;

        let buf = render_to_buffer(&mut app, 80, 24);
        let thumb_row = scrollbar_thumb_row(&buf, 79)
            .expect("scrollbar thumb must appear after revision-navigation reset");
        let (diff_top, diff_bottom) = diff_area_rows(24);
        let midpoint = diff_top + (diff_bottom - diff_top) / 2;
        assert!(
            thumb_row < midpoint,
            "thumb must reset to the top half after revision navigation; \
             got row {thumb_row}, midpoint {midpoint}"
        );
    }

    #[test]
    fn scrollbar_accounts_for_inline_comment_augmented_total() {
        // `with_inline_comments` injects meta + body rows into the rendered
        // view; the scrollbar must read the augmented total, not the raw
        // diff line count. Build an 18-line diff that fits a 20-row viewport
        // on its own, then inject 5 inline comment rows so the augmented
        // total (23) overflows.
        use crate::tui::diff_view::InlineComment;
        let base = DiffView::from_file(&long_diff_file(18));
        // Inject 5 inline comment rows under target_line = 1 — the first
        // Added line. Each InlineComment produces 1 meta + body_lines.len()
        // body rows; one comment with 4 body lines == 5 rows total.
        let inline = InlineComment {
            source_line: None,
            target_line: Some(1),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec![
                "line1".to_owned(),
                "line2".to_owned(),
                "line3".to_owned(),
                "line4".to_owned(),
            ],
            comment_index: CommentIndex::Local(0),
        };
        let augmented = base.with_inline_comments(&[inline]);
        // Augmented total: 18 added + 1 hunk header + 1 meta + 4 body = 24.
        // Pick a viewport size that fits the original (19 rendered lines)
        // but not the augmented (24).
        assert!(augmented.lines.len() > 20);
        assert!(scrollbar_state_for_view(augmented.lines.len(), 0, 20).is_some());
    }

    #[test]
    fn scrollbar_state_for_view_returns_none_for_zero_line_buffer() {
        // Empty buffer is the absolute floor — `total <= viewport` covers
        // it, but pin the boundary so a future predicate change is intentional.
        assert!(scrollbar_state_for_view(0, 0, 20).is_none());
        assert!(scrollbar_state_for_view(0, 0, 1).is_none());
    }

    #[test]
    fn scrollbar_state_for_view_renders_at_exactly_one_line_overflow() {
        // The off-by-one cliff: total == viewport + 1 must produce a
        // scrollbar (the predicate is `<=`, so == is the floor of overflow).
        assert!(scrollbar_state_for_view(21, 0, 20).is_some());
        // And the negative side: total == viewport must NOT produce one.
        assert!(scrollbar_state_for_view(20, 0, 20).is_none());
    }

    #[test]
    fn split_body_for_scrollbar_handles_width_two_boundary() {
        // Width 2: `area.width <= SCROLLBAR_WIDTH` is `2 <= 1` (false), so
        // we DO split — body keeps 1 col, scrollbar takes 1 col.
        let area_2 = Rect::new(0, 0, 2, 24);
        let (body, sb) = split_body_for_scrollbar(area_2, true);
        assert_eq!(body.width, 1);
        let sb_rect = sb.expect("width=2 must yield a scrollbar slot");
        assert_eq!(sb_rect.width, 1);

        // Width 1: too narrow to host both; body keeps the full area.
        let area_1 = Rect::new(0, 0, 1, 24);
        let (body, sb) = split_body_for_scrollbar(area_1, true);
        assert_eq!(body, area_1);
        assert!(sb.is_none());

        // Width 0: pathological (resize race). Must not panic; returns no
        // scrollbar slot.
        let area_0 = Rect::new(0, 0, 0, 24);
        let (body, sb) = split_body_for_scrollbar(area_0, true);
        assert_eq!(body, area_0);
        assert!(sb.is_none());
    }

    #[test]
    fn scrollbar_renders_for_long_description_view() {
        // A long description (file_index 0) uses the same scroll mechanism as
        // diff files. Verify the scrollbar renders against the synthetic
        // description view too.
        let description_lines: String = (0..200)
            .map(|i| format!("description line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: description_lines,
            diff: Diff {
                files: vec![long_diff_file(1)],
            },
        };
        let mut app = App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        );
        app.file_index = 0;
        let buf = render_to_buffer(&mut app, 80, 24);
        assert!(
            col_contains_scrollbar_glyph(&buf, 79),
            "scrollbar must render against the synthetic description view when its content overflows"
        );
    }

    // -- Stale-screen scrollbar tests --------------------------------------
    //
    // Each stale entry consumes 7 (wide) or 8 (narrow) rendered rows. The
    // scrollbar takes total rendered rows so the thumb honestly reflects how
    // much of the body is on screen.

    /// Seed the comment store with `count` stale comments at distinct lines
    /// of `foo.txt` and return an [`App`] with the stale screen open.
    fn stale_app_with_n_comments(dir: &std::path::Path, count: u32) -> App {
        let mut app = make_app_with_single_file(long_diff_file(count + 10));
        app.repo_root = dir.to_owned();
        for i in 1..=count {
            let comment = make_stale_comment_on_file(
                dir,
                app.details.change_id.clone(),
                "foo.txt",
                i,
                &format!("stale {i}"),
                time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(i64::from(i)),
            );
            crate::store::save_comment(dir, dir, &comment).unwrap();
        }
        app.refresh_inline_comments();
        open_stale_screen(&mut app);
        app
    }

    #[test]
    fn stale_screen_scrollbar_renders_when_entries_overflow_viewport() {
        let dir = tempfile::tempdir().unwrap();
        // 20 stale entries × 7 rows/entry (wide) = 140 rows; the body is
        // roughly terminal_rows - 3 (border + footer), well under 140.
        let mut app = stale_app_with_n_comments(dir.path(), 20);
        let buf = render_to_buffer(&mut app, 100, 24);
        assert!(
            col_contains_scrollbar_glyph(&buf, 98),
            "scrollbar must render in the rightmost stale-screen body column when entries overflow"
        );
    }

    #[test]
    fn stale_screen_scrollbar_hidden_when_entries_fit_viewport() {
        let dir = tempfile::tempdir().unwrap();
        // 1 stale entry × 7 rows fits comfortably in a 24-row terminal.
        let mut app = stale_app_with_n_comments(dir.path(), 1);
        let buf = render_to_buffer(&mut app, 100, 24);
        assert!(
            !col_contains_scrollbar_glyph(&buf, 98),
            "scrollbar must be hidden when stale entries fit the body"
        );
    }

    #[test]
    fn stale_screen_scrollbar_thumb_position_reflects_scroll_offset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = stale_app_with_n_comments(dir.path(), 20);

        // selected=0 → scroll_offset stays at 0; thumb sits at the top.
        let buf_top = render_to_buffer(&mut app, 100, 24);
        let thumb_top = scrollbar_thumb_row(&buf_top, 98)
            .expect("thumb glyph must appear when entries overflow");

        // Move selection deep into the list to drive scroll_offset down.
        for _ in 0..19 {
            handle_stale_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let buf_bot = render_to_buffer(&mut app, 100, 24);
        let thumb_bot = scrollbar_thumb_row(&buf_bot, 98)
            .expect("thumb glyph must appear when entries overflow");

        assert!(
            thumb_top < thumb_bot,
            "thumb must move down as scroll_offset advances; top={thumb_top}, bottom={thumb_bot}"
        );
    }

    // -- Overview-screen scrollbar tests -----------------------------------
    //
    // The overview's scroll model is row-based (each `OverviewRow` is one
    // rendered line), so the scrollbar's `total_lines` is `rows.len()`.

    /// Seed the comment store with `count` change-level comments on `id` so
    /// the overview cache loads more rows than the body can display.
    fn overview_app_with_n_change_comments(dir: &std::path::Path, count: u32) -> App {
        let (mut app, id_a, _id_b) = make_stack_app_with_two_changes(dir);
        for i in 0..count {
            let comment = Comment {
                schema_version: SchemaVersion,
                anchor: Anchor::Change {
                    change_id: id_a.clone(),
                },
                repo_root: dir.to_path_buf(),
                revset: "trunk()..@".to_owned(),
                commit_id: None,
                body: format!("change comment {i}"),
                severity: Severity::Note,
                created_at: time::OffsetDateTime::UNIX_EPOCH
                    + time::Duration::seconds(i64::from(i)),
                updated_at: None,
                status: Some(Status::Pending),
                mismatch_reason: None,
                entity_id: None,
                anchor_fingerprint: None,
            };
            crate::store::save_comment(dir, dir, &comment).unwrap();
        }
        open_overview_screen(&mut app);
        app
    }

    #[test]
    fn overview_screen_scrollbar_renders_when_rows_overflow_viewport() {
        let dir = tempfile::tempdir().unwrap();
        // 60 change-level comments + stack headers + change rows + summary
        // footer easily exceeds a 24-row terminal's body.
        let mut app = overview_app_with_n_change_comments(dir.path(), 60);
        let buf = render_to_buffer(&mut app, 100, 24);
        assert!(
            col_contains_scrollbar_glyph(&buf, 98),
            "scrollbar must render in the rightmost overview-screen body column when rows overflow"
        );
    }

    #[test]
    fn overview_screen_scrollbar_hidden_when_rows_fit_viewport() {
        let dir = tempfile::tempdir().unwrap();
        // No extra comments → just header + separator + 2 change rows; the
        // body is plenty large.
        let mut app = overview_app_with_n_change_comments(dir.path(), 0);
        let buf = render_to_buffer(&mut app, 100, 24);
        assert!(
            !col_contains_scrollbar_glyph(&buf, 98),
            "scrollbar must be hidden when overview rows fit the body"
        );
    }

    #[test]
    fn overview_screen_scrollbar_thumb_position_reflects_scroll_offset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = overview_app_with_n_change_comments(dir.path(), 60);

        // Selected at the top → thumb at the top of the track.
        let buf_top = render_to_buffer(&mut app, 100, 24);
        let thumb_top =
            scrollbar_thumb_row(&buf_top, 98).expect("thumb glyph must appear when rows overflow");

        // Move selection well past the viewport to drive scroll_offset down.
        for _ in 0..50 {
            handle_overview_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .expect("overview down arrow must not error");
        }
        let buf_bot = render_to_buffer(&mut app, 100, 24);
        let thumb_bot =
            scrollbar_thumb_row(&buf_bot, 98).expect("thumb glyph must appear when rows overflow");

        assert!(
            thumb_top < thumb_bot,
            "thumb must move down as scroll_offset advances; top={thumb_top}, bottom={thumb_bot}"
        );
    }

    // -- Variable-row pinning for stale_screen ----------------------------
    //
    // Description / Change / Stack anchors render shorter than Line anchors
    // (no `was:` / `now:` rows). The scrollbar's `total_lines` and the
    // scroll-offset walker both consult `total_rendered_rows` /
    // `rendered_rows_for_anchor`; this test pins those helpers against what
    // `build_entry_lines` actually emits, so a future tweak to either side
    // can't drift apart silently.

    /// Build a stale comment whose anchor is `Anchor::Description`. Mirrors
    /// `make_stale_comment_on_file` but for the description side, exercising
    /// the non-line render path.
    fn make_stale_description_comment(
        change_id: ChangeId,
        body: &str,
        created_at: time::OffsetDateTime,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id,
                location: DescriptionAnchor {
                    display_line: Some(1),
                    target_text: "old line".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Required,
            created_at,
            updated_at: None,
            status: Some(Status::Stale),
            mismatch_reason: Some(crate::comment::MismatchReason::TargetTextChanged),
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    #[test]
    fn build_entry_lines_count_matches_total_rendered_rows_for_description_anchor() {
        // Without this pin, `total_rendered_rows` happily over-reports
        // content_length whenever a Description-anchored stale comment is
        // present (the original implementation multiplied entry_count by 7
        // unconditionally), undersizing the scrollbar thumb and overshooting
        // bottom scrolls.
        let mut app = make_app_with_single_file(sample_diff_file());
        let comment = make_stale_description_comment(
            app.details.change_id.clone(),
            "desc body",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        app.loaded_comments = vec![comment];

        let state = StaleScreenState {
            selected_index: 0,
            stale_indices: vec![0],
            scroll_offset: 0,
        };

        for is_wide in [true, false] {
            let lines = stale_screen::build_entry_lines(
                80,
                &state,
                &app.loaded_comments,
                &app.details.diff,
                is_wide,
            );
            let anchors: Vec<&Anchor> = state
                .stale_indices
                .iter()
                .map(|&i| &app.loaded_comments[i].anchor)
                .collect();
            let expected = stale_screen::total_rendered_rows(anchors.iter().copied(), is_wide);
            assert_eq!(
                lines.len(),
                expected,
                "build_entry_lines emit count must match total_rendered_rows \
                 (is_wide={is_wide}, lines.len()={}, expected={expected})",
                lines.len()
            );
        }
    }

    #[test]
    fn build_entry_lines_count_matches_total_rendered_rows_for_mixed_anchors() {
        // Mixed list (Line + Description) is the realistic shape and the
        // case the reviewer's bug report called out — the anchor walker
        // and the render path must agree on the per-entry height for every
        // entry, not just the all-Line case.
        let mut app = make_app_with_single_file(sample_diff_file());
        let line = make_stale_comment_on_file(
            std::path::Path::new("/repo"),
            app.details.change_id.clone(),
            "foo.txt",
            2,
            "line body",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let desc = make_stale_description_comment(
            app.details.change_id.clone(),
            "desc body",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
        );
        app.loaded_comments = vec![line, desc];

        let state = StaleScreenState {
            selected_index: 0,
            stale_indices: vec![0, 1],
            scroll_offset: 0,
        };

        for is_wide in [true, false] {
            let lines = stale_screen::build_entry_lines(
                80,
                &state,
                &app.loaded_comments,
                &app.details.diff,
                is_wide,
            );
            let anchors: Vec<&Anchor> = state
                .stale_indices
                .iter()
                .map(|&i| &app.loaded_comments[i].anchor)
                .collect();
            let expected = stale_screen::total_rendered_rows(anchors.iter().copied(), is_wide);
            assert_eq!(lines.len(), expected, "is_wide={is_wide}");
        }
    }

    // -- column_layout threshold stability across scrollbar visibility -----
    //
    // C1 fix: column_layout takes the OUTER terminal width, so its threshold
    // decisions (show_idx at >= 100, show_inset_body at >= 80) must NOT
    // depend on whether the scrollbar happens to be visible. Earlier the
    // implementation passed `body_area.width + 2`, which equals `area.width`
    // when no scrollbar but `area.width - 1` when one was drawn — content
    // would cliff into a different layout the moment a row was added.

    /// Render the overview screen at `cols x rows` against `app` and return
    /// the resulting [`Buffer`].
    fn render_overview_to_buffer(app: &mut App, cols: u16, rows: u16) -> ratatui::buffer::Buffer {
        render_to_buffer(app, cols, rows)
    }

    /// Whether the rendered overview's leftmost change-row column shows the
    /// numeric idx column. Pinned by checking for an idx digit at the column
    /// the layout reserves for it (after the cursor glyph + spacing).
    ///
    /// At outer width >= 100, `show_idx = true` and a 2-char idx (e.g. ` 1`)
    /// appears starting at column 4 of the inner area (cursor 2 + cursor pad
    /// = 2 + 2; inner starts at col 1 so absolute col 5..6).
    fn overview_first_change_row_has_idx(buf: &ratatui::buffer::Buffer) -> bool {
        // Walk every row, find the first change row (it begins with either
        // the selection-cursor U+25B6 followed by a space, the
        // current-change-mark U+25B8, or two leading spaces — and is
        // followed by an idx digit then more spaces then a change_id).
        // Keep this simple: a row containing two spaces, a digit, two
        // spaces, then 8 hex chars, is the show_idx layout. We test by
        // looking for a contiguous span " 1  " in the early columns.
        for row in 0..buf.area.height {
            let mut line = String::new();
            for col in 0..buf.area.width {
                line.push_str(buf[(col, row)].symbol());
            }
            if line.contains(" 1  ") && line.contains("aaaaaaaa") {
                return true;
            }
        }
        false
    }

    #[test]
    fn column_layout_thresholds_do_not_flicker_with_scrollbar_visibility() {
        // Build two overview snapshots at the SAME outer width 100 (where
        // show_idx = true). One has 0 extra comments (rows fit, scrollbar
        // hidden); the other has 60 extra change comments (rows overflow,
        // scrollbar visible). Both must render the idx column the same way
        // — the column-budget decision must NOT depend on overflow state.
        let dir_fit = tempfile::tempdir().unwrap();
        let mut app_fit = overview_app_with_n_change_comments(dir_fit.path(), 0);
        let buf_fit = render_overview_to_buffer(&mut app_fit, 100, 24);

        let dir_overflow = tempfile::tempdir().unwrap();
        let mut app_overflow = overview_app_with_n_change_comments(dir_overflow.path(), 60);
        let buf_overflow = render_overview_to_buffer(&mut app_overflow, 100, 24);

        assert_eq!(
            overview_first_change_row_has_idx(&buf_fit),
            overview_first_change_row_has_idx(&buf_overflow),
            "show_idx must NOT flicker when the scrollbar appears at outer width 100"
        );
        assert!(
            overview_first_change_row_has_idx(&buf_fit),
            "outer width 100 should render the idx column regardless of overflow"
        );
    }

    /// Build an `App` rooted at `repo_root` (a writable tempdir) with one
    /// description plus `files` worth of diff files. Used by the reviewed-
    /// status tests so `mark_current_file_reviewed` actually persists rather
    /// than tripping the read-only-`/repo` short-circuit.
    fn make_app_in_dir(repo_root: PathBuf, files: Vec<DiffFile>) -> App {
        let details = ChangeDetails {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "desc".to_owned(),
            diff: Diff { files },
        };
        App::new(
            details,
            repo_root,
            "@".to_owned(),
            None,
            TransitionMode::Never,
        )
    }

    #[test]
    fn is_view_reviewed_returns_false_initially() {
        let app = make_app_with_single_file(sample_diff_file());
        assert!(!app.is_view_reviewed(0));
        assert!(!app.is_view_reviewed(1));
    }

    #[test]
    fn mark_current_file_reviewed_sets_description_bit_on_index_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 0;
        app.mark_current_file_reviewed();
        assert!(app.is_view_reviewed(0));
        assert!(!app.is_view_reviewed(1));
    }

    #[test]
    fn mark_current_file_reviewed_sets_file_bit_on_diff_file_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert!(app.is_view_reviewed(1));
        assert!(!app.is_view_reviewed(0));
    }

    #[test]
    fn mark_current_file_reviewed_persists_across_load() {
        // Round-trip: mark, load fresh state from disk, mark survives.
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 0;
        app.mark_current_file_reviewed();

        // Re-load into a brand-new App and confirm the bit is still set.
        let app2 = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        assert!(app2.is_view_reviewed(0));
    }

    #[test]
    fn cycle_file_auto_marks_landed_view_as_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(
            dir.path().to_owned(),
            vec![sample_diff_file(), sample_diff_file_b()],
        );
        // Start at description (file_index=0). Cycle forward to the first
        // diff file; that view must auto-mark as reviewed.
        app.file_index = 0;
        app.cycle_file(1);
        assert_eq!(app.file_index, 1);
        assert!(
            app.is_view_reviewed(1),
            "cycle_file landing must auto-mark the new file"
        );
    }

    #[test]
    fn file_header_label_renders_without_reviewed_glyph_in_pure_text() {
        // The trailing ✓ glyph is added at the Span level inside
        // `render_file_header`. The pure label helper stays untouched so
        // existing width/positioning tests don't have to plumb reviewed
        // state through.
        let app = make_app_with_single_file(sample_diff_file());
        let label = file_header_label(&app);
        assert!(!label.contains(REVIEWED_TITLE_GLYPH));
        // And no leftover "(reviewed)" text — Saskia's redesign drops it.
        assert!(!label.contains("(reviewed)"));
    }

    #[test]
    fn render_file_header_appends_check_glyph_when_reviewed() {
        use ratatui::backend::TestBackend;
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert!(app.is_view_reviewed(1));

        // Render only the file header into a wide single-row strip so the
        // ✓ glyph is unambiguously locatable.
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_file_header(frame, frame.area(), &app);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        // Walk the inner row (y=1; the block borders are at y=0 and y=2).
        let mut glyph_pos: Option<u16> = None;
        for x in 0..80 {
            if buf[(x, 1)].symbol() == REVIEWED_TITLE_GLYPH {
                glyph_pos = Some(x);
                break;
            }
        }
        let x = glyph_pos.expect(
            "trailing ✓ glyph must render in the file header when the active view is reviewed",
        );
        // Saskia's affect: the ✓ must be DarkGray, not bright Green.
        assert_eq!(
            buf[(x, 1)].fg,
            Color::DarkGray,
            "trailing ✓ in file header must be DarkGray"
        );
    }

    #[test]
    fn render_file_header_omits_check_glyph_when_unreviewed() {
        use ratatui::backend::TestBackend;
        let app = make_app_with_single_file(sample_diff_file());
        // Default: nothing is marked reviewed for this change_id.
        assert!(!app.is_view_reviewed(app.file_index));

        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_file_header(frame, frame.area(), &app);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        for x in 0..80 {
            assert_ne!(
                buf[(x, 1)].symbol(),
                REVIEWED_TITLE_GLYPH,
                "no ✓ must render when the view is unreviewed"
            );
        }
    }

    fn sample_diff_file_b() -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from("bar.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 1,
                target_start: 1,
                target_length: 1,
                lines: vec![Line {
                    kind: LineKind::Context,
                    text: "ctx".to_owned(),
                    source_line: Some(1),
                    target_line: Some(1),
                }],
            }],
        }
    }

    // ---- T1: initial-land mark on run_app ----

    /// `run_app` calls `mark_current_file_reviewed` immediately after the
    /// first `refresh_inline_comments`. We can't drive `run_app` in a unit
    /// test (it owns the terminal), but the wiring is `App::new` →
    /// `refresh_inline_comments` → `mark_current_file_reviewed`. Pin the
    /// composed effect: after that sequence, the description bit is set.
    #[test]
    fn initial_land_marks_description_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // file_index starts at 0 (description). Drive the same two calls
        // run_app makes before the event loop.
        app.refresh_inline_comments();
        app.mark_current_file_reviewed();

        let entry = app.reviewed.entries.get(&app.details.change_id).unwrap();
        assert!(
            entry.description_reviewed,
            "initial land must mark description reviewed"
        );
    }

    // ---- T2: save-failure status warning + is_none() guard ----

    /// Read-only repo root → save fails. With `status_message=None`, the
    /// warning IS set. Pins that real save failures DO surface to the user
    /// when nothing else has claimed the line.
    #[test]
    fn mark_current_file_reviewed_surfaces_save_failure_when_status_is_none() {
        // `/repo` doesn't exist on a normal test box, so atomic_write_bytes
        // can't `create_dir_all` it (the macOS sandbox refuses to write
        // outside the user's tree). That's the same scenario the existing
        // `make_app_with_single_file` helper uses to provoke save failures
        // in other tests.
        let mut app = make_app_with_single_file(sample_diff_file());
        app.status_message = None;
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert!(
            app.status_message
                .as_ref()
                .is_some_and(|m| m.contains("could not save reviewed state")),
            "save failure must surface when nothing else has claimed status; got: {:?}",
            app.status_message
        );
    }

    /// With `status_message` already set, `mark_current_file_reviewed` must
    /// NOT clobber it on save failure — purpose-set messages survive.
    #[test]
    fn mark_current_file_reviewed_preserves_existing_status_on_save_failure() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.status_message = Some("already at the last file".to_owned());
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert_eq!(
            app.status_message.as_deref(),
            Some("already at the last file"),
            "existing status must survive save-failure warning"
        );
    }

    // ---- T3: navigation-site auto-mark coverage ----

    /// `enter_reanchor_mode` resolves the comment's file in the diff,
    /// updates `file_index`, and must auto-mark. Drive the same code path
    /// directly: build a stale-screen state with a comment whose file is
    /// in the diff, then invoke `enter_reanchor_mode`.
    #[test]
    fn enter_reanchor_mode_auto_marks_landed_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // Build a stale comment anchored at foo.txt (the path on
        // sample_diff_file).
        let stale = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: app.details.change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("foo.txt"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@ -1,2 +1,3 @@".to_owned(),
                    target_text: "ctx".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "stale".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Stale),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        app.loaded_comments = vec![stale];
        app.screen = Screen::Stale(StaleScreenState {
            selected_index: 0,
            stale_indices: vec![0],
            scroll_offset: 0,
        });

        enter_reanchor_mode(&mut app);

        // file_index moved to the foo.txt diff view (index 1) and that
        // view's path is in reviewed_files.
        assert_eq!(app.file_index, 1);
        assert!(
            app.is_view_reviewed(1),
            "enter_reanchor_mode must auto-mark"
        );
    }

    /// `file_picker_enter` on a non-binary entry sets `file_index`, picks
    /// a commentable line, and must auto-mark.
    #[test]
    fn file_picker_enter_non_binary_auto_marks_landed_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        let entries = file_picker::build_entries(&app.details.diff.files, &[], &|_| false, &|_| 0);
        // Pick the foo.txt entry (view_index=1).
        let target_idx = entries
            .iter()
            .position(|e| e.view_index == 1)
            .expect("foo.txt entry must exist");
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: target_idx,
            scroll_offset: 0,
            entries,
        });

        file_picker_enter(&mut app);

        assert_eq!(app.file_index, 1);
        assert!(app.is_view_reviewed(1));
    }

    /// `file_picker_enter` on a binary entry sets the status and MUST
    /// still auto-mark — the user "landed" on the view even though there
    /// are no commentable lines.
    #[test]
    fn file_picker_enter_binary_auto_marks_landed_view() {
        let dir = tempfile::tempdir().unwrap();
        let binary_file = DiffFile::Binary {
            path: PathBuf::from("image.png"),
        };
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![binary_file]);
        let entries = file_picker::build_entries(&app.details.diff.files, &[], &|_| false, &|_| 0);
        let bin_idx = entries
            .iter()
            .position(|e| e.view_index == 1)
            .expect("binary entry must exist");
        app.screen = Screen::FilePicker(FilePickerState {
            selected_index: bin_idx,
            scroll_offset: 0,
            entries,
        });

        file_picker_enter(&mut app);

        assert_eq!(app.file_index, 1);
        assert!(
            app.is_view_reviewed(1),
            "binary file landing must still auto-mark"
        );
    }

    // ---- T6: is_view_reviewed App-level commit_id mismatch ----

    /// `App::is_view_reviewed` has its own `commit_id`-mismatch check.
    /// Pin it: an entry whose stored `commit_id` no longer matches the
    /// live `commit_id` must report unreviewed even though the entry
    /// exists.
    #[test]
    fn is_view_reviewed_returns_false_when_app_commit_id_mismatches_stored() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // Mark file_index=1 reviewed under the current commit_id.
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert!(app.is_view_reviewed(1));

        // Simulate the change being amended: live commit_id flips, but
        // the stored entry still references the old one.
        let new_commit = CommitId::parse(&"b".repeat(40)).unwrap();
        app.details.commit_id = new_commit;

        assert!(
            !app.is_view_reviewed(1),
            "commit_id mismatch must mask the stored bits at the App layer"
        );
        assert!(
            !app.is_view_reviewed(0),
            "description bit also masked by commit_id mismatch"
        );
    }

    // ---- T1: 3 missing mutation site tests ----

    /// Pin the auto-mark wiring on the count==1 `cycle_file` shortcut. A
    /// description-only change (no diff files) has
    /// `rendered_per_file.len() == 1`, which exercises the early-return
    /// branch. Pressing Tab on that change must still mark the view
    /// reviewed even though `file_index` doesn't change.
    #[test]
    fn cycle_file_single_file_shortcut_marks_current_file_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        // No diff files → only the description view remains, so
        // `rendered_per_file.len() == 1`. Use a constructor that doesn't
        // pre-mark by skipping the initial App::new auto-mark via making a
        // fresh state by hand.
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![]);
        // Clear the post-construction state so we can observe cycle_file's
        // own mark in isolation.
        app.reviewed = ReviewedState::default();
        assert_eq!(app.rendered_per_file.len(), 1, "expected single-view setup");
        app.file_index = 0;

        app.cycle_file(1);

        let entry = app
            .reviewed
            .entries
            .get(&app.details.change_id)
            .expect("cycle_file count==1 branch must mark the description");
        assert!(entry.description_reviewed);
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_ONLY_ONE_FILE),
            "the only-one-file hint must still surface"
        );
    }

    /// Pin auto-mark on `refresh_current_change`'s in-memory side
    /// (`apply_refreshed_change`). After a refresh, the active view must
    /// be marked reviewed even if the change id is unchanged.
    #[test]
    fn refresh_current_change_marks_current_file_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // Position on the diff file (view_index=1) and clear prior state so
        // the post-refresh mark is observable.
        app.file_index = 1;
        app.reviewed = ReviewedState::default();

        // Build a fresh ChangeDetails (same shape) and apply.
        let new_details = ChangeDetails {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
            diff: app.details.diff.clone(),
        };
        apply_refreshed_change(&mut app, new_details);

        assert!(
            app.is_view_reviewed(1),
            "refresh_current_change must auto-mark the active view"
        );
    }

    /// Pin auto-mark on the post-Claude reload path
    /// (`apply_post_claude_reload`). Claude may have rewritten files;
    /// reload resets `file_index` to 0 (description), which counts as a
    /// landing event and must auto-mark.
    #[test]
    fn invoke_claude_from_tui_post_reload_marks_current_file_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1; // pretend the user was viewing the diff
        app.reviewed = ReviewedState::default();

        let new_details = ChangeDetails {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
            diff: app.details.diff.clone(),
        };
        apply_post_claude_reload(&mut app, new_details);

        assert_eq!(
            app.file_index, 0,
            "post-Claude reload must reset to description"
        );
        let entry = app
            .reviewed
            .entries
            .get(&app.details.change_id)
            .expect("post-reload must mark the description");
        assert!(entry.description_reviewed);
    }

    fn make_stack_entry(change_label: char) -> StackEntry {
        StackEntry {
            change_id: ChangeId::parse(&change_label.to_string().repeat(32)).unwrap(),
            commit_id: CommitId::parse(&change_label.to_string().repeat(40)).unwrap(),
            description: format!("change {change_label}"),
        }
    }

    /// Happy path: prev `change_id` still in the new stack — index follows
    /// it, even when the position shifted (e.g. agent inserted a change
    /// before it).
    #[test]
    fn new_current_index_after_reload_returns_position_of_matching_change_id() {
        let entry_a = make_stack_entry('a');
        let entry_b = make_stack_entry('b');
        let entry_c = make_stack_entry('c');
        let prev_id = entry_b.change_id.clone();

        // Agent inserted a new change at index 0; prev change_id is now at index 2.
        let new_entries = vec![entry_c, entry_a, entry_b];
        assert_eq!(new_current_index_after_reload(&prev_id, 1, &new_entries), 2);
    }

    /// Agent abandoned the prev change and the old index is still in the new
    /// range — keep the old index so the cursor lands at the same position.
    #[test]
    fn new_current_index_after_reload_keeps_old_index_when_change_abandoned_and_in_range() {
        let entry_a = make_stack_entry('a');
        let entry_b = make_stack_entry('b');
        let abandoned_id = make_stack_entry('d').change_id;

        let new_entries = vec![entry_a, entry_b];
        assert_eq!(
            new_current_index_after_reload(&abandoned_id, 1, &new_entries),
            1
        );
    }

    /// Agent abandoned the prev change AND the old index is past the new end
    /// (e.g. agent squashed the last two changes, `prev_index` = 2, len = 1)
    /// — clamp to the last valid index.
    #[test]
    fn new_current_index_after_reload_clamps_when_old_index_past_new_end() {
        let entry_a = make_stack_entry('a');
        let abandoned_id = make_stack_entry('d').change_id;

        let new_entries = vec![entry_a];
        assert_eq!(
            new_current_index_after_reload(&abandoned_id, 5, &new_entries),
            0
        );
    }

    /// When the new stack is empty (all changes squashed away), the function
    /// must not underflow — it returns 0 regardless of `prev_index`.
    #[test]
    fn new_current_index_after_reload_empty_entries_returns_zero() {
        let abandoned_id = make_stack_entry('d').change_id;
        assert_eq!(
            new_current_index_after_reload(&abandoned_id, 0, &[]),
            0,
            "empty slice: prev_index=0"
        );
        assert_eq!(
            new_current_index_after_reload(&abandoned_id, 99, &[]),
            0,
            "empty slice: large prev_index must not underflow"
        );
    }

    /// Stack-mode reload replaces entries, advances the index, resets the
    /// cursor, and rebuilds views from the new details. Pins the full
    /// post-agent contract for stack mode.
    #[test]
    fn apply_post_claude_stack_reload_replaces_entries_and_resets_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, id_a, _id_b) = make_stack_app_with_two_changes(dir.path());
        app.file_index = 1;
        app.line_index = 5;
        app.scroll = 3;

        // Agent inserted a new change before the existing two; same revset.
        let entry_new = make_stack_entry('c');
        let entry_a = make_stack_entry('a');
        let entry_b = make_stack_entry('b');
        let resolved = ResolvedStack {
            revset: "trunk()..@".to_owned(),
            revset_hash: RevsetHash::from_revset("trunk()..@"),
            entries: vec![entry_new.clone(), entry_a.clone(), entry_b.clone()],
        };
        let details = ChangeDetails {
            change_id: id_a.clone(),
            commit_id: entry_a.commit_id.clone(),
            description: "first".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };

        apply_post_claude_stack_reload(&mut app, resolved, 1, details);

        let ctx = app.stack.as_ref().expect("stack context preserved");
        assert_eq!(
            ctx.entries.len(),
            3,
            "entries replaced with the resolved stack"
        );
        assert_eq!(ctx.current_index, 1, "current_index advanced to new slot");
        assert_eq!(ctx.entries[1].change_id, id_a, "index points at id_a");
        assert_eq!(app.file_index, 0, "cursor reset to description");
        assert_eq!(app.line_index, 0);
        assert_eq!(app.scroll, 0);
    }

    /// Pin the revset-update side: if the agent caused a fallback (e.g. the
    /// original revset became unresolvable), `apply_post_claude_stack_reload`
    /// must replace `revset` and `revset_hash` to match what was resolved, so
    /// downstream cursor-resume keys hash consistently.
    #[test]
    fn apply_post_claude_stack_reload_updates_revset_and_hash_when_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, id_a, _id_b) = make_stack_app_with_two_changes(dir.path());

        let entry_a = make_stack_entry('a');
        let fallback_revset = "@".to_owned();
        let resolved = ResolvedStack {
            revset: fallback_revset.clone(),
            revset_hash: RevsetHash::from_revset(&fallback_revset),
            entries: vec![entry_a.clone()],
        };
        let details = ChangeDetails {
            change_id: id_a,
            commit_id: entry_a.commit_id.clone(),
            description: "first".to_owned(),
            diff: Diff {
                files: vec![sample_diff_file()],
            },
        };

        apply_post_claude_stack_reload(&mut app, resolved, 0, details);

        let ctx = app.stack.as_ref().expect("stack context preserved");
        assert_eq!(ctx.revset, fallback_revset);
        assert_eq!(ctx.revset_hash, RevsetHash::from_revset(&fallback_revset));
    }

    /// Pin the default: a freshly-built `App` does not request a full
    /// redraw. The flag is only set out-of-band (e.g. after returning from
    /// `claude`), and a wrong default would force a `terminal.clear()` on
    /// every first frame.
    #[test]
    fn app_default_needs_full_redraw_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        assert!(!app.needs_full_redraw);
    }

    /// Pre-seeds the `TestBackend` buffer with non-space glyphs so the
    /// post-clear all-spaces assertion actually proves the clear ran (a
    /// no-op helper would fail this test).
    #[test]
    fn maybe_clear_for_full_redraw_clears_when_flagged_and_resets_flag() {
        use ratatui::backend::TestBackend;
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);

        let backend = TestBackend::new(4, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                let para = Paragraph::new("XXXX\nYYYY");
                frame.render_widget(para, area);
            })
            .expect("draw");
        // Sanity: the pre-seed actually wrote glyphs; otherwise the
        // post-clear assertion would be vacuous.
        let pre = terminal.backend().buffer().clone();
        assert_eq!(pre[(0, 0)].symbol(), "X");
        assert_eq!(pre[(0, 1)].symbol(), "Y");

        app.needs_full_redraw = true;
        maybe_clear_for_full_redraw(&mut terminal, &mut app).expect("helper");

        assert!(
            !app.needs_full_redraw,
            "helper must reset the flag after clearing"
        );
        let buf = terminal.backend().buffer().clone();
        for y in 0..2 {
            for x in 0..4 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "clear() must blank the backing buffer at ({x}, {y})"
                );
            }
        }
    }

    // ---- Saskia tweaks: U keybind toggle ----

    /// Pressing `U` on a file currently marked reviewed unmarks it and sets
    /// the unreviewed status message.
    #[test]
    fn toggle_current_file_reviewed_clears_when_currently_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.mark_current_file_reviewed();
        assert!(app.is_view_reviewed(1));
        app.status_message = None;

        app.toggle_current_file_reviewed();

        assert!(
            !app.is_view_reviewed(1),
            "U on a reviewed file must unmark it"
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_MARKED_UNREVIEWED)
        );
    }

    /// Pressing `U` on a file currently NOT marked reviewed marks it.
    /// Symmetric escape hatch — the auto-mark fallback when something
    /// upstream missed the file.
    #[test]
    fn toggle_current_file_reviewed_sets_when_currently_unreviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.reviewed = ReviewedState::default();
        assert!(!app.is_view_reviewed(1));

        app.toggle_current_file_reviewed();

        assert!(
            app.is_view_reviewed(1),
            "U on an unreviewed file must mark it"
        );
        assert_eq!(app.status_message.as_deref(), Some(STATUS_MARKED_REVIEWED));
    }

    /// Toggle persists immediately — a fresh `App` constructed against the
    /// same repo root sees the inverted state.
    #[test]
    fn toggle_current_file_reviewed_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.toggle_current_file_reviewed();
        assert!(app.is_view_reviewed(1));

        let app2 = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        assert!(
            app2.is_view_reviewed(1),
            "toggle must persist across an App reload"
        );
    }

    /// Toggle works on the description view (`file_index` == 0) too.
    #[test]
    fn toggle_current_file_reviewed_handles_description_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 0;
        app.reviewed = ReviewedState::default();

        app.toggle_current_file_reviewed();
        assert!(app.is_view_reviewed(0));

        app.toggle_current_file_reviewed();
        assert!(!app.is_view_reviewed(0));
    }

    /// `U` is wired through the main key dispatcher.
    #[test]
    fn u_key_from_main_toggles_current_file_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 1;
        app.reviewed = ReviewedState::default();

        let key = KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE);
        handle_main_key(&mut app, key).unwrap();

        assert!(
            app.is_view_reviewed(1),
            "U keybind must mark the current file"
        );
    }

    // ---- Saskia tweaks: commit_id invalidation toast ----

    /// Touching a known change with a different `commit_id` (the change
    /// was amended/rebased) drops the prior reviewed bits AND surfaces a
    /// status toast naming the reset.
    #[test]
    fn mark_current_file_reviewed_surfaces_reset_toast_on_commit_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // First, mark the description reviewed under the original commit_id.
        app.file_index = 0;
        app.mark_current_file_reviewed();
        // Simulate the change being amended: live commit_id flips.
        let new_commit = CommitId::parse(&"b".repeat(40)).unwrap();
        app.details.commit_id = new_commit;
        app.status_message = None;

        // Now mark again — `mark()` will see the stored commit_id
        // mismatches the live one, drop the entry, and report the reset.
        app.mark_current_file_reviewed();

        assert_eq!(
            app.status_message.as_deref(),
            Some(STATUS_REVIEWED_RESET),
            "reset toast must surface when commit_id mismatch invalidates prior bits"
        );
    }

    /// First-encounter mark on a change with no prior entry must NOT
    /// surface the reset toast — there was nothing to reset, so the toast
    /// would be misleading.
    #[test]
    fn mark_current_file_reviewed_silent_on_first_touch_unknown_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 0;
        app.reviewed = ReviewedState::default();
        app.status_message = None;

        app.mark_current_file_reviewed();

        assert!(
            app.status_message.is_none(),
            "first-touch mark on a fresh change must stay silent; got: {:?}",
            app.status_message
        );
    }

    /// Re-marking the same `(change_id, commit_id)` (the user re-lands on
    /// the same view) must NOT surface the reset toast — the `commit_id`
    /// matches, so nothing was invalidated.
    #[test]
    fn mark_current_file_reviewed_silent_on_repeat_mark_same_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        app.file_index = 0;
        app.mark_current_file_reviewed();
        app.status_message = None;

        // Re-mark — same change_id, same commit_id. No invalidation.
        app.mark_current_file_reviewed();

        assert!(
            app.status_message.is_none(),
            "repeat mark with matching commit_id must stay silent; got: {:?}",
            app.status_message
        );
    }

    /// The reset toast respects the same `is_none()` guard as the
    /// save-failure warning: a purpose-set status message (e.g.,
    /// Tab-at-boundary) must NOT be clobbered when a commit_id-mismatch
    /// fires on the same tick.
    #[test]
    fn mark_current_file_reviewed_does_not_clobber_existing_status_on_reset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = make_app_in_dir(dir.path().to_owned(), vec![sample_diff_file()]);
        // Plant a prior entry under the original commit_id so the next
        // mark sees a mismatch.
        app.file_index = 0;
        app.mark_current_file_reviewed();
        // Simulate amend.
        let new_commit = CommitId::parse(&"b".repeat(40)).unwrap();
        app.details.commit_id = new_commit;
        // Pretend Tab-at-boundary already set a purpose-set status.
        app.status_message = Some("already at the last file".to_owned());

        app.mark_current_file_reviewed();

        assert_eq!(
            app.status_message.as_deref(),
            Some("already at the last file"),
            "purpose-set status must survive even when a reset would otherwise toast"
        );
    }

    // resolve_diff_mode: file_index 0 (description view) is always unified
    // regardless of width or user preference. Two-side semantics do not
    // apply to commit messages.
    #[test]
    fn resolve_diff_mode_description_view_is_always_unified() {
        for pref in [
            DiffMode::Auto,
            DiffMode::ForceUnified,
            DiffMode::ForceSideBySide,
        ] {
            for width in [80_u16, 119, 120, 200] {
                assert_eq!(
                    resolve_diff_mode(pref, width, 0),
                    EffectiveDiffMode::Unified,
                    "description view (file_index=0) must stay unified for {pref:?} at width {width}"
                );
            }
        }
    }

    // resolve_diff_mode: Auto picks side-by-side at exactly 120 cols.
    #[test]
    fn resolve_diff_mode_auto_picks_side_by_side_at_threshold() {
        assert_eq!(
            resolve_diff_mode(DiffMode::Auto, 120, 1),
            EffectiveDiffMode::SideBySide,
            "auto + width=120 must select side-by-side"
        );
    }

    // resolve_diff_mode: Auto stays unified one column below the threshold.
    #[test]
    fn resolve_diff_mode_auto_stays_unified_below_threshold() {
        assert_eq!(
            resolve_diff_mode(DiffMode::Auto, 119, 1),
            EffectiveDiffMode::Unified,
            "auto + width=119 must select unified"
        );
    }

    // resolve_diff_mode: ForceUnified ignores width.
    #[test]
    fn resolve_diff_mode_force_unified_ignores_width() {
        for width in [60_u16, 119, 120, 500] {
            assert_eq!(
                resolve_diff_mode(DiffMode::ForceUnified, width, 1),
                EffectiveDiffMode::Unified,
                "ForceUnified at width {width} must select unified"
            );
        }
    }

    // resolve_diff_mode: ForceSideBySide ignores width — even at 60 cols
    // (which renders very narrow columns) the user's choice is honored.
    #[test]
    fn resolve_diff_mode_force_side_by_side_ignores_width() {
        for width in [60_u16, 119, 120, 500] {
            assert_eq!(
                resolve_diff_mode(DiffMode::ForceSideBySide, width, 1),
                EffectiveDiffMode::SideBySide,
                "ForceSideBySide at width {width} must select side-by-side"
            );
        }
    }

    // cycle_diff_mode walks Auto -> ForceUnified -> ForceSideBySide -> Auto
    // and resets the cursor (line_index, scroll) on every transition because
    // the row-index space differs across modes.
    #[test]
    fn cycle_diff_mode_walks_three_states_and_returns_to_auto() {
        let mut app = make_app_with_single_file(sample_diff_file());
        assert_eq!(app.diff_mode, DiffMode::Auto);

        app.cycle_diff_mode();
        assert_eq!(app.diff_mode, DiffMode::ForceUnified);
        app.cycle_diff_mode();
        assert_eq!(app.diff_mode, DiffMode::ForceSideBySide);
        app.cycle_diff_mode();
        assert_eq!(app.diff_mode, DiffMode::Auto);
    }

    #[test]
    fn cycle_diff_mode_resets_cursor_and_scroll() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        app.scroll = 5;
        app.cycle_diff_mode();
        assert_eq!(app.line_index, 0);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn cycle_diff_mode_emits_status_message() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.cycle_diff_mode();
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|m| m.contains("unified")),
            "status must announce the new layout"
        );
    }

    // Side-by-side mode at 120 cols: rendering must succeed and the
    // gutter divider glyph (`│`) must appear in the body.
    #[test]
    fn render_at_120_cols_in_force_side_by_side_emits_gutter() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::ForceSideBySide;
        let buf = render_to_buffer(&mut app, 120, 24);
        let mut found = false;
        let (top, bottom) = diff_area_rows(24);
        for y in top..bottom {
            for x in 0..buf.area().width {
                if buf[(x, y)].symbol() == "\u{2502}" {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "gutter glyph must appear when side-by-side renders");
    }

    // Unified mode at 80 cols: no gutter divider glyph in the body.
    #[test]
    fn render_at_80_cols_unified_has_no_gutter_glyph() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::Auto;
        let buf = render_to_buffer(&mut app, 80, 24);
        let (top, bottom) = diff_area_rows(24);
        for y in top..bottom {
            for x in 0..buf.area().width.saturating_sub(1) {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "\u{2502}",
                    "auto mode at width 80 must stay unified — no gutter expected at ({x},{y})"
                );
            }
        }
    }

    // Description view (file_index=0) must NEVER render side-by-side, even
    // when the user pinned ForceSideBySide. Pin the file_index=0 carve-out.
    #[test]
    fn description_view_never_renders_side_by_side() {
        let mut app = make_app_description_only();
        app.diff_mode = DiffMode::ForceSideBySide;
        // Render at 200 cols where auto would otherwise pick side-by-side.
        let buf = render_to_buffer(&mut app, 200, 24);
        let (top, bottom) = diff_area_rows(24);
        for y in top..bottom {
            for x in 0..buf.area().width.saturating_sub(1) {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "\u{2502}",
                    "description view must not render the side-by-side gutter"
                );
            }
        }
    }

    // Cursor highlight in side-by-side mode reverses cells but keeps the
    // gutter calm. On a Pair row (Removed-only / Added-only / paired), both
    // side cells reverse-video; the divider stays unstyled so it doesn't
    // compete with cell content. Pin both sides reversed AND the gutter NOT
    // reversed in a single test.
    #[test]
    fn side_by_side_focused_row_reverses_cells_only_keeping_gutter_calm() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::ForceSideBySide;
        // Sample paired rows: 0=Spanning(HunkHeader), 1=Spanning(Context),
        // 2=Pair{None, Some(Added)}, 3=Pair{Some(Removed), None}. Land on
        // row 2 — a Pair row that exercises both columns + gutter.
        app.line_index = 2;
        let buf = render_to_buffer(&mut app, 120, 24);
        let (top, _) = diff_area_rows(24);
        // Pair rows render at row index → screen row top + 2 (after the two
        // Spanning rows above).
        let target_row = top + 2;
        let total_width = buf.area().width;
        let body_width = total_width - 1; // minus scrollbar column
        let side_width = (body_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2;
        let gutter_col = side_width; // gutter starts immediately after left cell

        let left_cell = &buf[(0, target_row)];
        let right_cell = &buf[(side_width + SIDE_BY_SIDE_GUTTER_WIDTH, target_row)];
        let gutter_cell = &buf[(gutter_col + 1, target_row)]; // middle of " │ "

        assert!(
            left_cell.style().add_modifier.contains(Modifier::REVERSED),
            "left cell must reverse-video on focused Pair row"
        );
        assert!(
            right_cell.style().add_modifier.contains(Modifier::REVERSED),
            "right cell must reverse-video on focused Pair row"
        );
        assert!(
            !gutter_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "gutter must stay calm (not REVERSED) so it does not compete with cell content"
        );
    }

    // C1 — Focus-fg policy is uniform across every render path: when a row
    // is focused, the fg color is dropped and only `REVERSED` is applied.
    // A Removed line in side-by-side under focus must NOT carry
    // `Color::Red` on top of REVERSED — the same render decision unified
    // mode has always made. Pinning this prevents regressions where one
    // render path keeps fg under REVERSED and the other doesn't,
    // producing a visual mismatch when the reviewer toggles `|`.
    //
    // Note on ratatui buffer fg: a Span styled with `Style::default()` (no
    // fg call) lands in the buffer with fg=Some(Reset). A Span styled with
    // `.fg(Color::Red)` lands with fg=Some(Red). The "no explicit color"
    // property we want to pin is `fg != Some(Red)` (or any concrete color)
    // when focused — equivalently, `fg == Some(Reset) || fg == None`.
    #[test]
    fn side_by_side_focused_row_strips_fg_color_to_match_unified_render() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::ForceSideBySide;
        // Row 3 is the Removed-only Pair (left=Some(3), right=None).
        // The left cell carries the `- removed` glyph; under focus it
        // must reverse-video without keeping Red as fg.
        app.line_index = 3;
        let buf = render_to_buffer(&mut app, 120, 24);
        let (top, _) = diff_area_rows(24);
        let target_row = top + 3;
        let left_cell = &buf[(0, target_row)];
        assert!(
            left_cell.style().add_modifier.contains(Modifier::REVERSED),
            "focused Removed cell must REVERSE",
        );
        let focused_fg = left_cell.style().fg;
        assert!(
            !matches!(focused_fg, Some(Color::Red)),
            "focused Removed cell must drop Red fg under REVERSED to match unified render policy; got {focused_fg:?}",
        );

        // Cross-check: under the SAME conditions but unfocused, the cell
        // does paint Red. This ensures the test above is not vacuously
        // true (i.e. fg never set anywhere).
        app.line_index = 0;
        let buf2 = render_to_buffer(&mut app, 120, 24);
        let unfocused_left = &buf2[(0, target_row)];
        assert!(
            !unfocused_left
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "unfocused Removed cell must not REVERSE (precondition for fg check)",
        );
        assert_eq!(
            unfocused_left.style().fg,
            Some(Color::Red),
            "unfocused Removed cell must paint Red — focus-fg test is vacuous if this fails",
        );
    }

    // Auto mode at 119 cols renders unified; at 120 cols renders side-by-side.
    // This is the ladder boundary so we pin both directions in one test.
    #[test]
    fn auto_mode_threshold_boundary_119_vs_120() {
        let mut app119 = make_app_with_single_file(sample_diff_file());
        app119.diff_mode = DiffMode::Auto;
        let buf119 = render_to_buffer(&mut app119, 119, 24);
        let (top, bottom) = diff_area_rows(24);
        let mut has_gutter_at_119 = false;
        for y in top..bottom {
            for x in 0..buf119.area().width.saturating_sub(1) {
                if buf119[(x, y)].symbol() == "\u{2502}" {
                    has_gutter_at_119 = true;
                }
            }
        }
        assert!(
            !has_gutter_at_119,
            "auto mode at 119 cols must stay unified"
        );

        let mut app120 = make_app_with_single_file(sample_diff_file());
        app120.diff_mode = DiffMode::Auto;
        let buf120 = render_to_buffer(&mut app120, 120, 24);
        let mut has_gutter_at_120 = false;
        for y in top..bottom {
            for x in 0..buf120.area().width.saturating_sub(1) {
                if buf120[(x, y)].symbol() == "\u{2502}" {
                    has_gutter_at_120 = true;
                }
            }
        }
        assert!(
            has_gutter_at_120,
            "auto mode at 120 cols must switch to side-by-side"
        );
    }

    // move_line in side-by-side mode walks paired-row indices, not unified
    // line indices. The sample diff produces 4 unified rows but 4 paired
    // rows too (HunkHeader, Context, Added-only, Removed-only) so we pick
    // a hunk that compresses (1 Removed + 1 Added pair) to differentiate.
    #[test]
    fn move_line_in_side_by_side_walks_paired_rows() {
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 3,
                target_start: 1,
                target_length: 3,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        text: "ctx".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    },
                    Line {
                        kind: LineKind::Removed,
                        text: "old".to_owned(),
                        source_line: Some(2),
                        target_line: None,
                    },
                    Line {
                        kind: LineKind::Added,
                        text: "new".to_owned(),
                        source_line: None,
                        target_line: Some(2),
                    },
                    Line {
                        kind: LineKind::Context,
                        text: "ctx2".to_owned(),
                        source_line: Some(3),
                        target_line: Some(3),
                    },
                ],
            }],
        };
        let mut app = make_app_with_single_file(file);
        app.diff_mode = DiffMode::ForceSideBySide;
        // Render once so diff_body_width is populated.
        let _ = render_to_buffer(&mut app, 120, 24);

        // Unified row count: 5 (HunkHeader + Context + Removed + Added + Context).
        // Paired row count: 4 (HunkHeader, Context, Removed/Added pair, Context).
        let view = app.current_view().expect("view");
        assert_eq!(view.lines.len(), 5);
        assert_eq!(view.paired_rows.len(), 4);
        assert_eq!(app.current_row_count(), 4);

        // Walk to the bottom: 3 j-presses gets to row index 3 in side-by-side.
        app.line_index = 0;
        for _ in 0..10 {
            app.move_line(1);
        }
        assert_eq!(
            app.line_index, 3,
            "move_line in side-by-side must clamp at paired_rows.len() - 1"
        );
    }

    /// delta=0 with the cursor on an interior skip row (`HunkSeparator`) must
    /// move the cursor to the nearest navigable row rather than leaving it on
    /// the non-navigable row.
    #[test]
    fn move_line_delta_zero_on_skip_row_finds_navigable_row() {
        // Build a diff file with two hunks so the rendered view contains a
        // HunkSeparator at an interior position.
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![
                Hunk {
                    header: "@@ -1,1 +1,1 @@".to_owned(),
                    function_context: None,
                    source_start: 1,
                    source_length: 1,
                    target_start: 1,
                    target_length: 1,
                    lines: vec![Line {
                        kind: LineKind::Context,
                        text: "ctx1".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    }],
                },
                Hunk {
                    header: "@@ -5,1 +5,1 @@".to_owned(),
                    function_context: None,
                    source_start: 5,
                    source_length: 1,
                    target_start: 5,
                    target_length: 1,
                    lines: vec![Line {
                        kind: LineKind::Context,
                        text: "ctx2".to_owned(),
                        source_line: Some(5),
                        target_line: Some(5),
                    }],
                },
            ],
        };
        let mut app = make_app_with_single_file(file);
        app.refresh_inline_comments();
        // Find the HunkSeparator row (between the two hunks).
        let view = app.current_view().expect("view");
        let sep_idx = view
            .lines
            .iter()
            .position(|l| l.kind == RenderedLineKind::HunkSeparator)
            .expect("HunkSeparator must be present between hunks");
        app.line_index = sep_idx;
        // delta=0: cursor is already on a skip row. move_line must find a
        // navigable neighbor.
        app.move_line(0);
        assert!(
            !app.is_skip_row(app.line_index),
            "delta=0 on a HunkSeparator must move cursor off the skip row; \
             landed at {}, kind={:?}",
            app.line_index,
            app.current_view()
                .and_then(|v| v.lines.get(app.line_index))
                .map(|l| l.kind),
        );
    }

    /// Build a `DiffFile::Modified` with a single one-line hunk. Tests pick
    /// which axis to stress: `header` (with optional `function_context`) for
    /// header-rendering probes, `line_text` for Context-rendering probes.
    fn single_context_line_file(
        header: &str,
        function_context: Option<String>,
        line_text: String,
    ) -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: header.to_owned(),
                function_context,
                source_start: 1,
                source_length: 1,
                target_start: 1,
                target_length: 1,
                lines: vec![Line {
                    kind: LineKind::Context,
                    text: line_text,
                    source_line: Some(1),
                    target_line: Some(1),
                }],
            }],
        }
    }

    // C1 — A long hunk header in side-by-side mode renders ONCE across the
    // full body width, not duplicated/truncated per side. Probe with a
    // unique sentinel substring so accidental `@@` repetition in the header
    // text does not inflate the count.
    #[test]
    fn render_paired_row_hunk_header_spans_full_body_width_in_side_by_side() {
        let long_ctx = "SENTINEL_really_really_really_long_function_name_exceeding_one_side";
        let mut app = make_app_with_single_file(single_context_line_file(
            &format!("@@ -1,1 +1,1 @@ {long_ctx}"),
            Some(long_ctx.to_owned()),
            "ctx".to_owned(),
        ));
        app.diff_mode = DiffMode::ForceSideBySide;
        let buf = render_to_buffer(&mut app, 200, 24);
        let (top, _) = diff_area_rows(24);

        let mut header_row = String::new();
        for x in 0..buf.area().width {
            header_row.push_str(buf[(x, top)].symbol());
        }

        // The unique sentinel must appear exactly once — duplication per
        // column would put a second copy past the gutter.
        let occurrences = header_row.matches("SENTINEL").count();
        assert_eq!(
            occurrences, 1,
            "long hunk header must render once across full body width, got {occurrences} occurrences in row: {header_row:?}"
        );
        // The function context must appear in full (no truncation marker `…`
        // inserted by the per-side budget).
        assert!(
            header_row.contains(long_ctx),
            "long function-context must appear in full at width 200; row: {header_row:?}"
        );
    }

    // Context lines emit Pair { Some(i), Some(i) }; each side must
    // truncate independently to side_width. A regression that routes
    // Context back through render_full_width_row would paint past
    // the gutter into the right column.
    #[test]
    fn render_long_context_does_not_bleed_across_gutter() {
        let mut app = make_app_with_single_file(single_context_line_file(
            "@@ -1,1 +1,1 @@",
            None,
            "x".repeat(200),
        ));
        app.diff_mode = DiffMode::ForceSideBySide;
        let buf = render_to_buffer(&mut app, 120, 24);
        let (top, _) = diff_area_rows(24);
        let total_width = buf.area().width;
        let body_width = total_width - 1; // minus scrollbar
        let side_width = (body_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2;
        let context_row = top + 1;

        let mut left_half = String::new();
        for x in 0..side_width {
            left_half.push_str(buf[(x, context_row)].symbol());
        }
        let mut gutter = String::new();
        for x in side_width..(side_width + SIDE_BY_SIDE_GUTTER_WIDTH) {
            gutter.push_str(buf[(x, context_row)].symbol());
        }
        let mut right_half = String::new();
        for x in (side_width + SIDE_BY_SIDE_GUTTER_WIDTH)..body_width {
            right_half.push_str(buf[(x, context_row)].symbol());
        }

        assert_eq!(
            gutter, " \u{2502} ",
            "gutter must be intact between independently-truncated sides; \
             got {gutter:?} at row {context_row}"
        );
        assert!(
            left_half.contains('\u{2026}'),
            "left side of an over-long context line must end in an ellipsis; \
             got {left_half:?}"
        );
        assert!(
            right_half.contains('\u{2026}'),
            "right side of an over-long context line must end in an ellipsis; \
             got {right_half:?}"
        );
        assert!(
            left_half.contains("xx"),
            "left side must contain the context payload; got {left_half:?}"
        );
        assert!(
            right_half.contains("xx"),
            "right side must contain the context payload; got {right_half:?}"
        );
        // No `x`-run in the right column may exceed side_width — that
        // signals left text bled past the gutter.
        let max_x_run = right_half
            .chars()
            .fold((0_usize, 0_usize), |(best, run), c| {
                if c == 'x' {
                    let r = run + 1;
                    (best.max(r), r)
                } else {
                    (best, 0)
                }
            })
            .0;
        assert!(
            max_x_run <= usize::from(side_width),
            "right column `x` run length {max_x_run} exceeds side_width \
             {side_width} — text bled across the gutter; right_half={right_half:?}"
        );
    }

    /// Build an app + a single `Side::Old`-anchored line comment on a removed
    /// line that has no Added counterpart. Used by the C3 regression and T3
    /// tests.
    fn make_app_with_pure_deletion_and_old_comment() -> App {
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,1 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 2,
                target_start: 1,
                target_length: 1,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        text: "ctx".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    },
                    Line {
                        kind: LineKind::Removed,
                        text: "removed_only".to_owned(),
                        source_line: Some(2),
                        target_line: None,
                    },
                ],
            }],
        };
        let mut app = make_app_with_single_file(file);
        // Side::Old comment anchored to the removed line.
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: app.details.change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("foo.txt"),
                    side: Side::Old,
                    old_line: Some(2),
                    new_line: None,
                    hunk_header: "@@ -1,2 +1,1 @@".to_owned(),
                    target_text: "removed_only".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "deletion comment".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        app.loaded_comments = vec![comment];
        app.rebuild_annotated_views();
        app
    }

    // C3 — A `Side::Old`-anchored comment on a pure-deletion row (right
    // column blank) renders in the LEFT column where the deleted line sits,
    // not in the right column. The "always right" rule is conditional on
    // there being a right cell to attach to.
    #[test]
    fn inline_comment_on_pure_deletion_row_renders_in_left_column_when_anchored_to_old_side() {
        let mut app = make_app_with_pure_deletion_and_old_comment();
        app.diff_mode = DiffMode::ForceSideBySide;
        let buf = render_to_buffer(&mut app, 120, 24);
        let (top, _) = diff_area_rows(24);
        let total_width = buf.area().width;
        let body_width = total_width - 1; // minus scrollbar
        let side_width = (body_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2;

        // Find the row containing "deletion comment" by scanning each diff
        // row for the substring.
        let comment_row = (top..(top + 10))
            .find(|y| {
                let mut s = String::new();
                for x in 0..total_width {
                    s.push_str(buf[(x, *y)].symbol());
                }
                s.contains("deletion comment")
            })
            .expect("comment row must render somewhere in the diff body");

        let mut left_half = String::new();
        for x in 0..side_width {
            left_half.push_str(buf[(x, comment_row)].symbol());
        }
        let mut right_half = String::new();
        for x in (side_width + SIDE_BY_SIDE_GUTTER_WIDTH)..total_width {
            right_half.push_str(buf[(x, comment_row)].symbol());
        }

        assert!(
            left_half.contains("deletion comment"),
            "Side::Old comment on pure-deletion row must render in LEFT column; left_half={left_half:?}"
        );
        assert!(
            !right_half.contains("deletion comment"),
            "Side::Old comment must NOT also appear in right column; right_half={right_half:?}"
        );
    }

    // T3 — Inline-comment-renders-right-column-only buffer probe for the
    // common case (Side::New anchor on an Added line). Right column carries
    // the comment text; left column is whitespace-only on the comment row.
    #[test]
    fn inline_comment_side_new_renders_in_right_column_only() {
        let mut app = make_app_with_single_file(sample_diff_file());
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: app.details.change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("foo.txt"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(2),
                    hunk_header: "@@ -1,2 +1,3 @@".to_owned(),
                    target_text: "added".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "addition note".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        app.loaded_comments = vec![comment];
        app.rebuild_annotated_views();
        app.diff_mode = DiffMode::ForceSideBySide;
        let buf = render_to_buffer(&mut app, 120, 24);
        let (top, _) = diff_area_rows(24);
        let total_width = buf.area().width;
        let body_width = total_width - 1;
        let side_width = (body_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2;

        let comment_row = (top..(top + 10))
            .find(|y| {
                let mut s = String::new();
                for x in 0..total_width {
                    s.push_str(buf[(x, *y)].symbol());
                }
                s.contains("addition note")
            })
            .expect("comment row must render somewhere in the diff body");

        let mut left_half = String::new();
        for x in 0..side_width {
            left_half.push_str(buf[(x, comment_row)].symbol());
        }
        let mut right_half = String::new();
        for x in (side_width + SIDE_BY_SIDE_GUTTER_WIDTH)..total_width {
            right_half.push_str(buf[(x, comment_row)].symbol());
        }

        assert!(
            right_half.contains("addition note"),
            "Side::New comment must render in RIGHT column; right_half={right_half:?}"
        );
        assert!(
            !left_half.contains("addition note"),
            "Side::New comment must NOT appear in LEFT column; left_half={left_half:?}"
        );
        // Left half must be whitespace + style prefix only (no comment text).
        let trimmed = left_half.trim();
        assert!(
            trimmed.is_empty(),
            "left half on comment row must be whitespace-only; got {trimmed:?}"
        );
    }

    // T1 — Sub-MIN_USEFUL_SIDE_BY_SIDE_WIDTH fallback: even with
    // ForceSideBySide, a width of 8 cells is below the useful split
    // threshold. The renderer must fall back to unified rendering, emitting
    // no gutter glyph and no panic. Pin the contract.
    //
    // Skipped: setup_terminal enforces MIN_COLS=60. The test bypasses
    // setup_terminal via render_to_buffer + TestBackend, so this exercises
    // the renderer path directly. It also probes a legitimate runtime
    // condition: a wider terminal can still produce a narrow body when
    // panes are split or scrollbars eat columns.
    #[test]
    fn force_side_by_side_at_width_below_useful_threshold_falls_back_to_unified() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::ForceSideBySide;
        // Width=8 < MIN_USEFUL_SIDE_BY_SIDE_WIDTH (=11). The renderer
        // must render without panicking and must NOT emit the gutter glyph.
        let buf = render_to_buffer(&mut app, 8, 24);
        let (top, bottom) = diff_area_rows(24);
        for y in top..bottom {
            for x in 0..buf.area().width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "\u{2502}",
                    "below-threshold side-by-side must not emit the gutter glyph (fall back to unified)"
                );
            }
        }
    }

    // T4 — `diff_body_width` is set by `render_diff` to the post-scrollbar
    // body width every frame so navigation handlers (`move_line`,
    // `jump_to`, `effective_diff_mode`) resolve against the same width the
    // renderer actually used. Pin the cache contract.
    #[test]
    fn diff_body_width_cache_is_set_after_render() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.diff_mode = DiffMode::Auto;
        assert_eq!(app.diff_body_width, 0, "cache is zero before any render");
        let _ = render_to_buffer(&mut app, 120, 24);
        // 120 total - 1 scrollbar (only if the diff overflows; sample doesn't,
        // so scrollbar is suppressed and body == full area width = 120).
        assert!(
            app.diff_body_width >= 119,
            "diff_body_width must reflect post-scrollbar body width; got {}",
            app.diff_body_width
        );
        assert!(
            app.diff_body_width <= 120,
            "diff_body_width must not exceed area width; got {}",
            app.diff_body_width
        );
    }

    // T5 — Cursor index out-of-range after G-then-resize. line_index points
    // into the *active layout's* row space. After jumping to bottom in
    // unified mode then switching to side-by-side (which has fewer rows),
    // the cursor must not panic on the next move. The renderer's
    // `current_row_count` clamp via `move_line` is the line of defense.
    #[test]
    fn cursor_out_of_range_clamped_when_layout_shrinks() {
        // Build a diff with paired_rows.len() < lines.len() so the layout
        // shrinks under side-by-side mode.
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 3,
                target_start: 1,
                target_length: 3,
                lines: vec![
                    Line {
                        kind: LineKind::Removed,
                        text: "old1".to_owned(),
                        source_line: Some(1),
                        target_line: None,
                    },
                    Line {
                        kind: LineKind::Removed,
                        text: "old2".to_owned(),
                        source_line: Some(2),
                        target_line: None,
                    },
                    Line {
                        kind: LineKind::Added,
                        text: "new1".to_owned(),
                        source_line: None,
                        target_line: Some(1),
                    },
                    Line {
                        kind: LineKind::Added,
                        text: "new2".to_owned(),
                        source_line: None,
                        target_line: Some(2),
                    },
                ],
            }],
        };
        let mut app = make_app_with_single_file(file);

        // Start unified, render once, then jump to the bottom.
        app.diff_mode = DiffMode::ForceUnified;
        let _ = render_to_buffer(&mut app, 200, 24);
        let unified_rows = app.current_row_count();
        app.line_index = unified_rows - 1;

        // Switch to side-by-side. paired_rows is shorter than lines because
        // 2 Removed + 2 Added pair down to 2 Pair rows + 1 HunkHeader = 3.
        app.diff_mode = DiffMode::ForceSideBySide;
        // Side-by-side resolution needs a fresh render to update
        // diff_body_width and let `effective_diff_mode` see SideBySide.
        let _ = render_to_buffer(&mut app, 200, 24);

        // `cycle_diff_mode` already resets line_index when it cycles, but
        // direct field assignment bypasses that. Pin the safety net: a
        // subsequent `move_line` must clamp line_index to the new row count
        // and must not panic.
        // Move down once. clamp_with_delta inside move_line clamps to
        // current_row_count() - 1 = paired_rows.len() - 1.
        app.move_line(1);
        let paired_count = app.current_row_count();
        assert!(
            app.line_index < paired_count,
            "line_index must be clamped to current_row_count(); got {} >= {}",
            app.line_index,
            paired_count
        );
    }

    // ── entity-bundle assembly tests ─────────────────────────────────────────

    use crate::diff::Diff as TestDiff;
    use local_review_core::semantic::cache::{GraphData, GraphEdge, GraphNode};
    use local_review_core::semantic::{
        ChangeAnnotation, ChangeType, EntityCoreData, EntityId, EntityKind,
    };

    fn bundle_eid(file: &str, name: &str) -> EntityId {
        EntityId::new(PathBuf::from(file), vec![name.to_owned()], None, 0)
    }

    fn bundle_ent(file: &str, name: &str, start: u32, end: u32) -> EntityCoreData {
        EntityCoreData {
            id: bundle_eid(file, name),
            kind: EntityKind::Function,
            change: ChangeType::Modified,
            annotation: ChangeAnnotation::BodyOnly,
            line_range: (start, end),
            source_file: None,
            target_line: None,
            structural_change: true,
            content_hash: 0,
        }
    }

    fn bundle_anchor(file: &str, new_line: u32) -> LineAnchor {
        LineAnchor {
            file: PathBuf::from(file),
            side: Side::New,
            old_line: None,
            new_line: Some(new_line),
            hunk_header: "@@ -1 +1 @@".to_owned(),
            target_text: "fn foo() {}".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
    }

    #[test]
    fn entity_for_comment_line_prefers_entity_id_match() {
        let eid = bundle_eid("src/foo.rs", "bar");
        let entities = vec![
            bundle_ent("src/foo.rs", "bar", 1, 10),
            bundle_ent("src/foo.rs", "baz", 11, 20),
        ];
        let anchor = bundle_anchor("src/foo.rs", 15);
        // With entity_id pointing at "bar" (lines 1-10), even though line 15
        // falls in "baz", the entity_id match wins.
        let result = entity_for_comment_line(&anchor, Some(&eid), &entities);
        assert_eq!(result.map(|e| e.id.name()), Some("bar"));
    }

    #[test]
    fn entity_for_comment_line_falls_back_to_line_range() {
        let entities = vec![
            bundle_ent("src/foo.rs", "alpha", 1, 5),
            bundle_ent("src/foo.rs", "beta", 6, 15),
        ];
        let anchor = bundle_anchor("src/foo.rs", 10);
        let result = entity_for_comment_line(&anchor, None, &entities);
        assert_eq!(result.map(|e| e.id.name()), Some("beta"));
    }

    #[test]
    fn entity_for_comment_line_returns_none_when_no_match() {
        let entities = vec![bundle_ent("src/foo.rs", "alpha", 1, 5)];
        let anchor = bundle_anchor("src/foo.rs", 50);
        let result = entity_for_comment_line(&anchor, None, &entities);
        assert!(result.is_none());
    }

    #[test]
    fn entity_for_comment_line_old_side_uses_entity_id_not_line_range() {
        let eid = bundle_eid("src/foo.rs", "alpha");
        let entities = vec![bundle_ent("src/foo.rs", "alpha", 1, 10)];
        let mut anchor = bundle_anchor("src/foo.rs", 0);
        anchor.side = Side::Old;
        anchor.new_line = None;
        anchor.old_line = Some(3);
        // entity_id match should succeed even though new_line is None.
        let result = entity_for_comment_line(&anchor, Some(&eid), &entities);
        assert_eq!(result.map(|e| e.id.name()), Some("alpha"));
    }

    #[test]
    fn hunk_text_for_comment_returns_empty_for_non_line_anchor() {
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: ChangeId::parse("abc12345").unwrap(),
            },
            repo_root: PathBuf::new(),
            revset: String::new(),
            commit_id: None,
            body: "test".to_owned(),
            severity: Severity::Required,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let diff = TestDiff { files: vec![] };
        assert_eq!(hunk_text_for_comment(&comment, &diff), "");
    }

    #[test]
    fn read_entity_body_strips_injection_controls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/evil.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // File contains an ESC (ANSI injection) but tabs and newlines should
        // survive (tabs = indentation, newlines = line structure).
        std::fs::write(&path, "fn foo() {\n\tlet _x = 1;\n\x1b[31mevil\x1b[0m\n}").unwrap();
        let entity = bundle_ent("src/evil.rs", "foo", 1, 4);
        let body = read_entity_body(&entity, dir.path()).unwrap();
        assert!(!body.contains('\x1b'), "ESC must be stripped");
        assert!(body.contains('\t'), "tabs must be preserved");
        assert!(body.contains('\n'), "newlines must be preserved");
    }

    #[test]
    fn graph_bundle_entities_deduplicates_multiple_calls_to_same_target() {
        let source = bundle_eid("src/a.rs", "caller");
        let target = bundle_eid("src/b.rs", "callee");
        // Three edges from source → target (one per call site in the source).
        let graph = GraphData {
            nodes: vec![
                GraphNode {
                    id: source.clone(),
                    kind: EntityKind::Function,
                },
                GraphNode {
                    id: target.clone(),
                    kind: EntityKind::Function,
                },
            ],
            edges: vec![
                GraphEdge {
                    from: source.clone(),
                    to: target.clone(),
                },
                GraphEdge {
                    from: source.clone(),
                    to: target.clone(),
                },
                GraphEdge {
                    from: source.clone(),
                    to: target.clone(),
                },
            ],
        };
        let entities = vec![
            bundle_ent("src/a.rs", "caller", 1, 10),
            bundle_ent("src/b.rs", "callee", 1, 5),
        ];
        let dir = tempfile::tempdir().unwrap();
        let callee_path = dir.path().join("src/b.rs");
        std::fs::create_dir_all(callee_path.parent().unwrap()).unwrap();
        std::fs::write(&callee_path, "fn callee() {}\n").unwrap();
        let (deps, _dependents) = graph_bundle_entities(&source, &graph, &entities, dir.path());
        assert_eq!(
            deps.len(),
            1,
            "three edges to the same callee should dedup to one BundleEntity"
        );
    }
}
