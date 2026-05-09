//! Terminal UI for `ggr` Phase 1: read-only PR diff viewer.
use std::io::{stdout, Stdout};
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::{Frame, Terminal};

use local_review_core::diff::Diff;

use crate::error::{GgrError, Result};
use crate::gh;
use crate::pr::PrDetails;
use crate::util::{clamp_with_delta, page_size, truncate};

mod diff_view;
mod help_screen;

use diff_view::{DiffView, RenderedLine, RenderedLineKind};

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;
const SCROLLBAR_WIDTH: u16 = 1;
const FALLBACK_VIEWPORT_ROWS: u16 = 20;
const STACK_BAR_MIN_COLS_FOR_FILL: u16 = 80;
const STACK_PROGRESS_BAR_WIDTH: u16 = 20;

// ── Screen variants ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Help,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct App {
    pr: PrDetails,
    repo_root: PathBuf,
    /// Current commit index within `pr.commits`.
    commit_idx: usize,
    /// Diff for the currently loaded commit.
    diff: Diff,
    /// Views built from `diff.files`; `file_idx` selects the active view.
    views: Vec<DiffView>,
    /// Index into `views`. 0 is always the first file (no separate description
    /// view in ggr Phase 1 — commits have a title in the stack bar only).
    file_idx: usize,
    /// Cursor row within the active view (0-based index into `views[file_idx].lines`).
    cursor: usize,
    /// Scroll offset: first visible line of the diff body.
    scroll_offset: usize,
    /// Measured diff-body height in rows; updated every render.
    viewport_rows: u16,
    screen: Screen,
    /// Transient one-frame status hint shown in the footer.
    status: Option<String>,
}

impl App {
    fn new(pr: PrDetails, initial_diff: Diff, repo_root: PathBuf) -> Self {
        let views = build_views(&initial_diff);
        Self {
            pr,
            repo_root,
            commit_idx: 0,
            diff: initial_diff,
            views,
            file_idx: 0,
            cursor: 0,
            scroll_offset: 0,
            viewport_rows: FALLBACK_VIEWPORT_ROWS,
            screen: Screen::Main,
            status: None,
        }
    }

    fn active_view(&self) -> Option<&DiffView> {
        self.views.get(self.file_idx)
    }

    fn active_line_count(&self) -> usize {
        self.active_view().map_or(0, |v| v.lines.len())
    }

    fn load_commit(&mut self, idx: usize) -> Result<()> {
        let sha = self.pr.commits[idx].sha.clone();
        let diff = gh::fetch_commit_diff(&self.repo_root, &sha)?;
        self.diff = diff;
        self.views = build_views(&self.diff);
        self.commit_idx = idx;
        self.file_idx = 0;
        self.cursor = 0;
        self.scroll_offset = 0;
        Ok(())
    }

    fn go_next_commit(&mut self) -> Result<()> {
        if self.commit_idx + 1 < self.pr.commits.len() {
            self.load_commit(self.commit_idx + 1)?;
        } else {
            self.status = Some("already at the last commit".to_owned());
        }
        Ok(())
    }

    fn go_prev_commit(&mut self) -> Result<()> {
        if self.commit_idx > 0 {
            self.load_commit(self.commit_idx - 1)?;
        } else {
            self.status = Some("already at the first commit".to_owned());
        }
        Ok(())
    }

    fn go_next_file(&mut self) {
        if self.views.is_empty() {
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

// ── Terminal setup / teardown ─────────────────────────────────────────────────

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

// ── Entry point ───────────────────────────────────────────────────────────────

/// Open the TUI for `pr`.
pub(crate) fn run(pr: PrDetails, initial_diff: Diff, repo_root: PathBuf) -> Result<()> {
    let size = crossterm::terminal::size().map_err(|source| GgrError::Io { source })?;
    if size.0 < MIN_COLS {
        return Err(GgrError::TerminalTooNarrow { cols: size.0 });
    }
    if size.1 < MIN_ROWS {
        return Err(GgrError::TerminalTooShort { rows: size.1 });
    }

    let (mut terminal, _guard) = enter_tui()?;
    let mut app = App::new(pr, initial_diff, repo_root);

    loop {
        terminal
            .draw(|f| render(f, &mut app))
            .map_err(|e| GgrError::Io {
                source: std::io::Error::other(e),
            })?;

        // Clear one-frame status after render.
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

/// Returns `true` when the app should quit.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match app.screen {
        Screen::Help => {
            match key.code {
                KeyCode::Char('q' | '?') | KeyCode::Esc => {
                    app.screen = Screen::Main;
                }
                _ => {}
            }
            return Ok(false);
        }
        Screen::Main => {}
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => {
            app.screen = Screen::Help;
        }
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
        _ => {}
    }

    Ok(false)
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(frame: &mut Frame<'_>, app: &mut App) {
    if app.screen == Screen::Help {
        help_screen::render(frame);
        return;
    }

    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    render_stack_bar(frame, app, layout[0]);
    render_diff_body(frame, app, layout[1]);
    render_footer(frame, app, layout[2]);
}

fn render_stack_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let total = app.pr.commits.len();
    let current = app.commit_idx + 1;
    let short_sha = &app.pr.commits[app.commit_idx].short_sha;
    let commit_title = &app.pr.commits[app.commit_idx].title;

    let cols = usize::from(area.width);

    // "#42 PR Title (base←head)  [====    ]  1/3  sha  commit title"
    // Narrow (<80): drop the progress bar fill and PR title
    let pr_title_budget = if cols >= 120 {
        40_usize
    } else if cols >= 80 {
        20_usize
    } else {
        0_usize
    };
    let pr_tag = if pr_title_budget > 0 {
        format!(
            "#{} {}  ({}←{})",
            app.pr.number,
            truncate(&app.pr.title, pr_title_budget),
            app.pr.base_ref,
            app.pr.head_ref,
        )
    } else {
        format!(
            "#{} ({}←{})",
            app.pr.number, app.pr.base_ref, app.pr.head_ref
        )
    };
    let pos = format!("{current}/{total}");

    let mut spans: Vec<Span<'_>> = Vec::new();

    spans.push(Span::styled(
        pr_tag,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    if cols >= usize::from(STACK_BAR_MIN_COLS_FOR_FILL) {
        let filled = if total > 1 {
            usize::from(STACK_PROGRESS_BAR_WIDTH) * (current - 1) / (total - 1)
        } else {
            usize::from(STACK_PROGRESS_BAR_WIDTH)
        };
        let empty = usize::from(STACK_PROGRESS_BAR_WIDTH) - filled;
        let bar = format!("  [{}{}]", "=".repeat(filled), " ".repeat(empty));
        spans.push(Span::styled(bar, Style::default().fg(Color::DarkGray)));
    }

    spans.push(Span::raw(format!("  {pos}  ")));
    spans.push(Span::styled(
        short_sha.clone(),
        Style::default().fg(Color::Yellow),
    ));
    spans.push(Span::raw("  "));

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let budget = cols.saturating_sub(used);
    let commit_title_truncated = truncate(commit_title, budget);
    spans.push(Span::raw(commit_title_truncated));

    frame.render_widget(TuiLine::from(spans), area);
}

fn render_diff_body(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let body_width = area.width.saturating_sub(SCROLLBAR_WIDTH);

    // File header
    let header_height: u16 = 1;
    let body_area = Rect {
        y: area.y + header_height,
        height: area.height.saturating_sub(header_height),
        ..area
    };

    // Render file header
    let file_title = app.active_view().map_or("<no files>", |v| v.title.as_str());
    let file_count = app.views.len();
    let file_pos = if file_count > 0 {
        format!(" {}/{file_count}  {file_title}", app.file_idx + 1)
    } else {
        " (no files)".to_owned()
    };
    let header_widget = Paragraph::new(file_pos).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    let header_area = Rect {
        height: header_height,
        ..area
    };
    frame.render_widget(header_widget, header_area);

    // Measure viewport (after removing the file header row)
    app.viewport_rows = body_area.height;
    let viewport = usize::from(body_area.height);

    if app.active_line_count() == 0 {
        let notice = Paragraph::new("(no diff — empty commit)");
        frame.render_widget(notice, body_area);
        return;
    }

    // Ensure cursor is in view (may have changed due to resize)
    app.adjust_scroll();

    let Some(view) = app.active_view() else {
        return;
    };

    let visible: Vec<TuiLine<'_>> = view
        .lines
        .iter()
        .skip(app.scroll_offset)
        .take(viewport)
        .enumerate()
        .map(|(row_in_viewport, line)| {
            let abs_idx = app.scroll_offset + row_in_viewport;
            let is_cursor = abs_idx == app.cursor;
            render_diff_line(line, body_width, is_cursor)
        })
        .collect();

    let paragraph = Paragraph::new(visible);
    let content_area = Rect {
        width: body_width,
        ..body_area
    };
    frame.render_widget(paragraph, content_area);

    // Scrollbar
    let total_lines = view.lines.len();
    if total_lines > viewport {
        let mut scrollbar_state = ScrollbarState::new(total_lines).position(app.scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let scrollbar_area = Rect {
            x: body_area.x + body_width,
            width: SCROLLBAR_WIDTH,
            ..body_area
        };
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_diff_line(line: &RenderedLine, body_width: u16, is_cursor: bool) -> TuiLine<'_> {
    let (prefix, fg, bg) = match line.kind {
        RenderedLineKind::Added => (
            "+",
            Color::Green,
            if is_cursor {
                Color::DarkGray
            } else {
                Color::Reset
            },
        ),
        RenderedLineKind::Removed => (
            "-",
            Color::Red,
            if is_cursor {
                Color::DarkGray
            } else {
                Color::Reset
            },
        ),
        RenderedLineKind::Context => (
            " ",
            Color::Reset,
            if is_cursor {
                Color::DarkGray
            } else {
                Color::Reset
            },
        ),
        RenderedLineKind::HunkHeader => (
            "",
            Color::Cyan,
            if is_cursor {
                Color::DarkGray
            } else {
                Color::Reset
            },
        ),
        RenderedLineKind::HunkSeparator | RenderedLineKind::Notice => {
            ("", Color::DarkGray, Color::Reset)
        }
    };

    // Line number gutter (6 chars: 3 old + space + 3 new)
    let gutter_width: u16 = 7; // "123 456 "
    let gutter = match line.kind {
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice => " ".repeat(usize::from(gutter_width)),
        RenderedLineKind::Context | RenderedLineKind::Added | RenderedLineKind::Removed => {
            let old = line
                .source_line
                .map_or_else(|| "   ".to_owned(), |n| format!("{n:>3}"));
            let new = line
                .target_line
                .map_or_else(|| "   ".to_owned(), |n| format!("{n:>3}"));
            format!("{old} {new} ")
        }
    };

    let max_text = usize::from(body_width)
        .saturating_sub(gutter_width.into())
        .saturating_sub(1); // prefix char

    let text = truncate(&line.text, max_text);

    let style = Style::default().fg(fg).bg(bg);
    let cursor_style = if is_cursor {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    };

    TuiLine::from(vec![
        Span::styled(gutter, Style::default().fg(Color::DarkGray).bg(bg)),
        Span::styled(format!("{prefix}{text}"), cursor_style),
    ])
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let cols = usize::from(area.width);

    let status = app.status.as_deref().unwrap_or("");

    // Build footer segments right-to-left (drop segments if they don't fit).
    // Minimum: arrow/jk + Tab + q
    let min_keys = "↑↓/jk line   Tab file   q quit";
    let with_nav = "n/p commit   ↑↓/jk line   Tab file   q quit";
    let full = "n/p commit   ↑↓/jk line   Tab file   ?   q quit";

    let keys = if cols >= full.len() + status.len() + 2 {
        full
    } else if cols >= with_nav.len() + status.len() + 2 {
        with_nav
    } else {
        min_keys
    };

    let text = if status.is_empty() {
        keys.to_owned()
    } else {
        let budget = cols.saturating_sub(keys.len() + 2);
        let s = truncate(status, budget);
        format!("{s}  {keys}")
    };

    let widget = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(widget, area);
}
