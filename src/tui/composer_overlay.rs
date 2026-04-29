use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::comment::Severity;

use super::composer::{Composer, ComposerScope};
use super::diff_view::DiffView;

/// Composer modal overlay width (columns), capped to terminal width.
pub(super) const COMPOSER_OVERLAY_WIDTH: u16 = 72;

/// Composer modal overlay height (rows), capped to terminal height.
pub(super) const COMPOSER_OVERLAY_HEIGHT: u16 = 22;

// Per-row heights for the composer modal interior, top to bottom:
//   CONTEXT  — diff lines around the cursor (3 lines + padding)
//   SCOPE    — scope picker row + 1 spacer
//   SEVERITY — severity picker row + 1 spacer
//   BODY     — multi-line editor (consumes remaining space, min 4 rows)
//   FOOTER   — keybinding hints split across 2 lines
const CONTEXT_ROWS: u16 = 5;
const SCOPE_ROWS: u16 = 2;
const SEVERITY_ROWS: u16 = 2;
const BODY_MIN_ROWS: u16 = 4;
const FOOTER_ROWS: u16 = 2;

pub(super) fn render_composer_overlay(
    frame: &mut Frame<'_>,
    composer: &Composer,
    current_view: Option<&DiffView>,
) {
    let area = frame.area();
    let overlay = centered_rect(area, COMPOSER_OVERLAY_WIDTH, COMPOSER_OVERLAY_HEIGHT);

    frame.render_widget(Clear, overlay);

    let title = composer.title();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Double)
        .title(title.as_str());

    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let chunks = Layout::vertical([
        Constraint::Length(CONTEXT_ROWS),
        Constraint::Length(SCOPE_ROWS),
        Constraint::Length(SEVERITY_ROWS),
        Constraint::Min(BODY_MIN_ROWS),
        Constraint::Length(FOOTER_ROWS),
    ])
    .split(inner);

    render_composer_context(frame, chunks[0], composer, current_view);
    render_scope_picker(frame, chunks[1], composer);
    render_severity_picker(frame, chunks[2], composer);
    render_body_editor(frame, chunks[3], composer);
    render_composer_footer(frame, chunks[4], composer.editing.is_some());
}

fn render_composer_context(
    frame: &mut Frame<'_>,
    area: Rect,
    composer: &Composer,
    view: Option<&DiffView>,
) {
    let idx = composer.target.rendered_index;

    let context_lines: Vec<TuiLine<'_>> = if let Some(view) = view {
        let start = idx.saturating_sub(2);
        let end = (idx + 2).min(view.lines.len().saturating_sub(1));
        (start..=end)
            .filter_map(|i| view.lines.get(i))
            .map(|l| {
                let marker = if l.source_line == composer.target.source_line
                    && l.target_line == composer.target.target_line
                    && (l.source_line.is_some() || l.target_line.is_some())
                {
                    "▶ "
                } else {
                    "  "
                };
                TuiLine::from(format!("{marker}{}", l.text))
            })
            .collect()
    } else {
        Vec::new()
    };

    let widget = Paragraph::new(context_lines);
    frame.render_widget(widget, area);
}

fn render_scope_picker(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let line_mark = if composer.scope == ComposerScope::Line {
        "[x]"
    } else {
        "[ ]"
    };
    let change_mark = if composer.scope == ComposerScope::Change {
        "[x]"
    } else {
        "[ ]"
    };
    let stack_mark = if composer.scope == ComposerScope::Stack {
        "[x]"
    } else {
        "[ ]"
    };
    let text =
        format!("  scope     {line_mark} line    {change_mark} change    {stack_mark} stack");
    let widget = Paragraph::new(text.as_str());
    frame.render_widget(widget, area);
}

fn render_severity_picker(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let note_mark = if composer.severity == Severity::Note {
        "[x]"
    } else {
        "[ ]"
    };
    let sug_mark = if composer.severity == Severity::Suggestion {
        "[x]"
    } else {
        "[ ]"
    };
    let req_mark = if composer.severity == Severity::Required {
        "[x]"
    } else {
        "[ ]"
    };
    // Color the picked severity to match the inline-comment palette in
    // `tui::render_rendered_line` so the reviewer sees the same red/yellow/gray.
    let picked_color = match composer.severity {
        Severity::Required => Color::Red,
        Severity::Suggestion => Color::Yellow,
        Severity::Note => Color::DarkGray,
    };
    let picked_style = Style::default()
        .fg(picked_color)
        .add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'_>> = Vec::with_capacity(8);
    spans.push(Span::raw("  severity  "));
    if composer.severity == Severity::Note {
        spans.push(Span::styled(format!("{note_mark} note"), picked_style));
    } else {
        spans.push(Span::raw(format!("{note_mark} note")));
    }
    spans.push(Span::raw("    "));
    if composer.severity == Severity::Suggestion {
        spans.push(Span::styled(format!("{sug_mark} suggestion"), picked_style));
    } else {
        spans.push(Span::raw(format!("{sug_mark} suggestion")));
    }
    spans.push(Span::raw("    "));
    if composer.severity == Severity::Required {
        spans.push(Span::styled(format!("{req_mark} required"), picked_style));
    } else {
        spans.push(Span::raw(format!("{req_mark} required")));
    }

    let widget = Paragraph::new(TuiLine::from(spans));
    frame.render_widget(widget, area);
}

fn render_body_editor(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    frame.render_widget(&composer.body, area);
}

fn render_composer_footer(frame: &mut Frame<'_>, area: Rect, editing: bool) {
    let line1 = TuiLine::from("  ^L line  ^C change  ^K stack");
    let line2 = if editing {
        TuiLine::from("  ^1 note  ^2 suggestion  ^3 required   ^D delete  ^X save  Esc")
    } else {
        TuiLine::from("  ^1 note  ^2 suggestion  ^3 required        ^X save  Esc")
    };
    let widget = Paragraph::new(Text::from(vec![line1, line2]));
    frame.render_widget(widget, area);
}

pub(super) fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let [horiz] = Layout::horizontal([Constraint::Length(max_width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [centered] = Layout::vertical([Constraint::Length(max_height.min(area.height))])
        .flex(Flex::Center)
        .areas(horiz);
    centered
}
