use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub(crate) fn render(frame: &mut Frame<'_>) {
    let area = frame.area();
    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let body = HELP_BODY;
    let block = Block::default()
        .borders(Borders::ALL)
        .title("jjr · keybindings");
    let widget = Paragraph::new(body).block(block).alignment(Alignment::Left);
    frame.render_widget(widget, layout[0]);

    let footer = " Esc / q / ?   close help";
    let footer_widget = Paragraph::new(footer);
    frame.render_widget(footer_widget, layout[1]);
}

const HELP_BODY: &str = "
Movement
    ↑ ↓     k j           line
    PgUp PgDn             page
    Home End  g G         top / bottom
    Tab     S-Tab         next / previous file

Views
    ?                     this help
    q                     quit
";
