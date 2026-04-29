use std::io::{stdout, Stdout};

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
use crate::error::{JjrError, Result};
use crate::jj::{self, ChangeDetails};
use crate::util::{clamp_with_delta, page_size, truncate};

mod diff_view;
mod help_screen;

use diff_view::{DiffView, RenderedLine, RenderedLineKind};

const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 10;

/// Column chars consumed by a `Borders::ALL` block (one `│` on each side).
const BLOCK_BORDER_COLS: u16 = 2;

/// Initial value for `App::viewport_rows` before the first render measures the
/// real diff area height. Overwritten by `render_main` on every frame.
const FALLBACK_VIEWPORT_ROWS: u16 = 20;

pub fn run(change_id: &ChangeId) -> Result<()> {
    let details = jj::show(change_id)?;

    let mut terminal = setup_terminal()?;
    let outcome = run_app(&mut terminal, details);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Help,
}

struct App {
    details: ChangeDetails,
    rendered_per_file: Vec<DiffView>,
    file_index: usize,
    line_index: usize,
    scroll: u16,
    screen: Screen,
    should_quit: bool,
    /// Cached viewport height (set during `render_main`, read in `handle_main_key`).
    /// Overwritten on first render before any key event is processed.
    viewport_rows: u16,
}

impl App {
    fn new(details: ChangeDetails) -> Self {
        let rendered_per_file = details.diff.files.iter().map(DiffView::from_file).collect();
        Self {
            details,
            rendered_per_file,
            file_index: 0,
            line_index: 0,
            scroll: 0,
            screen: Screen::Main,
            should_quit: false,
            viewport_rows: FALLBACK_VIEWPORT_ROWS,
        }
    }

    fn current_view(&self) -> Option<&DiffView> {
        self.rendered_per_file.get(self.file_index)
    }

    fn current_line_count(&self) -> usize {
        self.current_view().map_or(0, |v| v.lines.len())
    }

    fn move_line(&mut self, delta: isize) {
        let count = self.current_line_count();
        if count == 0 {
            return;
        }
        let max_index = count - 1;
        let mut next = clamp_with_delta(self.line_index, delta, max_index);
        // Skip hunk separators — they are visual dividers, not navigable lines.
        let step: isize = if delta >= 0 { 1 } else { -1 };
        while next > 0
            && next < max_index
            && self
                .current_view()
                .is_some_and(|v| v.lines[next].kind == RenderedLineKind::HunkSeparator)
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

fn run_app(terminal: &mut Term, details: ChangeDetails) -> Result<()> {
    let mut app = App::new(details);

    while !app.should_quit {
        terminal
            .draw(|frame| render(frame, &mut app))
            .map_err(io_err)?;
        handle_event(&mut app)?;
    }

    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    match app.screen {
        Screen::Main => render_main(frame, app),
        Screen::Help => help_screen::render(frame),
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

    render_footer(frame, layout[3]);
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
    let prefix = match line.kind {
        RenderedLineKind::Added => "+ ",
        RenderedLineKind::Removed => "- ",
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice => "  ",
    };

    let content_style = match line.kind {
        RenderedLineKind::Added => Style::default().fg(Color::Green),
        RenderedLineKind::Removed => Style::default().fg(Color::Red),
        RenderedLineKind::HunkHeader
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Context
        | RenderedLineKind::Notice => Style::default(),
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

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    let footer = " ↑↓ line   Tab file   ? help   q quit";
    let widget = Paragraph::new(footer);
    frame.render_widget(widget, area);
}

fn handle_event(app: &mut App) -> Result<()> {
    let evt = event::read().map_err(io_err)?;
    if let Event::Key(key) = evt {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match app.screen {
            Screen::Main => handle_main_key(app, key),
            Screen::Help => handle_help_key(app, key),
        }
    }
    Ok(())
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled KeyCode variants are intentionally ignored"
)]
fn handle_main_key(app: &mut App, key: KeyEvent) {
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
        _ => {}
    }
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q' | '?') | KeyCode::Esc) {
        app.screen = Screen::Main;
    }
}
