//! Terminal UI for ggr: read-only PR diff viewer.
use std::io::{stdout, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

use local_review_core::diff::Diff;

use crate::error::{GgrError, Result};
use crate::gh;
use crate::pr::PrDetails;
use crate::util::{clamp_with_delta, page_size, truncate};

mod diff_view;
mod help_screen;

use diff_view::{DiffView, PairedRow, RenderedLine, RenderedLineKind};

// ── constants ─────────────────────────────────────────────────────────────────

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;
const SCROLLBAR_WIDTH: u16 = 1;
const BLOCK_BORDER_COLS: u16 = 2;
const FALLBACK_VIEWPORT_ROWS: u16 = 20;
const STACK_BAR_MIN_COLS_FOR_FILL: u16 = 80;
const STACK_PROGRESS_BAR_WIDTH: u16 = 20;

/// Body width at which `DiffMode::Auto` switches to side-by-side.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;
/// Width of the ` │ ` gutter between the two columns.
const SIDE_BY_SIDE_GUTTER_WIDTH: u16 = 3;
/// Minimum useful cells per side (prefix + 2 chars of content).
const MIN_USEFUL_SIDE_CELL_WIDTH: u16 = 4;
/// Below this body width, side-by-side falls back to unified.
const MIN_USEFUL_SIDE_BY_SIDE_WIDTH: u16 =
    SIDE_BY_SIDE_GUTTER_WIDTH + 2 * MIN_USEFUL_SIDE_CELL_WIDTH;

// ── description view types ────────────────────────────────────────────────────

struct DescLine {
    kind: DescLineKind,
    text: String,
}

#[derive(Clone, Copy)]
enum DescLineKind {
    Body,
    Separator,
    Author,
}

// ── diff mode ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Auto,
    ForceUnified,
    ForceSideBySide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveDiffMode {
    Unified,
    SideBySide,
}

fn resolve_diff_mode(pref: DiffMode, body_width: u16) -> EffectiveDiffMode {
    if body_width < MIN_USEFUL_SIDE_BY_SIDE_WIDTH {
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

// ── screen variants ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Help,
}

// ── app state ─────────────────────────────────────────────────────────────────

struct App {
    pr: PrDetails,
    /// `true` while the description page is the active view.
    showing_description: bool,
    description_lines: Vec<DescLine>,
    commit_idx: usize,
    diff: Diff,
    views: Vec<DiffView>,
    file_idx: usize,
    cursor: usize,
    scroll_offset: usize,
    viewport_rows: u16,
    /// Cached body width from the last render; used by navigation to resolve
    /// the effective layout. Set to 0 until the first draw.
    diff_body_width: u16,
    diff_mode: DiffMode,
    screen: Screen,
    status: Option<String>,
}

impl App {
    fn new(pr: PrDetails) -> Self {
        let description_lines = build_description_lines(&pr);
        Self {
            showing_description: true,
            description_lines,
            pr,
            commit_idx: 0,
            diff: Diff { files: vec![] },
            views: vec![],
            file_idx: 0,
            cursor: 0,
            scroll_offset: 0,
            viewport_rows: FALLBACK_VIEWPORT_ROWS,
            diff_body_width: 0,
            diff_mode: DiffMode::Auto,
            screen: Screen::Main,
            status: None,
        }
    }

    fn active_view(&self) -> Option<&DiffView> {
        self.views.get(self.file_idx)
    }

    fn active_line_count(&self) -> usize {
        if self.showing_description {
            return self.description_lines.len();
        }
        let Some(view) = self.active_view() else {
            return 0;
        };
        match self.effective_mode() {
            EffectiveDiffMode::Unified => view.lines.len(),
            EffectiveDiffMode::SideBySide => view.paired_rows.len(),
        }
    }

    fn effective_mode(&self) -> EffectiveDiffMode {
        resolve_diff_mode(self.diff_mode, self.diff_body_width)
    }

    fn cycle_diff_mode(&mut self) {
        if self.showing_description {
            return;
        }
        self.diff_mode = match self.diff_mode {
            DiffMode::Auto => DiffMode::ForceUnified,
            DiffMode::ForceUnified => DiffMode::ForceSideBySide,
            DiffMode::ForceSideBySide => DiffMode::Auto,
        };
        self.cursor = 0;
        self.scroll_offset = 0;
        self.status = Some(
            match self.diff_mode {
                DiffMode::Auto => "diff layout: auto",
                DiffMode::ForceUnified => "diff layout: unified",
                DiffMode::ForceSideBySide => "diff layout: side-by-side",
            }
            .to_owned(),
        );
    }

    fn load_commit(&mut self, idx: usize) -> Result<()> {
        let sha = self.pr.commits[idx].sha.clone();
        let diff = gh::fetch_commit_diff(&self.pr.repo_name, &sha, self.pr.hostname.as_deref())?;
        self.diff = diff;
        self.views = build_views(&self.diff);
        self.commit_idx = idx;
        self.file_idx = 0;
        self.cursor = 0;
        self.scroll_offset = 0;
        Ok(())
    }

    fn go_next_commit(&mut self) -> Result<()> {
        if self.showing_description {
            self.load_commit(0)?;
            self.showing_description = false;
        } else if self.commit_idx + 1 < self.pr.commits.len() {
            self.load_commit(self.commit_idx + 1)?;
        } else {
            self.status = Some("already at the last commit".to_owned());
        }
        Ok(())
    }

    fn go_prev_commit(&mut self) -> Result<()> {
        if self.showing_description {
            self.status = Some("already at the PR description".to_owned());
        } else if self.commit_idx == 0 {
            self.showing_description = true;
            self.cursor = 0;
            self.scroll_offset = 0;
        } else {
            self.load_commit(self.commit_idx - 1)?;
        }
        Ok(())
    }

    fn go_next_file(&mut self) {
        if self.showing_description || self.views.is_empty() {
            return;
        }
        let max = self.views.len() - 1;
        if self.file_idx >= max {
            self.status = Some("already at the last file".to_owned());
        } else {
            self.file_idx += 1;
            self.cursor = 0;
            self.scroll_offset = 0;
        }
    }

    fn go_prev_file(&mut self) {
        if self.showing_description {
            return;
        }
        if self.file_idx == 0 {
            self.status = Some("already at the first file".to_owned());
        } else {
            self.file_idx -= 1;
            self.cursor = 0;
            self.scroll_offset = 0;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let max = self.active_line_count().saturating_sub(1);
        self.cursor = clamp_with_delta(self.cursor, delta, max);
        self.adjust_scroll();
    }

    fn go_top(&mut self) {
        self.cursor = 0;
        self.scroll_offset = 0;
    }

    fn go_bottom(&mut self) {
        let max = self.active_line_count().saturating_sub(1);
        self.cursor = max;
        self.adjust_scroll();
    }

    fn page_down(&mut self) {
        let step = isize::try_from(page_size(self.viewport_rows)).unwrap_or(isize::MAX);
        self.move_cursor(step);
    }

    fn page_up(&mut self) {
        let step = isize::try_from(page_size(self.viewport_rows)).unwrap_or(isize::MAX);
        self.move_cursor(-step);
    }

    fn adjust_scroll(&mut self) {
        let viewport = usize::from(self.viewport_rows);
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset + viewport {
            self.scroll_offset = self.cursor + 1 - viewport;
        }
    }
}

fn build_views(diff: &Diff) -> Vec<DiffView> {
    diff.files.iter().map(DiffView::from_file).collect()
}

// ── terminal setup / teardown ─────────────────────────────────────────────────

struct TuiGuard;

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

fn enter_tui() -> Result<(Terminal<CrosstermBackend<Stdout>>, TuiGuard)> {
    enable_raw_mode().map_err(|source| GgrError::Io { source })?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).map_err(|source| GgrError::Io { source })?;
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).map_err(|e| GgrError::Io {
        source: std::io::Error::other(e),
    })?;
    Ok((terminal, TuiGuard))
}

// ── entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run(pr: PrDetails) -> Result<()> {
    let size = crossterm::terminal::size().map_err(|source| GgrError::Io { source })?;
    if size.0 < MIN_COLS {
        return Err(GgrError::TerminalTooNarrow { cols: size.0 });
    }
    if size.1 < MIN_ROWS {
        return Err(GgrError::TerminalTooShort { rows: size.1 });
    }

    let (mut terminal, _guard) = enter_tui()?;
    let mut app = App::new(pr);

    loop {
        terminal
            .draw(|f| render(f, &mut app))
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e),
            })?;

        app.status = None;

        if !event::poll(std::time::Duration::from_millis(200))
            .map_err(|source| GgrError::Io { source })?
        {
            continue;
        }

        match event::read().map_err(|source| GgrError::Io { source })? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(&mut app, key)? {
                    break;
                }
            }
            Event::Resize(cols, rows) => {
                if cols < MIN_COLS {
                    return Err(GgrError::TerminalTooNarrow { cols });
                }
                if rows < MIN_ROWS {
                    return Err(GgrError::TerminalTooShort { rows });
                }
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Paste(_) => {}
        }
    }

    Ok(())
}

// ── key handling ──────────────────────────────────────────────────────────────

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match app.screen {
        Screen::Help => {
            match key.code {
                KeyCode::Char('q' | '?') | KeyCode::Esc => app.screen = Screen::Main,
                _ => {}
            }
            return Ok(false);
        }
        Screen::Main => {}
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => app.screen = Screen::Help,
        KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::Home | KeyCode::Char('g') if key.modifiers == KeyModifiers::NONE => {
            app.go_top();
        }
        KeyCode::End | KeyCode::Char('G') => app.go_bottom(),
        KeyCode::Tab => app.go_next_file(),
        KeyCode::BackTab => app.go_prev_file(),
        KeyCode::Char('n') => app.go_next_commit()?,
        KeyCode::Char('p') => app.go_prev_commit()?,
        KeyCode::Char('|') => app.cycle_diff_mode(),
        _ => {}
    }

    Ok(false)
}

// ── rendering ─────────────────────────────────────────────────────────────────

fn render(frame: &mut Frame<'_>, app: &mut App) {
    if app.screen == Screen::Help {
        help_screen::render(frame);
        return;
    }

    let area = frame.area();

    if app.showing_description {
        // Description page: [stack bar (3), body (fill), footer (1)] — no file header.
        let layout = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

        render_stack_bar(frame, app, layout[0]);

        let body_area = layout[1];
        app.viewport_rows = body_area.height;
        app.adjust_scroll();
        render_description(frame, app, body_area);

        render_footer(frame, app, layout[2]);
    } else {
        // Commit diff page: [stack bar (3), file header (3), diff body (fill), footer (1)].
        let layout = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

        render_stack_bar(frame, app, layout[0]);
        render_file_header(frame, app, layout[1]);

        let diff_area = layout[2];
        app.viewport_rows = diff_area.height;
        app.adjust_scroll();
        render_diff(frame, app, diff_area);

        render_footer(frame, app, layout[3]);
    }
}

// ── stack bar ─────────────────────────────────────────────────────────────────

fn render_stack_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let max_title_chars = usize::from(area.width).saturating_sub(10);
    let pr_block_title = format!(
        "PR #{} — {}",
        app.pr.number,
        truncate(&app.pr.title, max_title_chars)
    );
    let block = Block::default().borders(Borders::ALL).title(pr_block_title);

    if app.showing_description {
        let total = app.pr.commits.len();
        let label = format!(
            "description  ({total} commit{})",
            if total == 1 { "" } else { "s" }
        );
        frame.render_widget(Paragraph::new(label).block(block), area);
        return;
    }

    let total = app.pr.commits.len();
    let current = app.commit_idx + 1;
    let short_sha = &app.pr.commits[app.commit_idx].short_sha;
    let commit_title = &app.pr.commits[app.commit_idx].title;

    let interior_cols = area.width.saturating_sub(BLOCK_BORDER_COLS);
    let bar_segment = if area.width >= STACK_BAR_MIN_COLS_FOR_FILL && total > 0 {
        progress_bar_string(current, total, STACK_PROGRESS_BAR_WIDTH)
    } else {
        String::new()
    };

    let text_segment = format!("{current}/{total}  {short_sha}  ");
    let used = bar_segment.chars().count() + text_segment.chars().count();
    let title_budget = usize::from(interior_cols).saturating_sub(used);
    let label = format!(
        "{bar_segment}{text_segment}{}",
        truncate(commit_title, title_budget)
    );

    frame.render_widget(Paragraph::new(label).block(block), area);
}

/// `████░░░░  ` style progress fill of fixed `width` cells, followed by two spaces.
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

// ── file header ───────────────────────────────────────────────────────────────

fn render_file_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let total = app.views.len();
    let position = app.file_idx.saturating_add(1);
    let path_label = app
        .active_view()
        .map_or_else(|| "(no files)".to_owned(), |v| v.title.clone());
    let label = format!("{path_label}  ·  {position} of {total}");
    let block = Block::default().borders(Borders::ALL).title("File");
    frame.render_widget(Paragraph::new(label).block(block), area);
}

// ── diff body ─────────────────────────────────────────────────────────────────

fn render_diff(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let scroll = u16::try_from(app.scroll_offset).unwrap_or(u16::MAX);

    let Some(view) = app.active_view() else {
        frame.render_widget(Paragraph::new("No files in this commit."), area);
        app.diff_body_width = area.width;
        return;
    };

    let probe_total = view.lines.len();
    let probe_mode = resolve_diff_mode(app.diff_mode, area.width.saturating_sub(SCROLLBAR_WIDTH));
    let row_total = match probe_mode {
        EffectiveDiffMode::Unified => probe_total,
        EffectiveDiffMode::SideBySide => view.paired_rows.len(),
    };

    let (body_area, sb_area, mut sb_state) = scrollbar_layout(area, row_total, scroll);
    let body_width = body_area.width;
    let mode = resolve_diff_mode(app.diff_mode, body_width);

    match mode {
        EffectiveDiffMode::Unified => {
            let cursor = app.cursor;
            let lines: Vec<TuiLine<'_>> = view
                .lines
                .iter()
                .enumerate()
                .map(|(idx, line)| render_rendered_line(line, idx == cursor, body_width))
                .collect();
            frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body_area);
        }
        EffectiveDiffMode::SideBySide => {
            render_diff_side_by_side(frame, body_area, view, app.cursor, scroll);
        }
    }

    render_scrollbar(frame, sb_state.as_mut(), sb_area);
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
        let cursor = cursor_row;
        let lines: Vec<TuiLine<'_>> = view
            .lines
            .iter()
            .enumerate()
            .map(|(idx, line)| render_rendered_line(line, idx == cursor, total_width))
            .collect();
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
        return;
    }

    let side_width = (total_width - SIDE_BY_SIDE_GUTTER_WIDTH) / 2;

    let lines: Vec<TuiLine<'_>> = view
        .paired_rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            render_paired_row(view, *row, row_idx == cursor_row, side_width, total_width)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

fn render_paired_row(
    view: &DiffView,
    row: PairedRow,
    focused: bool,
    side_width: u16,
    full_width: u16,
) -> TuiLine<'_> {
    match row {
        PairedRow::Spanning(idx) => {
            let Some(line) = view.lines.get(idx) else {
                return TuiLine::raw("");
            };
            render_full_width_row(line, focused, full_width)
        }
        PairedRow::Pair { left, right } => {
            let left_spans = match left.and_then(|i| view.lines.get(i)) {
                Some(line) => side_cell_spans(line, side_width, focused),
                None => blank_cell_spans(side_width, focused),
            };
            let right_spans = match right.and_then(|i| view.lines.get(i)) {
                Some(line) => side_cell_spans(line, side_width, focused),
                None => blank_cell_spans(side_width, focused),
            };
            TuiLine::from([left_spans, side_by_side_gutter_spans(), right_spans].concat())
        }
    }
}

fn render_full_width_row(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    let (body, fg) = prefix_truncate_pad(line, width);
    TuiLine::from(vec![Span::styled(body, focus_style(fg, focused))])
}

fn side_cell_spans(line: &RenderedLine, side_width: u16, focused: bool) -> Vec<Span<'_>> {
    let (body, fg) = prefix_truncate_pad(line, side_width);
    vec![Span::styled(body, focus_style(fg, focused))]
}

fn blank_cell_spans<'a>(side_width: u16, focused: bool) -> Vec<Span<'a>> {
    let body = " ".repeat(usize::from(side_width));
    vec![Span::styled(body, focus_style(Color::Reset, focused))]
}

fn side_by_side_gutter_spans<'a>() -> Vec<Span<'a>> {
    vec![Span::styled(
        " \u{2502} ",
        Style::default().fg(Color::DarkGray),
    )]
}

/// Assemble `prefix + truncated text + space-padding` to exactly `width` cells.
/// Returns `(body, fg_color)`; callers apply focus via [`focus_style`].
fn prefix_truncate_pad(line: &RenderedLine, width: u16) -> (String, Color) {
    let attrs = line_visual_attrs(line);
    let prefix_chars = attrs.prefix.chars().count();
    let max_text = usize::from(width).saturating_sub(prefix_chars);
    let text = truncate(&line.text, max_text);
    let used = prefix_chars + text.chars().count();
    let pad = usize::from(width).saturating_sub(used);
    (
        format!("{}{}{}", attrs.prefix, text, " ".repeat(pad)),
        attrs.fg_color,
    )
}

struct LineVisual {
    prefix: &'static str,
    fg_color: Color,
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
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice => LineVisual {
            prefix: "  ",
            fg_color: Color::Reset,
        },
    }
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

/// Render one unified-mode diff line. Focused rows are padded to `width` and
/// reversed; unfocused rows use the natural prefix + text length.
fn render_rendered_line(line: &RenderedLine, focused: bool, width: u16) -> TuiLine<'_> {
    if focused {
        let (body, fg) = prefix_truncate_pad(line, width);
        TuiLine::from(vec![Span::styled(body, focus_style(fg, true))])
    } else {
        let attrs = line_visual_attrs(line);
        TuiLine::from(vec![
            Span::raw(attrs.prefix),
            Span::styled(line.text.as_str(), focus_style(attrs.fg_color, false)),
        ])
    }
}

// ── description body ──────────────────────────────────────────────────────────

fn render_description(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let scroll = u16::try_from(app.scroll_offset).unwrap_or(u16::MAX);
    let total = app.description_lines.len();

    let block = Block::default().borders(Borders::ALL).title("Description");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (body_area, sb_area, mut sb_state) = scrollbar_layout(inner, total, scroll);
    let lines: Vec<TuiLine<'_>> = app.description_lines.iter().map(render_desc_line).collect();
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body_area);
    render_scrollbar(frame, sb_state.as_mut(), sb_area);
}

fn render_desc_line(dl: &DescLine) -> TuiLine<'_> {
    match dl.kind {
        DescLineKind::Separator => TuiLine::from(Span::styled(
            dl.text.as_str(),
            Style::default().fg(Color::DarkGray),
        )),
        DescLineKind::Author => TuiLine::from(Span::styled(
            dl.text.as_str(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        DescLineKind::Body => TuiLine::raw(dl.text.as_str()),
    }
}

fn build_description_lines(pr: &PrDetails) -> Vec<DescLine> {
    let mut lines = Vec::new();

    if pr.body.is_empty() {
        lines.push(DescLine {
            kind: DescLineKind::Body,
            text: "(no description)".to_owned(),
        });
    } else {
        for line in pr.body.lines() {
            lines.push(DescLine {
                kind: DescLineKind::Body,
                text: line.to_owned(),
            });
        }
    }

    if !pr.comments.is_empty() {
        let n = pr.comments.len();
        lines.push(DescLine {
            kind: DescLineKind::Body,
            text: String::new(),
        });
        lines.push(DescLine {
            kind: DescLineKind::Separator,
            text: format!("── {} comment{} ──", n, if n == 1 { "" } else { "s" }),
        });
        for comment in &pr.comments {
            lines.push(DescLine {
                kind: DescLineKind::Body,
                text: String::new(),
            });
            lines.push(DescLine {
                kind: DescLineKind::Author,
                text: format!("@{}:", comment.author),
            });
            for line in comment.body.lines() {
                lines.push(DescLine {
                    kind: DescLineKind::Body,
                    text: line.to_owned(),
                });
            }
        }
    }

    lines
}

// ── scrollbar helpers ─────────────────────────────────────────────────────────

fn scrollbar_layout(
    area: Rect,
    total_lines: usize,
    scroll: u16,
) -> (Rect, Option<Rect>, Option<ScrollbarState>) {
    let viewport = usize::from(area.height);
    let sb_state = if viewport > 0 && total_lines > viewport {
        let max_scroll = total_lines - viewport;
        let pos = usize::from(scroll).min(max_scroll);
        Some(ScrollbarState::new(max_scroll + 1).position(pos))
    } else {
        None
    };
    let (body, sb_area) = if sb_state.is_some() && area.width > SCROLLBAR_WIDTH {
        let split = Layout::horizontal([Constraint::Min(0), Constraint::Length(SCROLLBAR_WIDTH)])
            .split(area);
        (split[0], Some(split[1]))
    } else {
        (area, None)
    };
    (body, sb_area, sb_state)
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    sb_state: Option<&mut ScrollbarState>,
    sb_area: Option<Rect>,
) {
    if let (Some(state), Some(area)) = (sb_state, sb_area) {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .track_style(Style::default().fg(Color::DarkGray))
            .thumb_style(Style::default().fg(Color::Gray))
            .begin_style(Style::default().fg(Color::DarkGray))
            .end_style(Style::default().fg(Color::DarkGray));
        frame.render_stateful_widget(scrollbar, area, state);
    }
}

// ── footer ────────────────────────────────────────────────────────────────────

const FOOTER_IRREDUCIBLE: &str = " \u{2191}\u{2193} line  Tab file  n/p commit";

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (text, style) = if let Some(msg) = app.status.as_deref() {
        (msg.to_owned(), Style::default().fg(Color::Yellow))
    } else {
        (footer_text(area.width), Style::default())
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn footer_text(width: u16) -> String {
    let width = usize::from(width);
    let optional: &[&str] = &["  |", "  ?", "  q quit"];
    let mut text = FOOTER_IRREDUCIBLE.to_owned();
    let mut used = text.chars().count();
    for seg in optional {
        let seg_len = seg.chars().count();
        if used + seg_len <= width {
            text.push_str(seg);
            used += seg_len;
        }
    }
    text
}
