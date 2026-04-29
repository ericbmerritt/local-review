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
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::change_id::ChangeId;
use crate::comment::{
    Anchor, Comment, LineAnchor, SchemaVersion, Severity, Side, Status, CONTEXT_MAX,
};
use crate::cursor;
use crate::error::{JjrError, Result};
use crate::jj::{self, ChangeDetails};
use crate::stack::{ResolvedStack, RevsetHash, StackEntry};
use crate::util::{clamp_with_delta, page_size, truncate};

mod composer;
mod composer_overlay;
mod diff_view;
mod help_screen;

use composer::{
    default_severity, Composer, ComposerAction, ComposerScope, EditedComment, LineTarget,
};
use diff_view::{comment_to_inline, DiffView, InlineComment, RenderedLine, RenderedLineKind};

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block (one `│` on each side).
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render measures the
/// real diff area height. Overwritten by `render_main` on every frame.
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

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

/// Maximum number of `●` dots to render per severity in the transition modal.
/// Beyond this, dots are truncated with a trailing `…`; the numeric count
/// stays accurate so the user still sees the true total.
const TRANSITION_DOT_MAX: usize = 5;

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

/// Counts of comments by severity for the reviewed change.
#[derive(Debug, Default, Clone, Copy)]
struct SeverityHistogram {
    required: usize,
    suggestion: usize,
    note: usize,
}

impl SeverityHistogram {
    fn from_comments(comments: &[Comment]) -> Self {
        let mut h = Self::default();
        for c in comments {
            match c.severity {
                Severity::Required => h.required += 1,
                Severity::Suggestion => h.suggestion += 1,
                Severity::Note => h.note += 1,
            }
        }
        h
    }
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
}

impl App {
    fn new(
        details: ChangeDetails,
        repo_root: PathBuf,
        revset: String,
        stack: Option<StackContext>,
        transition_mode: TransitionMode,
    ) -> Self {
        let rendered_per_file: Vec<DiffView> =
            details.diff.files.iter().map(DiffView::from_file).collect();
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
        }
    }

    fn current_view(&self) -> Option<&DiffView> {
        self.annotated_per_file.get(self.file_index)
    }

    fn current_line_count(&self) -> usize {
        self.current_view().map_or(0, |v| v.lines.len())
    }

    fn refresh_inline_comments(&mut self) {
        match crate::store::load_change_comments(&self.repo_root, &self.details.change_id) {
            Ok(comments) => {
                self.loaded_comments = comments;
                self.comments_loaded_ok = true;
            }
            Err(e) => {
                self.status_message = Some(format!("warning: could not load comments: {e}"));
                self.loaded_comments = Vec::new();
                self.comments_loaded_ok = false;
            }
        }
        self.rebuild_annotated_views();
    }

    fn rebuild_annotated_views(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        self.annotated_per_file = self
            .rendered_per_file
            .iter()
            .enumerate()
            .map(|(file_idx, base_view)| {
                let file_path = self
                    .details
                    .diff
                    .files
                    .get(file_idx)
                    .map(|f| f.display_path().to_owned());
                let inline: Vec<InlineComment> = self
                    .loaded_comments
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, c)| comment_to_inline(c, idx, file_path.as_deref(), now))
                    .collect();
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
        let max_index = count - 1;
        self.file_index = clamp_with_delta(self.file_index, delta, max_index);
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

        self.rendered_per_file = details.diff.files.iter().map(DiffView::from_file).collect();
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

// Always draw the main view first; modals (Help, Composer, Transition) overlay
// on top so they sit visually above the diff with the same back-state preserved.
fn render(frame: &mut Frame<'_>, app: &mut App) {
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

    let width = area.width;
    let lines: Vec<TuiLine<'_>> = view
        .lines
        .iter()
        .enumerate()
        .map(|(idx, line)| render_rendered_line(line, idx == app.line_index, width))
        .collect();

    let widget = Paragraph::new(lines).scroll((app.scroll, 0));
    frame.render_widget(widget, area);
}

fn render_rendered_line(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    // Inline comment lines use a per-severity color (spec principle 6:
    // severity is color, not text). The `●` sigil in the meta line ensures
    // NO_COLOR terminals can still distinguish severity by reading the label.
    match line.kind {
        RenderedLineKind::InlineCommentMeta { .. } | RenderedLineKind::InlineCommentBody => {
            let color = match line.comment_severity {
                Some(Severity::Required) => Color::Red,
                Some(Severity::Suggestion) => Color::Yellow,
                Some(Severity::Note) => Color::DarkGray,
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
        | RenderedLineKind::Removed => {}
    }

    let prefix = match line.kind {
        RenderedLineKind::Added => "+ ",
        RenderedLineKind::Removed => "- ",
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice => "  ",
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
        | RenderedLineKind::Notice => Style::default(),
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

fn footer_text(app: &App) -> (&'static str, Style) {
    if app.status_message.is_some() {
        // Status message rendered by caller; return empty sentinel.
        ("", Style::default().fg(Color::Yellow))
    } else if focused_comment(app).is_some() {
        (
            " ↑↓ line  e edit  d delete  c new comment  ? help  q quit",
            Style::default(),
        )
    } else if app.stack.is_some() {
        (
            " ↑↓ line  Tab file  n/p revision  Enter comment  ? help  q quit",
            Style::default(),
        )
    } else {
        (
            " ↑↓ line  Tab file  c comment  ? help  q quit",
            Style::default(),
        )
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (text, style) = if let Some(msg) = app.status_message.as_deref() {
        (msg.to_owned(), Style::default().fg(Color::Yellow))
    } else {
        let (text, style) = footer_text(app);
        (text.to_owned(), style)
    };
    let widget = Paragraph::new(text).style(style);
    frame.render_widget(widget, area);
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
        }
    }
    Ok(())
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_main_key(app: &mut App, key: KeyEvent) -> Result<()> {
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
        KeyCode::Char('c') | KeyCode::Enter => open_composer(app),
        KeyCode::Char('e') => open_composer_for_edit(app),
        KeyCode::Char('d') => delete_focused_comment(app),
        KeyCode::Char('n') => app.advance_stack()?,
        KeyCode::Char('p') => app.retreat_stack()?,
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

/// Render a string of `●` dots for the transition severity summary.
///
/// Caps at [`TRANSITION_DOT_MAX`]; any overflow becomes a trailing `…`. The
/// numeric count next to the dots still tells the truth.
fn render_dots(count: usize) -> String {
    if count == 0 {
        String::new()
    } else if count <= TRANSITION_DOT_MAX {
        "●".repeat(count)
    } else {
        format!("{}…", "●".repeat(TRANSITION_DOT_MAX))
    }
}

/// Append `s` to `word` when `count != 1`. English plurals only; deliberately
/// simple — the only words this serves are "suggestion" and "note".
fn pluralize(word: &str, count: usize) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

fn handle_composer_event(app: &mut App, key: KeyEvent) {
    // Take the composer out of app.screen temporarily so we can mutate app.
    let Screen::Composer(mut composer) = std::mem::replace(&mut app.screen, Screen::Main) else {
        return;
    };

    let action = composer::handle_composer_key(&mut composer, key);

    match action {
        ComposerAction::Continue => {
            app.screen = Screen::Composer(composer);
        }
        ComposerAction::Cancel => {
            // Discard; screen already set to Main by the replace above.
        }
        ComposerAction::Save => {
            match save_composer(app, &composer, time::OffsetDateTime::now_utc()) {
                SaveOutcome::Saved => {
                    // Screen already set to Main by the replace above.
                }
                SaveOutcome::Refused(msg) | SaveOutcome::Errored(msg) => {
                    app.status_message = Some(msg);
                    // Preserve the body and selections so the reviewer can fix
                    // and retry without retyping.
                    app.screen = Screen::Composer(composer);
                }
            }
        }
        ComposerAction::Delete => {
            match delete_via_composer(app, &composer) {
                SaveOutcome::Saved => {
                    // Screen already set to Main by the replace above.
                }
                SaveOutcome::Refused(msg) | SaveOutcome::Errored(msg) => {
                    app.status_message = Some(msg);
                    app.screen = Screen::Composer(composer);
                }
            }
        }
    }
}

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

fn open_composer(app: &mut App) {
    match build_line_target(app) {
        BuildTargetResult::Ready(target) => {
            let severity = default_severity(app.last_severity);
            app.screen = Screen::Composer(Box::new(Composer::new(target, severity)));
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

    let edited = EditedComment {
        target,
        severity: comment.severity,
        body: comment.body.clone(),
        identity: comment.created_at,
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
            app.status_message = Some(format!("delete failed: {e}"));
        }
    }
}

enum BuildTargetResult {
    Ready(LineTarget),
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
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody => return BuildTargetResult::NonCommentable,
    }

    let Some(file) = app.details.diff.files.get(app.file_index) else {
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

fn build_location_from_composer(app: &App, composer: &Composer) -> (ChangeId, LineAnchor) {
    let side = pick_side(composer.target.source_line, composer.target.target_line);
    let location = LineAnchor {
        file: PathBuf::from(&composer.target.file),
        side,
        old_line: composer.target.source_line,
        new_line: composer.target.target_line,
        hunk_header: composer.target.hunk_header.clone(),
        target_text: composer.target.target_text.clone(),
        context_before: composer.target.context_before.clone(),
        context_after: composer.target.context_after.clone(),
    };
    (app.details.change_id.clone(), location)
}

fn save_composer(app: &mut App, composer: &Composer, now: time::OffsetDateTime) -> SaveOutcome {
    // Change and Stack scopes are selectable in the UI (so the picker reflects
    // the chord state) but persistence is not yet supported for those scopes.
    match composer.scope {
        ComposerScope::Line => {}
        ComposerScope::Change => {
            return SaveOutcome::Refused("change-scope not supported yet — not saved".to_owned());
        }
        ComposerScope::Stack => {
            return SaveOutcome::Refused("stack-scope not supported yet — not saved".to_owned());
        }
    }

    let body = composer.body_text();
    if body.trim().is_empty() {
        return SaveOutcome::Refused("comment body is empty — not saved".to_owned());
    }

    // Body silently truncates at the serializer; warn the reviewer so they
    // know to copy the overflow elsewhere if needed. Save proceeds.
    let oversized = body.chars().count() > crate::comment::BODY_MAX;
    let (change_id, location) = build_location_from_composer(app, composer);

    if let Some(created_at) = composer.editing {
        return persist_update_from_composer(
            app,
            composer,
            UpdateArgs {
                body,
                change_id,
                location,
                created_at,
                now,
                oversized,
            },
        );
    }

    let comment = Comment {
        schema_version: SchemaVersion,
        anchor: Anchor::Line {
            change_id,
            location,
        },
        repo_root: app.repo_root.clone(),
        revset: app.revset.clone(),
        commit_id: Some(app.details.commit_id.clone()),
        body,
        severity: composer.severity,
        created_at: now,
        updated_at: None,
        status: Some(Status::Pending),
        mismatch_reason: None,
    };

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
        Err(e) => SaveOutcome::Errored(format!("save failed: {e}")),
    }
}

struct UpdateArgs {
    body: String,
    change_id: ChangeId,
    location: LineAnchor,
    created_at: time::OffsetDateTime,
    now: time::OffsetDateTime,
    oversized: bool,
}

fn persist_update_from_composer(
    app: &mut App,
    composer: &Composer,
    args: UpdateArgs,
) -> SaveOutcome {
    let updated = Comment {
        schema_version: SchemaVersion,
        anchor: Anchor::Line {
            change_id: args.change_id,
            location: args.location,
        },
        repo_root: app.repo_root.clone(),
        revset: app.revset.clone(),
        commit_id: Some(app.details.commit_id.clone()),
        body: args.body,
        severity: composer.severity,
        created_at: args.created_at,
        updated_at: Some(args.now),
        status: Some(Status::Pending),
        mismatch_reason: None,
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
        Err(e) => SaveOutcome::Errored(format!("update failed: {e}")),
    }
}

fn delete_via_composer(app: &mut App, composer: &Composer) -> SaveOutcome {
    let Some(created_at) = composer.editing else {
        return SaveOutcome::Refused(
            "delete only available in edit mode — this is a bug".to_owned(),
        );
    };

    // `delete_comment` keys records by `(anchor, created_at)`. The other
    // `Comment` fields are unused by the store; we still build the full
    // record because the API requires it.
    let (change_id, location) = build_location_from_composer(app, composer);
    let comment = Comment {
        schema_version: SchemaVersion,
        anchor: Anchor::Line {
            change_id,
            location,
        },
        repo_root: app.repo_root.clone(),
        revset: app.revset.clone(),
        commit_id: Some(app.details.commit_id.clone()),
        body: composer.body_text(),
        severity: composer.severity,
        created_at,
        updated_at: None,
        status: Some(Status::Pending),
        mismatch_reason: None,
    };

    match crate::store::delete_comment(&app.repo_root, &comment) {
        Ok(()) => {
            app.refresh_inline_comments();
            app.status_message = Some("comment deleted".to_owned());
            SaveOutcome::Saved
        }
        Err(e) => SaveOutcome::Errored(format!("delete failed: {e}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};
    use crossterm::event::KeyModifiers;

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
        App::new(
            details,
            PathBuf::from("/repo"),
            "@".to_owned(),
            None,
            TransitionMode::Never,
        )
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
            // Replace the annotated view's first line with one of `kind` so the
            // cursor lands on it. This bypasses normal rendering — fine for a
            // build_line_target test which only reads `current_view().lines[idx]`.
            let first_line = app.annotated_per_file[0]
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
            BuildTargetResult::NonCommentable | BuildTargetResult::NoView => {
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
            BuildTargetResult::NonCommentable | BuildTargetResult::NoView => {
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
            BuildTargetResult::NonCommentable | BuildTargetResult::NoView => {
                panic!("expected Ready");
            }
        }
    }

    fn make_composer_with_body(target: LineTarget, body: &str) -> Composer {
        let mut composer = Composer::new(target, Severity::Suggestion);
        for ch in body.chars() {
            composer
                .body
                .input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        composer
    }

    #[test]
    fn save_composer_refuses_change_scope_with_message() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let mut composer = make_composer_with_body(target, "hello");
        composer.scope = ComposerScope::Change;
        let outcome = save_composer(&mut app, &composer, time::OffsetDateTime::UNIX_EPOCH);
        match outcome {
            SaveOutcome::Refused(msg) => {
                assert!(msg.contains("change-scope"), "got message: {msg}");
                assert!(msg.contains("not supported yet"), "got message: {msg}");
            }
            SaveOutcome::Saved | SaveOutcome::Errored(_) => panic!("expected Refused"),
        }
    }

    #[test]
    fn save_composer_refuses_empty_body() {
        let mut app = make_app_with_single_file(sample_diff_file());
        app.line_index = 2;
        let BuildTargetResult::Ready(target) = build_line_target(&app) else {
            panic!("expected Ready");
        };
        let composer = make_composer_with_body(target, "");
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
        let composer = make_composer_with_body(target, "first body content");
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
        let composer = make_composer_with_body(target, "test comment body");
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
            Composer::for_edit(EditedComment {
                target: c.target.clone(),
                severity: c.severity,
                body: c.body_text(),
                identity: c.editing.unwrap(),
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
        let composer = Composer::new(target, Severity::Note);
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
            Composer::for_edit(EditedComment {
                target: c.target.clone(),
                severity: c.severity,
                body: c.body_text(),
                identity: c.editing.unwrap(),
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
            Composer::for_edit(EditedComment {
                target: c.target.clone(),
                severity: c.severity,
                body: c.body_text(),
                identity: c.editing.unwrap(),
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
        let composer = make_composer_with_body(target, body_text);
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

    // ---- transition_enabled boundary tests (G3) ----

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

    // ---- load_transition_mode tests (G4) ----

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

    // ---- progress_bar_string tests (D1) ----

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

    // ---- single-change footer does not advertise n/p (H3) ----

    #[test]
    fn single_change_app_has_no_stack_context() {
        // The footer logic branches on `app.stack.is_some()`. Confirm a
        // single-change app starts with `stack = None` so the n/p hint stays
        // hidden in the footer text path.
        let app = make_app_with_single_file(sample_diff_file());
        assert!(app.stack.is_none(), "single-change app must not have stack");
    }

    // ---- SeverityHistogram (D2) ----

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

    // ---- Cursor persistence on quit (A2) ----

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

    // ---- pick_retreat_index (B) ----
    //
    // The pure helper carries the navigation contract for `p`. Side-effect
    // properties (no cursor write) follow from `retreat_stack` routing the
    // index through `load_stack_entry(idx, advance=false)`; the helper itself
    // does not touch any I/O.

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

    // ---- transition modal footer fits (A) ----

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

    // ---- render_dots cap (D) ----

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
        assert_eq!(render_dots(TRANSITION_DOT_MAX), "●●●●●");
    }

    #[test]
    fn render_dots_one_over_max_truncates_with_ellipsis() {
        assert_eq!(render_dots(TRANSITION_DOT_MAX + 1), "●●●●●…");
    }

    #[test]
    fn render_dots_far_over_max_still_truncates() {
        assert_eq!(render_dots(50), "●●●●●…");
    }

    // ---- pluralize (C) ----

    #[test]
    fn pluralize_count_one_is_singular() {
        assert_eq!(pluralize("note", 1), "note");
        assert_eq!(pluralize("suggestion", 1), "suggestion");
    }

    #[test]
    fn pluralize_count_zero_is_plural() {
        // We only ever call pluralize with count > 0 in practice (we skip the
        // span when the count is zero), but the rule "anything other than 1
        // is plural" is the safer default.
        assert_eq!(pluralize("note", 0), "notes");
    }

    #[test]
    fn pluralize_count_two_is_plural() {
        assert_eq!(pluralize("note", 2), "notes");
        assert_eq!(pluralize("suggestion", 3), "suggestions");
    }

    // ---- file_header_label ----

    #[test]
    fn file_header_label_shows_path_and_position() {
        let app = make_app_with_single_file(sample_diff_file());
        let label = file_header_label(&app);
        assert!(
            label.contains("foo.txt"),
            "label should include file path, got: {label:?}"
        );
        assert!(
            label.contains("1 of 1"),
            "label should show position of total, got: {label:?}"
        );
    }

    #[test]
    fn file_header_label_no_files_shows_placeholder() {
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
            label.contains("(no files)"),
            "empty diff should show placeholder, got: {label:?}"
        );
    }

    // ---- footer_text ----

    #[test]
    fn footer_text_stack_mode_contains_revision() {
        let mut app = make_app_with_single_file(sample_diff_file());
        let entry = StackEntry {
            change_id: app.details.change_id.clone(),
            commit_id: app.details.commit_id.clone(),
            description: String::new(),
        };
        app.stack = Some(StackContext {
            entries: vec![entry],
            current_index: 0,
            revset: "trunk()..@".to_owned(),
            revset_hash: RevsetHash::from_revset("trunk()..@"),
        });
        let (text, _style) = footer_text(&app);
        assert!(
            text.contains("n/p revision"),
            "footer should label n/p as 'revision', got: {text:?}"
        );
    }

    #[test]
    fn footer_text_single_change_mode_has_no_revision_label() {
        let app = make_app_with_single_file(sample_diff_file());
        // No stack context → single-change footer branch.
        let (text, _style) = footer_text(&app);
        assert!(
            !text.contains("n/p"),
            "single-change footer should not mention n/p, got: {text:?}"
        );
    }
}
