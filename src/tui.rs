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
use crate::comment::{Anchor, LineAnchor, SchemaVersion, Severity, Side, Status};
use crate::error::{JjrError, Result};
use crate::jj::{self, ChangeDetails};
use crate::util::{clamp_with_delta, page_size, truncate};

mod composer;
mod composer_overlay;
mod diff_view;
mod help_screen;

use composer::{default_severity, Composer, ComposerAction, ComposerScope, LineTarget};
use diff_view::{comment_to_inline, DiffView, InlineComment, RenderedLine, RenderedLineKind};

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block (one `│` on each side).
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render measures the
/// real diff area height. Overwritten by `render_main` on every frame.
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

pub fn run(change_id: &ChangeId) -> Result<()> {
    let details = jj::show(change_id)?;
    let repo_root = std::env::current_dir().map_err(|source| JjrError::Io { source })?;
    let revset = change_id.as_str().to_owned();

    let mut terminal = setup_terminal()?;
    let outcome = run_app(&mut terminal, details, repo_root, revset);
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
    loaded_comments: Vec<crate::comment::Comment>,
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
}

impl App {
    fn new(details: ChangeDetails, repo_root: PathBuf, revset: String) -> Self {
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
            }
            Err(e) => {
                self.status_message = Some(format!("warning: could not load comments: {e}"));
                self.loaded_comments = Vec::new();
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
                    .filter_map(|c| comment_to_inline(c, file_path.as_deref(), now))
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
        // Skip non-navigable lines: hunk separators and injected comment lines.
        let step: isize = if delta >= 0 { 1 } else { -1 };
        while next > 0
            && next < max_index
            && self.current_view().is_some_and(|v| {
                matches!(
                    v.lines[next].kind,
                    RenderedLineKind::HunkSeparator
                        | RenderedLineKind::InlineCommentMeta
                        | RenderedLineKind::InlineCommentBody
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
) -> Result<()> {
    let mut app = App::new(details, repo_root, revset);
    app.refresh_inline_comments();

    while !app.should_quit {
        terminal
            .draw(|frame| render(frame, &mut app))
            .map_err(io_err)?;
        handle_event(&mut app)?;
    }

    Ok(())
}

// Always draw the main view first; modals (Help, Composer) overlay on top so
// they sit visually above the diff with the same back-state preserved.
fn render(frame: &mut Frame<'_>, app: &mut App) {
    render_main(frame, app);
    match &app.screen {
        Screen::Main => {}
        Screen::Help => help_screen::render(frame),
        Screen::Composer(composer) => {
            composer_overlay::render_composer_overlay(frame, composer, app.current_view());
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

    render_stack_bar(frame, layout[0], &app.details);
    render_file_header(frame, layout[1], app);

    let diff_area = layout[2];
    let viewport_rows = diff_area.height;
    app.viewport_rows = viewport_rows;
    app.ensure_cursor_visible(viewport_rows);
    render_diff(frame, diff_area, app);

    render_footer(frame, layout[3], app.status_message.as_deref());
}

fn render_stack_bar(frame: &mut Frame<'_>, area: Rect, details: &ChangeDetails) {
    let prefix = format!("1/1  {}  ", details.change_id);
    let prefix_width = prefix.chars().count();
    let desc_budget =
        usize::from(area.width.saturating_sub(BLOCK_BORDER_COLS)).saturating_sub(prefix_width);
    let label = format!("{}{}", prefix, truncate(&details.description, desc_budget));
    let block = Block::default().borders(Borders::ALL).title("Stack");
    let widget = Paragraph::new(label).block(block);
    frame.render_widget(widget, area);
}

fn render_file_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let total = app.rendered_per_file.len();
    let position = app.file_index.saturating_add(1);
    let path_label = app
        .current_view()
        .map_or_else(|| "(no files)".to_owned(), |v| v.title.clone());
    let label = format!("{path_label}  ·  {position} of {total}");
    let block = Block::default().borders(Borders::ALL);
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
    // Inline comment lines are never focused and use a per-severity color
    // (spec principle 6: severity is color, not text). Severity is paired with
    // the `●` sigil in the meta line so NO_COLOR terminals still distinguish.
    match line.kind {
        RenderedLineKind::InlineCommentMeta | RenderedLineKind::InlineCommentBody => {
            let color = match line.comment_severity {
                Some(Severity::Required) => Color::Red,
                Some(Severity::Suggestion) => Color::Yellow,
                Some(Severity::Note) => Color::DarkGray,
                None => Color::Cyan,
            };
            return TuiLine::from(vec![Span::styled(
                line.text.as_str(),
                Style::default().fg(color),
            )]);
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
        RenderedLineKind::InlineCommentMeta | RenderedLineKind::InlineCommentBody => {
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
        RenderedLineKind::InlineCommentMeta | RenderedLineKind::InlineCommentBody => {
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

fn render_footer(frame: &mut Frame<'_>, area: Rect, status: Option<&str>) {
    let text = status.unwrap_or(" ↑↓ line  Tab file  c comment  ? help  q quit");
    let style = if status.is_some() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
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
            Screen::Main => handle_main_key(app, key),
            Screen::Help => handle_help_key(app, key),
            Screen::Composer(_) => handle_composer_event(app, key),
        }
    }
    Ok(())
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_main_key(app: &mut App, key: KeyEvent) {
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
        _ => {}
    }
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q' | '?') | KeyCode::Esc) {
        app.screen = Screen::Main;
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
        | RenderedLineKind::InlineCommentMeta
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

/// Collect up to 3 lines of context before and after `idx` in `lines`,
/// skipping non-diff lines (hunk headers, separators, inline comments).
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
        .take(3)
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let after: Vec<String> = lines[idx + 1..]
        .iter()
        .filter(|l| is_content(l.kind))
        .take(3)
        .map(|l| l.text.clone())
        .collect();

    (before, after)
}

fn save_composer(app: &mut App, composer: &Composer, now: time::OffsetDateTime) -> SaveOutcome {
    // Phase 2 only persists Line-scope comments; Change/Stack scopes are
    // selectable in the UI (so the picker reflects the chord state) but their
    // anchor types land in a later phase. Refuse to save with a status
    // message rather than silently coercing the scope.
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

    let side = pick_side(composer.target.source_line, composer.target.target_line);

    let change_id = app.details.change_id.clone();
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

    let comment = crate::comment::Comment {
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
            make_line(RenderedLineKind::InlineCommentMeta, "┃ meta", None, None),
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
        App::new(details, PathBuf::from("/repo"), "@".to_owned())
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
            RenderedLineKind::InlineCommentMeta,
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
}
