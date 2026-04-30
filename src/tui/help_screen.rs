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
    n       p             next / previous revision (stack mode)

Comments
    Enter   c             new comment on current line
    e                     edit (cursor on a comment)
    d                     delete (cursor on a comment)

Views
    S                     stale comments view
    ?                     this help
    q                     quit

Stale comments view
    ↑ ↓     k j           select entry
    Enter                 view in source (navigate main view to anchor)
    d                     delete focused stale comment
    e                     edit & re-anchor (switch to main, pick new line)
    q   Esc               back to main view

Re-anchor mode (after pressing e in stale view)
    c   Enter             open composer at current line (pre-filled body)
    Esc                   cancel re-anchor mode

In comment composer
    ^L ^C ^K              scope:    line / change / stack
    ^1 ^2 ^3              severity: note / suggestion / required
    ^X                    save
    ^D                    delete (edit mode only)
    Esc                   cancel  (^C inside the composer is captured as scope)
";
