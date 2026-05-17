//! Help screen overlay — rendering only; each tool supplies its own body text.

use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Render the help screen over the full frame area.
///
/// `body` is the tool-specific keybinding text. `scroll` is the number of
/// lines to skip from the top; the caller manages it in response to ↑/↓ keys.
pub fn render(frame: &mut Frame<'_>, title: &str, body: &'static str, scroll: u16) {
    let area = frame.area();
    // Clear the full area first so the underlying diff view doesn't bleed through.
    frame.render_widget(Clear, area);

    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "));
    let widget = Paragraph::new(body).block(block).scroll((scroll, 0));
    frame.render_widget(widget, layout[0]);

    let footer = " Esc / q / ?  close     ↑ ↓ / j k / PgUp PgDn  scroll";
    let footer_widget = Paragraph::new(footer);
    frame.render_widget(footer_widget, layout[1]);
}
