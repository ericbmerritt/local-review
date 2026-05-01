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
                          (on a description line: opens description-scoped comment)
    e                     edit (cursor on a comment)
    d                     delete (cursor on a comment)
    1                     filter to required only (press again to clear)
    2                     filter to suggestion only (press again to clear)
    3                     filter to note only (press again to clear)

Views
    f                     file picker — jump to a file or the description
    r                     refresh diff and comments for current change
    s                     stack overview (stack mode only)
    S                     stale comments view
    ?                     this help
    q                     quit

Actions
    C                     send current change to Claude

Stack overview  (press s from main view)
    ↑ ↓     k j           select row
    Enter                 open change (on change row) / edit comment (on comment row)
    c                     new comment; scope defaults from cursor
                          (stack header → stack scope; change row → change scope)
    ▶                     selection cursor
    ▸                     change loaded in the main view
    q   Esc               back to main view

Stale comments view
    ↑ ↓     k j           select entry
    Enter                 view in source (navigate main view to anchor)
    d                     delete focused stale comment
    e                     edit & re-anchor (switch to main, pick new line)
    q   Esc               back to main view

Re-anchor mode (after pressing e in stale view)
    c   Enter             open composer at current line (pre-filled body)
    Esc                   cancel re-anchor mode

Send to Claude  (press C from main view)
    v                     view full rendered prompt (what Claude will see)
    Enter                 send — suspends TUI, runs Claude, redraws on return
    Esc                   cancel

In comment composer
    M-l M-c M-k M-d       scope:    line / change / stack / description
                          (M-d only when opened on a description line)
    M-r M-s M-n           severity: required / suggestion / note
    ^X                    save
    ^D                    delete (edit mode only)
    Esc                   cancel
";
