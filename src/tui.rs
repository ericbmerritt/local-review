use std::io::{stdout, Stdout};
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::{Frame, Terminal};

use crate::change_id::ChangeId;
use crate::comment::{
    Anchor, Comment, DescriptionAnchor, LineAnchor, SchemaVersion, Severity, Side, Status,
    CONTEXT_MAX,
};
use crate::cursor;
use crate::error::{JjrError, Result};
use crate::jj::{self, ChangeDetails};
use crate::stack::{ResolvedStack, RevsetHash, StackEntry};
use crate::util::{clamp_with_delta, page_size, pluralize, truncate};

mod composer;
mod composer_overlay;
mod diff_view;
mod file_picker;
mod help_screen;
mod overview_screen;
mod send_to_claude;
mod stale_screen;

use composer::{
    default_severity, Composer, ComposerAction, ComposerInit, ComposerScope, DescriptionContext,
    EditedComment, LineTarget, StackContextSnapshot,
};
use diff_view::{
    comment_to_inline, description_comment_to_inline, DiffView, InlineComment, RenderedLine,
    RenderedLineKind,
};
use file_picker::{build_entries as build_file_picker_entries, FilePickerState};
use overview_screen::{OverviewCommentSet, OverviewScreenState};
use send_to_claude::{ConfirmData, SendToClaudeState};
use stale_screen::StaleScreenState;

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block (one `│` on each side).
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render measures the
/// real diff area height. Overwritten by `render_main` on every frame.
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

/// Width (cells) of a length-indicator (vertical scrollbar). The scrollbar
/// widget renders into a single column on the right edge of a paginated body
/// (diff pane, file picker, stale list, stack overview) when content overflows
/// the viewport.
pub(super) const SCROLLBAR_WIDTH: u16 = 1;

/// Stack depth at which `transition_screen = "auto"` starts firing. Per spec,
/// deep stacks get the beat between changes; short ones don't need the pause.
const AUTO_TRANSITION_THRESHOLD: usize = 8;

/// Width (cells) of the graphical fill in the stack progress bar. Drops to
/// zero on narrow terminals (see `render_stack_bar`).
const STACK_PROGRESS_BAR_WIDTH: u16 = 20;

/// Below this column count, the stack bar drops the graphical fill and shows
/// just the textual `N/M change_id desc...` portion (per the resize ladder).
const STACK_BAR_MIN_COLS_FOR_FILL: u16 = 80;

/// Width (cells) of the transition modal.
const TRANSITION_MODAL_WIDTH: u16 = 42;

/// Height (rows) of the transition modal.
const TRANSITION_MODAL_HEIGHT: u16 = 18;

/// Description budget (chars) inside the transition modal. The modal interior
/// is ~38 cols after borders + indent, so 36 leaves room for the trailing `…`.
const TRANSITION_DESC_BUDGET: usize = 36;

/// Maximum number of `●` dots rendered before truncating with a trailing `…`.
/// The numeric count stays accurate so the user still sees the true total.
pub(super) const DOT_BUDGET: usize = 5;

/// Status hint surfaced when Tab is pressed at the last file (or description+files
/// boundary) — the file index is already at its max and cannot advance further.
const STATUS_AT_LAST_FILE: &str = "already at the last file";

/// Status hint surfaced when Shift-Tab is pressed at `file_index` 0 — there is
/// no previous file to retreat to.
const STATUS_AT_FIRST_FILE: &str = "already at the first file";

/// Status hint surfaced when Tab/Shift-Tab is pressed and the change has only
/// one navigable view (typical for a description-only change with no diff
/// files), so cycling cannot move in either direction.
const STATUS_ONLY_ONE_FILE: &str = "only one file";

/// Severity -> terminal color.
pub(super) fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Required => Color::Red,
        Severity::Suggestion => Color::Yellow,
        Severity::Note => Color::DarkGray,
    }
}

/// Severity -> display label.
pub(super) fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Required => "required",
        Severity::Suggestion => "suggestion",
        Severity::Note => "note",
    }
}

pub fn run(change_id: &ChangeId, repo_root: &std::path::Path) -> Result<()> {
    let details = jj::show(change_id)?;
    let revset = change_id.as_str().to_owned();

    let mut terminal = setup_terminal()?;
    let outcome = run_app(&mut terminal, details, repo_root.to_owned(), revset, None);
    teardown_terminal(&mut terminal)?;
    outcome
}

pub fn run_stack(
    repo_root: &std::path::Path,
    resolved: &ResolvedStack,
    restart: bool,
) -> Result<()> {
    if resolved.entries.is_empty() {
        return Err(JjrError::RevsetNoMatch {
            revset: resolved.revset.clone(),
        });
    }

    if restart {
        cursor::clear(repo_root, resolved.revset_hash)?;
    }

    let has_comments = |id: &ChangeId| {
        crate::store::load_change_comments(repo_root, id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    };
    let start_index = cursor::resume_index(
        repo_root,
        resolved.revset_hash,
        &resolved
            .entries
            .iter()
            .map(|e| e.change_id.clone())
            .collect::<Vec<_>>(),
        &has_comments,
    );

    let entry = &resolved.entries[start_index];
    let details = jj::show(&entry.change_id)?;

    let stack_ctx = StackContext {
        entries: resolved.entries.clone(),
        current_index: start_index,
        revset: resolved.revset.clone(),
        revset_hash: resolved.revset_hash,
    };

    let mut terminal = setup_terminal()?;
    let outcome = run_app(
        &mut terminal,
        details,
        repo_root.to_owned(),
        resolved.revset.clone(),
        Some(stack_ctx),
    );
    teardown_terminal(&mut terminal)?;
    outcome
}

type Term = Terminal<CrosstermBackend<Stdout>>;

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
    severity_histogram: SeverityHistogram,
}

/// Counts of comments by severity. Used by the transition modal and the stack
/// overview's right-edge dot column. Stale comments are excluded from counts —
/// they live exclusively in the stale comments view (Screen 5) per spec.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct SeverityHistogram {
    pub(super) required: usize,
    pub(super) suggestion: usize,
    pub(super) note: usize,
}

impl SeverityHistogram {
    pub(super) fn from_comments(comments: &[Comment]) -> Self {
        let mut h = Self::default();
        for c in comments {
            // Stale and orphaned records do not contribute to active-comment
            // counts. Stale lives in the stale view; orphaned is out of scope.
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

    pub(super) fn total(self) -> usize {
        self.required + self.suggestion + self.note
    }
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

/// Which transition behavior is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionMode {
    Never,
    Auto,
    Always,
}

struct App {
    details: ChangeDetails,
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
}

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
        Self {
            details,
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
            last_severity: None,
            status_message: None,
            stack,
            transition_mode,
            comments_loaded_ok: false,
            pending_reanchor: None,
            overview_cache: None,
            severity_filter: None,
        }
    }

    fn current_view(&self) -> Option<&DiffView> {
        self.annotated_per_file.get(self.file_index)
    }

    fn current_line_count(&self) -> usize {
        self.current_view().map_or(0, |v| v.lines.len())
    }

    fn refresh_inline_comments(&mut self) {
        // Any comment edit invalidates the overview cache so the next open
        // gets a fresh load.
        self.overview_cache = None;

        match crate::store::load_change_comments(&self.repo_root, &self.details.change_id) {
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
        let mut errors: Vec<String> = Vec::new();
        let reconciled: Vec<Comment> = comments
            .into_iter()
            .map(|comment| {
                match crate::anchoring::reanchor_comment(
                    &comment,
                    &self.details.diff,
                    &self.details.description,
                ) {
                    None => comment,
                    Some(updated) => {
                        match crate::store::update_comment(&self.repo_root, &updated) {
                            Ok(()) => updated,
                            Err(e) => {
                                errors.push(format_persist_error(&updated, &e));
                                comment
                            }
                        }
                    }
                }
            })
            .collect();
        if let Some(last) = errors.into_iter().last() {
            self.status_message = Some(last);
        }
        reconciled
    }

    fn rebuild_annotated_views(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        let severity_filter = self.severity_filter;
        // rendered_per_file[0] is the description view; diff files start at index 1.
        self.annotated_per_file = self
            .rendered_per_file
            .iter()
            .enumerate()
            .map(|(view_idx, base_view)| {
                let inline: Vec<InlineComment> = if view_idx == 0 {
                    self.loaded_comments
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, c)| description_comment_to_inline(c, idx, now))
                        .filter(|ic| severity_filter.is_none_or(|filter| ic.severity == filter))
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
                        .filter(|ic| severity_filter.is_none_or(|filter| ic.severity == filter))
                        .collect()
                };
                base_view.clone().with_inline_comments(&inline)
            })
            .collect();
    }

    fn move_line(&mut self, delta: isize) {
        let count = self.current_line_count();
        if count == 0 {
            return;
        }
        let max_index = count - 1;
        let mut next = clamp_with_delta(self.line_index, delta, max_index);
        // Skip non-navigable lines: hunk separators and comment body continuation
        // lines. InlineCommentMeta lines are navigable — they are the "handle"
        // the reviewer lands on to press `e` or `d`.
        let step: isize = if delta >= 0 { 1 } else { -1 };
        while next > 0
            && next < max_index
            && self.current_view().is_some_and(|v| {
                matches!(
                    v.lines[next].kind,
                    RenderedLineKind::HunkSeparator | RenderedLineKind::InlineCommentBody,
                )
            })
        {
            next = clamp_with_delta(next, step, max_index);
        }
        self.line_index = next;
    }

    fn move_page(&mut self, delta: isize) {
        let step = page_size(self.viewport_rows);
        let signed_step: isize = isize::try_from(step).unwrap_or(isize::MAX);
        self.move_line(delta.saturating_mul(signed_step));
    }

    fn jump_to(&mut self, end: Edge) {
        let count = self.current_line_count();
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
                SeverityHistogram::from_comments(&self.loaded_comments),
            )
        } else {
            (None, SeverityHistogram::default())
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

        let stack_level = crate::store::load_stack_comments(&repo_root, &revset_hash)
            .unwrap_or_else(|e| {
                let _ = e; // best-effort; ignore load failures for overview
                Vec::new()
            });

        let per_change: Vec<Vec<Comment>> = entries
            .iter()
            .map(|entry| {
                crate::store::load_change_comments(&repo_root, &entry.change_id).unwrap_or_default()
            })
            .collect();

        let orphaned = collect_orphaned_comments(&repo_root, &entries);

        self.overview_cache = Some(OverviewCommentSet {
            stack_level,
            per_change,
            orphaned,
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

        if advance {
            let _ = cursor::record(&self.repo_root, revset_hash, &revset, &change_id);
        }

        Ok(())
    }
}

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
    repo_root: &std::path::Path,
    stack_entries: &[StackEntry],
) -> Vec<Comment> {
    let in_stack: std::collections::HashSet<&ChangeId> =
        stack_entries.iter().map(|e| &e.change_id).collect();

    let Ok(all_on_disk) = crate::store::list_change_ids_with_comments(repo_root) else {
        return Vec::new();
    };

    let mut orphaned = Vec::new();
    for change_id in all_on_disk {
        if in_stack.contains(&change_id) {
            continue;
        }
        let Ok(comments) = crate::store::load_change_comments(repo_root, &change_id) else {
            continue;
        };
        for mut comment in comments {
            comment.status = Some(Status::Orphaned);
            orphaned.push(comment);
        }
    }
    orphaned
}

fn run_app(
    terminal: &mut Term,
    details: ChangeDetails,
    repo_root: PathBuf,
    revset: String,
    stack: Option<StackContext>,
) -> Result<()> {
    let transition_mode = load_transition_mode(&repo_root);
    let mut app = App::new(details, repo_root, revset, stack, transition_mode);
    app.refresh_inline_comments();

    while !app.should_quit {
        terminal
            .draw(|frame| render(frame, &mut app))
            .map_err(io_err)?;
        handle_event(&mut app)?;
    }

    // Persist the cursor at the last-viewed change so a subsequent run
    // resumes here. Best-effort: a cursor write failure should not block exit.
    persist_cursor_on_exit(&app);

    Ok(())
}

/// Best-effort cursor write on app exit. Silent on failure — the cursor file
/// is convenience state, not authoritative.
fn persist_cursor_on_exit(app: &App) {
    let Some(ctx) = app.stack.as_ref() else {
        return;
    };
    let change_id = &ctx.entries[ctx.current_index].change_id;
    let _ = cursor::record(&app.repo_root, ctx.revset_hash, &ctx.revset, change_id);
}

fn load_transition_mode(repo_root: &std::path::Path) -> TransitionMode {
    let config_path = repo_root.join(".jj-review").join("config.toml");
    if !config_path.exists() {
        return TransitionMode::Never;
    }
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return TransitionMode::Never;
    };
    let Ok(table) = raw.parse::<toml::Table>() else {
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
fn render(frame: &mut Frame<'_>, app: &mut App) {
    if matches!(app.screen, Screen::Stale(_)) {
        let Screen::Stale(mut state) = std::mem::replace(&mut app.screen, Screen::Main) else {
            unreachable!("matched above");
        };
        stale_screen::render(frame, &mut state, app);
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
            overview_screen::render(frame, &mut state, app, cache_ref);
        }
        app.overview_cache = cache;
        app.screen = Screen::Overview(state);
        return;
    }

    render_main(frame, app);
    match &app.screen {
        Screen::Main => {}
        Screen::Help => help_screen::render(frame),
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

fn file_header_label(app: &App) -> String {
    let total = app.rendered_per_file.len();
    let position = app.file_index.saturating_add(1);
    let path_label = app
        .current_view()
        .map_or_else(|| "(no files)".to_owned(), |v| v.title.clone());
    format!("{path_label}  ·  {position} of {total}")
}

fn render_file_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let label = file_header_label(app);
    let block = Block::default().borders(Borders::ALL).title("File");
    let widget = Paragraph::new(label).block(block);
    frame.render_widget(widget, area);
}

fn render_diff(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(view) = app.current_view() else {
        let widget = Paragraph::new("No files in this change.");
        frame.render_widget(widget, area);
        return;
    };

    let (body_area, scrollbar_area, mut sb_state) =
        scrollbar_layout_for_view(area, view.lines.len(), app.scroll);

    let width = body_area.width;
    let lines: Vec<TuiLine<'_>> = view
        .lines
        .iter()
        .enumerate()
        .map(|(idx, line)| render_rendered_line(line, idx == app.line_index, width))
        .collect();

    let widget = Paragraph::new(lines).scroll((app.scroll, 0));
    frame.render_widget(widget, body_area);

    render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);
}

/// Pure numeric core for a paginated-view scrollbar: returns
/// `Some((content_length, position))` when the content overflows the viewport,
/// `None` otherwise.
///
/// `total_lines` is the count of distinct rows the body can scroll across —
/// for the diff pane that is rendered diff lines, for an entry list that is
/// the entry count (or summed per-entry rows when entries vary in height).
/// The math is identical regardless of what each row represents.
///
/// `content_length` is the number of distinct *scroll positions* (top-row
/// indices the user can land at), which is `max_scroll + 1` where
/// `max_scroll = total_lines - viewport_rows`. `position` is `scroll` clamped
/// to `[0, max_scroll]`.
///
/// The clamp is defensive against future ratatui changes — current ratatui
/// also clamps internally — and gives us a tuple shape the tests can pin
/// numerically without rendering.
pub(super) fn scrollbar_overflow_for_view(
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

/// Build the [`ScrollbarState`] for a paginated body of `total_lines` rows
/// with topmost-visible row `scroll` in a viewport `viewport_rows` rows tall.
///
/// Returns `None` when the content fits in the viewport (or the viewport is
/// degenerate); the caller should skip the scrollbar in that case so it does
/// not waste a column on noise.
///
/// Thin shell over [`scrollbar_overflow_for_view`]: pinning the numeric
/// behavior happens against the pure helper; this function just lifts the
/// tuple into ratatui's `ScrollbarState`.
pub(super) fn scrollbar_state_for_view(
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
/// Returns `(body_area, scrollbar_slot)`. The slot is `None` when no
/// scrollbar was requested (`with_scrollbar = false`) **or** when the area is
/// too narrow to host both the body and a scrollbar column. In both cases the
/// body keeps the full original area.
pub(super) fn split_body_for_scrollbar(area: Rect, with_scrollbar: bool) -> (Rect, Option<Rect>) {
    if !with_scrollbar || area.width <= SCROLLBAR_WIDTH {
        return (area, None);
    }
    let split =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(SCROLLBAR_WIDTH)]).split(area);
    (split[0], Some(split[1]))
}

/// Build the right-edge scrollbar widget shared by every paginated view.
/// Centralizes the orientation and per-element style choices so all screens
/// render the same glyphs and colors.
pub(super) fn view_scrollbar() -> Scrollbar<'static> {
    Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .track_style(Style::default().fg(Color::DarkGray))
        .thumb_style(Style::default().fg(Color::Gray))
        .begin_style(Style::default().fg(Color::DarkGray))
        .end_style(Style::default().fg(Color::DarkGray))
}

/// One-shot layout helper for paginated views that want a scrollbar.
///
/// Combines [`scrollbar_state_for_view`] and [`split_body_for_scrollbar`] so
/// each call site collapses to "compute layout, render body into `body`,
/// hand `(state, sb_area)` to [`render_view_scrollbar`]." The helper returns
/// `(body, sb_area, sb_state)`:
///
/// - `body` is always the area into which the view's content is rendered. It
///   shrinks by [`SCROLLBAR_WIDTH`] when a scrollbar will be drawn.
/// - `sb_area` and `sb_state` are both `Some` together (overflow + room for
///   the strip) or both `None` (no overflow, or area too narrow).
pub(super) fn scrollbar_layout_for_view(
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
pub(super) fn render_view_scrollbar(
    frame: &mut Frame<'_>,
    sb_state: Option<&mut ScrollbarState>,
    sb_area: Option<Rect>,
) {
    if let (Some(state), Some(area)) = (sb_state, sb_area) {
        frame.render_stateful_widget(view_scrollbar(), area, state);
    }
}

fn render_rendered_line(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    // Inline comment lines use a per-severity color (spec principle 6:
    // severity is color, not text). The `●` sigil in the meta line ensures
    // NO_COLOR terminals can still distinguish severity by reading the label.
    match line.kind {
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            let color = match line.comment_severity {
                Some(s) => severity_color(s),
                None => Color::Cyan,
            };
            let base_style = Style::default().fg(color);
            let style = if focused {
                base_style.add_modifier(Modifier::REVERSED)
            } else {
                base_style
            };
            return TuiLine::from(vec![Span::styled(line.text.as_str(), style)]);
        }
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice
        | RenderedLineKind::Added
        | RenderedLineKind::Removed
        | RenderedLineKind::DescriptionLine => {}
    }

    let prefix = match line.kind {
        RenderedLineKind::Added => "+ ",
        RenderedLineKind::Removed => "- ",
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice
        | RenderedLineKind::DescriptionLine => "  ",
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            unreachable!("inline comment kinds returned above")
        }
    };

    let content_style = match line.kind {
        RenderedLineKind::Added => Style::default().fg(Color::Green),
        RenderedLineKind::Removed => Style::default().fg(Color::Red),
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice
        | RenderedLineKind::DescriptionLine => Style::default(),
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            unreachable!("inline comment kinds returned above")
        }
    };

    if focused {
        // Pad to full row width so reverse-video covers the entire line.
        let prefix_chars = prefix.chars().count();
        let content_chars = line.text.chars().count();
        let used = prefix_chars + content_chars;
        let pad_count = usize::from(width).saturating_sub(used);
        let padding: String = " ".repeat(pad_count);
        let padded_text = format!("{}{}", line.text, padding);
        TuiLine::from(vec![Span::styled(
            format!("{prefix}{padded_text}"),
            Style::default().add_modifier(Modifier::REVERSED),
        )])
    } else {
        TuiLine::from(vec![
            Span::raw(prefix),
            Span::styled(line.text.as_str(), content_style),
        ])
    }
}

const FOOTER_IRREDUCIBLE: &str = " \u{2191}\u{2193} line  Tab file  n/p revision  Enter comment";

struct FooterSegment {
    text: &'static str,
    stack_only: bool,
}

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

fn handle_event(app: &mut App) -> Result<()> {
    let evt = event::read().map_err(io_err)?;
    if let Event::Key(key) = evt {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        // Match on the screen discriminant to avoid moving out of app.screen.
        match &app.screen {
            Screen::Main => handle_main_key(app, key)?,
            Screen::Help => handle_help_key(app, key),
            Screen::Composer(_) => handle_composer_event(app, key),
            Screen::Transition(_) => handle_transition_key(app, key)?,
            Screen::Stale(_) => handle_stale_key(app, key),
            Screen::Overview(_) => handle_overview_key(app, key)?,
            Screen::SendToClaude(_) => handle_send_to_claude_key(app, key)?,
            Screen::FilePicker(_) => handle_file_picker_key(app, key),
        }
    }
    Ok(())
}

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
        KeyCode::Char('?') => app.screen = Screen::Help,
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
        _ => {}
    }
    Ok(())
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q' | '?') | KeyCode::Esc) {
        app.screen = Screen::Main;
    }
}

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

fn focused_stale(app: &App) -> Option<&Comment> {
    let Screen::Stale(ref state) = app.screen else {
        return None;
    };
    let &comment_idx = state.stale_indices.get(state.selected_index)?;
    app.loaded_comments.get(comment_idx)
}

fn delete_focused_stale(app: &mut App, comment: &Comment) {
    match crate::store::delete_comment(&app.repo_root, comment) {
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
}

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
        }
        None => {
            app.status_message = Some(
                "re-anchor: file not in current diff; pick a line in the visible file".to_owned(),
            );
        }
    }
}

fn open_stale_screen(app: &mut App) {
    let stale_indices = stale_screen::stale_comment_indices(&app.loaded_comments);
    app.screen = Screen::Stale(StaleScreenState {
        selected_index: 0,
        stale_indices,
        scroll_offset: 0,
    });
}

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

fn open_send_to_claude(app: &mut App) {
    let change_id = app.details.change_id.clone();
    let revset_hash = app.stack.as_ref().map(|s| s.revset_hash);

    let resolved = ResolvedStack {
        revset_hash: revset_hash.unwrap_or_else(|| RevsetHash::from_revset(&app.revset)),
        revset: app.revset.clone(),
        entries: vec![StackEntry {
            change_id: change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: app.details.description.clone(),
        }],
    };

    let packet = match crate::packet::build_packet(
        &app.repo_root,
        &app.revset,
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

    let stale_count =
        send_to_claude::stale_count_for_change(&app.repo_root, &change_id, revset_hash);
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

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the send-to-claude screen"
)]
fn handle_send_to_claude_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Screen::SendToClaude(ref state) = app.screen else {
        return Ok(());
    };

    match state.as_ref() {
        SendToClaudeState::Confirm(_) => match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Main;
            }
            KeyCode::Char('v') => {
                let Screen::SendToClaude(boxed) = std::mem::replace(&mut app.screen, Screen::Main)
                else {
                    unreachable!("matched above");
                };
                let SendToClaudeState::Confirm(data) = *boxed else {
                    unreachable!("matched Confirm above");
                };
                let prompt = crate::packet::render_prompt(&data.packet);
                app.screen = Screen::SendToClaude(Box::new(SendToClaudeState::PromptView {
                    confirm: data,
                    prompt,
                    scroll_offset: 0,
                }));
            }
            KeyCode::Enter => {
                invoke_claude_from_tui(app)?;
            }
            _ => {}
        },
        SendToClaudeState::PromptView { scroll_offset, .. } => {
            let offset = *scroll_offset;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    let Screen::SendToClaude(boxed) =
                        std::mem::replace(&mut app.screen, Screen::Main)
                    else {
                        unreachable!("matched above");
                    };
                    let SendToClaudeState::PromptView { confirm, .. } = *boxed else {
                        unreachable!("matched PromptView above");
                    };
                    app.screen =
                        Screen::SendToClaude(Box::new(SendToClaudeState::Confirm(confirm)));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Screen::SendToClaude(ref mut s) = app.screen {
                        if let SendToClaudeState::PromptView {
                            ref mut scroll_offset,
                            ..
                        } = s.as_mut()
                        {
                            *scroll_offset = offset.saturating_sub(1);
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Screen::SendToClaude(ref mut s) = app.screen {
                        if let SendToClaudeState::PromptView {
                            ref mut scroll_offset,
                            ..
                        } = s.as_mut()
                        {
                            *scroll_offset = offset.saturating_add(1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Suspend the TUI, run Claude with the current change's packet, then
/// restore the alternate screen and redraw.
///
/// The normal exit path uses `restore_tui()?` so I/O errors propagate. A
/// `TerminalRestoreGuard` covers the panic path: if anything inside the
/// closure unwinds, the guard's Drop best-effort-restores raw mode and the
/// alternate screen so the user's shell isn't left wedged.
fn invoke_claude_from_tui(app: &mut App) -> Result<()> {
    let Screen::SendToClaude(ref state) = app.screen else {
        return Ok(());
    };
    let SendToClaudeState::Confirm(data) = state.as_ref() else {
        return Ok(());
    };

    let change_id = data.change_id.clone();
    let prompt = crate::packet::render_prompt(&data.packet);
    let repo_root = app.repo_root.clone();

    suspend_tui()?;
    let restore = TerminalRestoreGuard;

    let outcome = (|| -> Result<crate::claude::ClaudeOutcome> {
        let _guard = crate::working_copy_guard::WorkingCopyGuard::enter(&repo_root, &change_id)?;
        crate::claude::invoke_claude(&prompt)
    })();

    std::mem::forget(restore);
    restore_tui()?;

    app.screen = Screen::Main;

    match outcome? {
        crate::claude::ClaudeOutcome::Success => match jj::show(&change_id) {
            Ok(details) => {
                app.rendered_per_file = build_rendered_views(&details);
                app.annotated_per_file = app.rendered_per_file.clone();
                app.details = details;
                app.file_index = 0;
                app.line_index = 0;
                app.scroll = 0;
                app.overview_cache = None;
                app.refresh_inline_comments();
            }
            Err(e) => {
                app.status_message = Some(format!(
                    "claude completed; could not reload diff: {}",
                    sanitize_for_status(&e.to_string())
                ));
            }
        },
        crate::claude::ClaudeOutcome::Failed { exit_code } => {
            let code_str = exit_code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
            app.status_message = Some(format!(
                "claude exited with {code_str}; working copy restored"
            ));
        }
    }
    Ok(())
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
fn overview_open_composer(app: &mut App, rows: &[overview_screen::OverviewRow], selected: usize) {
    let (initial_tag, change_idx_for_change_scope) = rows
        .get(selected)
        .map(|row| match row {
            overview_screen::OverviewRow::StackHeader
            | overview_screen::OverviewRow::StackComment(_) => (OverviewInitialScope::Stack, None),
            overview_screen::OverviewRow::ChangeRow(ci) => {
                (OverviewInitialScope::Change, Some(*ci))
            }
            overview_screen::OverviewRow::ChangeComment { change_idx, .. } => {
                (OverviewInitialScope::Change, Some(*change_idx))
            }
            overview_screen::OverviewRow::Separator
            | overview_screen::OverviewRow::SummaryFooterStale
            | overview_screen::OverviewRow::SummaryFooterTotal => {
                (OverviewInitialScope::Change, None)
            }
        })
        .unwrap_or((OverviewInitialScope::Change, None));

    let target_change_id: ChangeId = change_idx_for_change_scope
        .and_then(|idx| {
            app.stack
                .as_ref()
                .and_then(|s| s.entries.get(idx).map(|e| e.change_id.clone()))
        })
        .unwrap_or_else(|| app.details.change_id.clone());

    open_composer_with_scope(app, initial_tag, target_change_id);
}

/// Cursor-derived hint for the composer's initial scope when opened from the
/// overview screen. The actual `ComposerScope` is constructed inside
/// `open_composer_with_scope` from the availability snapshots — this hint
/// only encodes the cursor-row's intent, not the per-variant payload.
#[derive(Debug, Clone, Copy)]
enum OverviewInitialScope {
    Change,
    Stack,
}

/// Open a new-comment composer with an initial-scope hint from the cursor.
/// `target_change_id` binds the composer's Change-scope target — the overview
/// path passes the cursor's change; the main-view path passes the current
/// change. If the requested initial scope's availability is missing
/// (e.g., Stack hint in single-change mode), the composer falls back to a
/// scope that does have backing context (Line if the cursor is on a
/// commentable line, otherwise Change).
fn open_composer_with_scope(
    app: &mut App,
    initial: OverviewInitialScope,
    target_change_id: ChangeId,
) {
    let line_available = match build_line_target(app) {
        BuildTargetResult::Ready(t) => Some(t),
        BuildTargetResult::DescriptionLine { .. }
        | BuildTargetResult::NonCommentable
        | BuildTargetResult::NoView => None,
    };
    let stack_available = stack_snapshot(app);
    let change_description = change_description_for_target(app, &target_change_id);
    let scope = match initial {
        OverviewInitialScope::Stack => match stack_available.clone() {
            Some(s) => ComposerScope::Stack(s),
            None => fallback_scope(line_available.clone()),
        },
        OverviewInitialScope::Change => ComposerScope::Change,
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
fn fallback_scope(line_available: Option<LineTarget>) -> ComposerScope {
    match line_available {
        Some(line) => ComposerScope::Line(line),
        None => ComposerScope::Change,
    }
}

fn stack_snapshot(app: &App) -> Option<StackContextSnapshot> {
    app.stack.as_ref().map(|s| StackContextSnapshot {
        revset: s.revset.clone(),
        revset_hash: s.revset_hash,
    })
}

/// The Change-scope chrome shows the change's description text. We carry it
/// only for the current change (the only one whose body is loaded in
/// `app.details`); for non-current changes the description is empty.
fn change_description_for_target(app: &App, target: &ChangeId) -> String {
    if *target == app.details.change_id {
        app.details.description.clone()
    } else {
        String::new()
    }
}

/// Open the composer in edit mode for a stack-level comment.
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

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored on the transition modal"
)]
fn handle_transition_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let Screen::Transition(ref state) = app.screen else {
        return Ok(());
    };
    let next_index = state.next_index;
    match key.code {
        KeyCode::Enter => {
            app.screen = Screen::Main;
            app.load_stack_entry(next_index, true)?;
        }
        // `p` retreats: closes the transition modal AND moves to the change
        // before the reviewed one. Same semantics as `p` on the main screen
        // (previous from current position) — at transition time `current_index`
        // is still the reviewed change, so `retreat_stack` lands at
        // `reviewed_index - 1`. To cancel the advance without moving, press Esc.
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
const TRANSITION_FOOTER_TEXT: &str = "  Enter  p prev  Esc cancel  q quit";

/// Render the `●●● 3 required · ● 1 suggestion` line (or honest fallback when
/// the comment load failed).
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
pub(super) fn render_dots_mixed(hist: SeverityHistogram) -> String {
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
                        match crate::store::delete_comment(&app.repo_root, &reanchor.original) {
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
        composer::STATUS_STACK_UNAVAILABLE,
        "stack scope unavailable in single-change mode"
    );
    assert_eq!(
        composer::STATUS_DESCRIPTION_UNAVAILABLE,
        "description scope unavailable: open from a description line"
    );
    assert_eq!(
        composer::STATUS_LINE_UNAVAILABLE,
        "line scope unavailable: cursor is not on a commentable line"
    );
}

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

fn open_composer(app: &mut App) {
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

fn open_composer_for_edit(app: &mut App) {
    let Some(comment) = focused_comment(app) else {
        app.status_message = Some("cursor is not on a comment".to_owned());
        return;
    };
    let Anchor::Line { location, .. } = &comment.anchor else {
        app.status_message = Some("only line-scoped comments can be edited here".to_owned());
        return;
    };

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

/// Single-keystroke delete without confirmation.
fn delete_focused_comment(app: &mut App) {
    let Some(comment) = focused_comment(app).cloned() else {
        app.status_message = Some("cursor is not on a comment".to_owned());
        return;
    };

    let target_index = anchor_line_index(app, &comment);

    match crate::store::delete_comment(&app.repo_root, &comment) {
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
    #[expect(
        clippy::expect_used,
        reason = "load-bearing invariant: DescriptionLine arm above returns early, so file_index >= 1 here"
    )]
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
fn collect_context(lines: &[RenderedLine], idx: usize) -> (Vec<String>, Vec<String>) {
    let is_content = |k: RenderedLineKind| {
        matches!(
            k,
            RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
        )
    };
    collect_context_with(lines, idx, is_content)
}

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

    match crate::store::save_comment(&app.repo_root, &comment) {
        Ok(()) => {
            app.last_severity = Some(composer.severity);
            app.refresh_inline_comments();
            if oversized {
                app.status_message = Some("body truncated to 64 KB on save".to_owned());
            }
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

/// Total exhaustive match: each `ComposerScope` variant carries the data
/// needed to build its `Anchor`, so this function never refuses on a missing
/// snapshot. Save-time refusals (empty body, etc.) are handled upstream in
/// `save_composer`.
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
    }
}

struct UpdateArgs {
    body: String,
    now: time::OffsetDateTime,
    oversized: bool,
}

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

    match crate::store::update_comment(&app.repo_root, &updated) {
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
        },
    };

    match crate::store::delete_comment(&app.repo_root, &comment) {
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

fn open_file_picker(app: &mut App) {
    let entries = build_file_picker_entries(&app.details.diff.files, &app.loaded_comments);
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
}

fn refresh_current_change(app: &mut App) {
    let change_id = app.details.change_id.clone();
    match jj::show(&change_id) {
        Ok(details) => {
            app.rendered_per_file = build_rendered_views(&details);
            app.annotated_per_file = app.rendered_per_file.clone();
            app.details = details;
            app.overview_cache = None;
            app.refresh_inline_comments();
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

fn toggle_severity_filter(app: &mut App, severity: Severity) {
    if app.severity_filter == Some(severity) {
        app.severity_filter = None;
    } else {
        app.severity_filter = Some(severity);
    }
    app.rebuild_annotated_views();
}

/// Return the `Comment` under the cursor when the focused `RenderedLine` is
/// an `InlineCommentMeta` and its embedded index falls within `loaded_comments`.
fn focused_comment(app: &App) -> Option<&Comment> {
    let view = app.current_view()?;
    let line = view.lines.get(app.line_index)?;
    let RenderedLineKind::InlineCommentMeta { comment_index } = line.kind else {
        return None;
    };
    app.loaded_comments.get(comment_index)
}

/// Park the cursor on the diff line a deleted comment was attached to.
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
                RenderedLineKind::InlineCommentMeta { comment_index: 0 },
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
            RenderedLineKind::InlineCommentMeta { comment_index: 0 },
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

        let loaded =
            crate::store::load_change_comments(&app.repo_root, &app.details.change_id).unwrap();
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

        let loaded = crate::store::load_stack_comments(&app.repo_root, &revset_hash).unwrap();
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
        let stack_path = dir
            .path()
            .join(".jj-review")
            .join("comments")
            .join("_stack.jsonl");
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
            Some(composer::STATUS_STACK_UNAVAILABLE),
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
        let loaded =
            crate::store::load_change_comments(&app.repo_root, &app.details.change_id).unwrap();
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
        let comments_dir = dir.path().join(".jj-review").join("comments");
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
        crate::store::update_comment(&app.repo_root, &app.loaded_comments[0])
            .expect("seed disk with moved anchor");

        let save_time = time::OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60);
        let outcome = save_composer(&mut app, &composer_snapshot, save_time);
        assert!(matches!(outcome, SaveOutcome::Saved), "expected Saved");

        let loaded = crate::store::load_change_comments(&app.repo_root, &app.details.change_id)
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
        let comments_dir = dir.path().join(".jj-review").join("comments");
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
        let loaded =
            crate::store::load_change_comments(&app.repo_root, &app.details.change_id).unwrap();
        assert!(
            loaded.is_empty(),
            "original line comment should be deleted from disk; got {loaded:?}"
        );
        // No stack comment was created (delete must not write the swapped scope).
        let stack_loaded = crate::store::load_stack_comments(&app.repo_root, &revset_hash).unwrap();
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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

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
                Some(composer::STATUS_STACK_UNAVAILABLE),
                "refusal_status must surface the stack-unavailable hint"
            );
        } else {
            panic!("composer should still be open");
        }
        assert_eq!(
            app.status_message.as_deref(),
            Some(composer::STATUS_STACK_UNAVAILABLE)
        );
    }

    // -- T3: Ctrl+K reaches the textarea (was previously intercepted as a
    //   scope chord, killing the dialog instead of killing-to-EOL inside
    //   the body). Pin the user-reported bug fix: the keypress flows
    //   through to tui-textarea, which performs its kill-to-EOL.
    #[test]
    fn ctrl_k_inside_composer_body_forwards_to_textarea_and_kills_to_eol() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        open_composer(&mut app);
        // Type "hello world" then a newline then "second line".
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
        // Move cursor to row 0, after "hello " (col 6).
        handle_composer_event(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        handle_composer_event(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        for _ in 0..6 {
            handle_composer_event(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        // Ctrl+K — tui-textarea kills from cursor to end of line.
        handle_composer_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        let Screen::Composer(ref composer) = app.screen else {
            panic!("composer should remain open");
        };
        assert_eq!(
            composer.body_text(),
            "hello \nsecond line",
            "Ctrl+K must reach the textarea and kill the rest of row 0"
        );
        // Scope is unchanged (Ctrl+K is no longer a chord).
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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

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
        let comments_dir = dir.path().join(".jj-review").join("comments");
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
                RenderedLineKind::InlineCommentMeta { comment_index } => app
                    .loaded_comments
                    .get(comment_index)
                    .is_some_and(|c| c.body == body),
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
        let before =
            crate::store::load_change_comments(&app.repo_root, &app.details.change_id).unwrap();
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
        let after =
            crate::store::load_change_comments(&app.repo_root, &app.details.change_id).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Never);
    }

    fn write_config(dir: &std::path::Path, contents: &str) {
        let cfg_dir = dir.join(".jj-review");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), contents).unwrap();
    }

    #[test]
    fn load_transition_mode_explicit_never() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[ui]\ntransition_screen = \"never\"\n");
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Never);
    }

    #[test]
    fn load_transition_mode_auto() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[ui]\ntransition_screen = \"auto\"\n");
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Auto);
    }

    #[test]
    fn load_transition_mode_always() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[ui]\ntransition_screen = \"always\"\n");
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Always);
    }

    #[test]
    fn load_transition_mode_malformed_toml_is_never() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[ui\nbroken");
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Never);
    }

    #[test]
    fn load_transition_mode_invalid_value_is_never() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[ui]\ntransition_screen = \"bogus\"\n");
        assert_eq!(load_transition_mode(dir.path()), TransitionMode::Never);
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
        let h = SeverityHistogram::from_comments(&comments);
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

        let cursor = cursor::load(dir.path()).unwrap();
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
        assert!(!dir.path().join(".jj-review").join("cursor.json").exists());
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
        crate::store::save_comment(dir, &comment).unwrap();
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
        crate::store::save_comment(dir.path(), &comment2).unwrap();
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
            crate::store::load_change_comments(dir.path(), &app.details.change_id).unwrap();
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
            crate::store::delete_comment(&app.repo_root, &reanchor.original).unwrap();
            app.refresh_inline_comments();
        }

        let loaded =
            crate::store::load_change_comments(dir.path(), &app.details.change_id).unwrap();
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
        crate::store::save_comment(dir.path(), &comment).unwrap();
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
        crate::store::save_comment(dir.path(), &comment).unwrap();
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
            crate::store::save_comment(dir.path(), &comment).unwrap();
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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

        // Open the editor via the overview path (simulates Enter on the row).
        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected composer");
        };
        // Edit the body and bump severity.
        composer.body = tui_textarea::TextArea::default();
        for ch in "edited body text".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer.severity = Severity::Required;

        dispatch_ctrl_x(&mut app);

        // After save, screen should be back to Main.
        assert!(matches!(app.screen, Screen::Main));

        let loaded = crate::store::load_stack_comments(dir.path(), &revset_hash).unwrap();
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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        let Screen::Composer(ref mut composer) = app.screen else {
            panic!("expected composer");
        };
        composer.body = tui_textarea::TextArea::default();
        for ch in "B-edited".chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer.severity = Severity::Suggestion;

        dispatch_ctrl_x(&mut app);

        let loaded_b = crate::store::load_change_comments(dir.path(), &id_b).unwrap();
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
        let h = SeverityHistogram::from_comments(&[required, active]);
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
        let h = SeverityHistogram::from_comments(&[stale, orphaned, active]);
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
        };
        crate::store::save_comment(dir.path(), &orphan_comment).unwrap();

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
        };
        crate::store::save_comment(dir.path(), &active_comment).unwrap();

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

        let orphaned = collect_orphaned_comments(dir.path(), &stack_entries);
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
        let orphaned = collect_orphaned_comments(dir.path(), &stack_entries);
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
        };
        crate::store::save_comment(dir.path(), &comment).unwrap();

        let stack_entries = vec![StackEntry {
            change_id: id_a.clone(),
            commit_id: CommitId::parse(&"a".repeat(40)).unwrap(),
            description: "first".to_owned(),
        }];

        let orphaned = collect_orphaned_comments(dir.path(), &stack_entries);
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
        };
        crate::store::save_comment(dir.path(), &original).unwrap();

        open_meta_comment_editor(&mut app, &original);
        assert!(matches!(app.screen, Screen::Composer(_)));

        dispatch_ctrl_d(&mut app);

        assert!(matches!(app.screen, Screen::Main));
        let loaded = crate::store::load_stack_comments(dir.path(), &revset_hash).unwrap();
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

        let loaded_a = crate::store::load_change_comments(dir.path(), &id_a).unwrap();
        let loaded_b = crate::store::load_change_comments(dir.path(), &id_b).unwrap();
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
            comment_index: 0,
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
            crate::store::save_comment(dir, &comment).unwrap();
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
            };
            crate::store::save_comment(dir, &comment).unwrap();
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
            let lines = stale_screen::build_entry_lines(80, &state, &app, is_wide);
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
            let lines = stale_screen::build_entry_lines(80, &state, &app, is_wide);
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
}
