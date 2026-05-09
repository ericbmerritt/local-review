//! Help screen (Screen 7 equivalent) for ggr Phase 1.
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub(crate) fn render(frame: &mut Frame<'_>) {
    let area = frame.area();
    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("ggr · keybindings");
    let widget = Paragraph::new(HELP_BODY)
        .block(block)
        .alignment(Alignment::Left);
    frame.render_widget(widget, layout[0]);

    let footer = " Esc / q / ?   close help";
    frame.render_widget(Paragraph::new(footer), layout[1]);
}

const HELP_BODY: &str = "
Movement
    ↑ ↓     k j           line
    PgUp PgDn             page
    Home End  g G         top / bottom
    Tab     S-Tab         next / previous file

Navigation
    n       p             next / previous commit in PR

Views
    ?                     this help
    q                     quit
";

#[cfg(test)]
mod tests {
    use super::HELP_BODY;

    #[test]
    fn help_body_documents_commit_navigation() {
        assert!(
            HELP_BODY.contains("next / previous commit"),
            "help must document n/p commit navigation"
        );
    }

    #[test]
    fn help_body_documents_file_navigation() {
        assert!(
            HELP_BODY.contains("next / previous file"),
            "help must document Tab file navigation"
        );
    }
}
