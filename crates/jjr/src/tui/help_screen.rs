//! jjr keybinding reference, supplied to the shared help screen renderer.

#[cfg(test)]
pub(super) use local_review_core::tui::help_screen::render;

pub(super) const JJR_HELP_BODY: &str = "
Movement
    ↑ ↓     k j           line
    PgUp PgDn             page
    Home End  g G         top / bottom
    Tab     S-Tab         next / previous file
    n       p             next / previous change (stack mode)

Comments
    Enter   c             new comment on current line
                          (on a description line: opens description-scoped comment)
    e                     edit (cursor must be on a comment line)
    d                     delete (cursor must be on a comment line)
    1                     filter to required only (press again to clear)
    2                     filter to suggestion only (press again to clear)
    3                     filter to note only (press again to clear)
    ;                     hide / show behavior-preserving rows
                          (cosmetic + renamed / moved / extracted tags)

Views
    (header)              entity list opens with the change subject, a
                          description peek, and a Σ scope line
                          (entities · files · LOC · sig changes)
    m                     open change description
    f                     file picker — jump to a file or the description
    r                     refresh diff and comments for current change
    s                     stack overview (stack mode only)
    S                     stale comments view
    |                     cycle diff layout: auto / unified / side-by-side
                          (auto picks side-by-side at >=120 cols)
    ?                     this help
    q                     quit

Actions
    C                     send current change to Claude (opens preview)
    y                     yank ±10 lines around cursor (with file:line header)
                          to system clipboard — paste into Claude as context

Review tracking
    U                     toggle reviewed status on current file
                          (description or diff file under cursor)
    ✓                     reviewed indicator: file picker, stack overview
                          right edge, main-view title. Auto-marked on land;
                          U is the escape hatch.

── Sub-screens (context-sensitive) ──────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::JJR_HELP_BODY;

    #[test]
    fn help_body_contains_review_tracking_section_header() {
        assert!(
            JJR_HELP_BODY.contains("Review tracking"),
            "help body must contain the `Review tracking` section header"
        );
    }

    #[test]
    fn help_body_documents_u_keybind_for_toggle() {
        assert!(
            JJR_HELP_BODY.contains('U'),
            "help must mention the U keybind"
        );
        assert!(
            JJR_HELP_BODY.contains("toggle reviewed status"),
            "help must explain what U does"
        );
    }

    #[test]
    fn help_body_documents_check_glyph_legend() {
        assert!(
            JJR_HELP_BODY.contains("\u{2713}"),
            "help must show the ✓ glyph"
        );
        assert!(
            JJR_HELP_BODY.contains("file picker"),
            "help must list `file picker` as a ✓ surface"
        );
        assert!(
            JJR_HELP_BODY.contains("stack overview"),
            "help must list `stack overview` as a ✓ surface"
        );
        assert!(
            JJR_HELP_BODY.contains("main-view title"),
            "help must list `main-view title` as a ✓ surface"
        );
    }
}
