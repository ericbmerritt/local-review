use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::comment::Severity;

use super::composer::{Composer, ComposerScope, STATUS_STACK_UNAVAILABLE};
use super::diff_view::DiffView;

/// Truncation indicators surface visibly so a reviewer can tell context was
/// trimmed. Distinct from a literal `…` that may appear in a commit message.
pub(crate) const ELLIPSIS_BEFORE: &str = "  ⋯ (more above)";
pub(crate) const ELLIPSIS_AFTER: &str = "  ⋯ (more below)";

/// Composer modal overlay width (columns), capped to terminal width.
pub(super) const COMPOSER_OVERLAY_WIDTH: u16 = 72;

/// Composer modal overlay height (rows), capped to terminal height.
pub(super) const COMPOSER_OVERLAY_HEIGHT: u16 = 22;

// Per-row heights for the composer modal interior, top to bottom:
//   CONTEXT  — scope-specific context block (line: diff + ▶; change/stack: 2 rows)
//   SCOPE    — scope picker row + 1 spacer
//   SEVERITY — severity picker row + 1 spacer
//   BODY     — multi-line editor (consumes remaining space, min 4 rows)
//   STATUS   — in-modal refusal hint (1 row); blank when nothing to show
//   FOOTER   — keybinding hints split across 2 lines
//
// Line scope uses 5 rows (room for up to 5 context lines); change/stack use 2
// rows (change_id + description, or revset). The layout is fixed at 5 rows to
// keep the modal height stable across scope switches; the change/stack blocks
// simply leave rows empty.
const CONTEXT_ROWS: u16 = 5;
const SCOPE_ROWS: u16 = 2;
const SEVERITY_ROWS: u16 = 2;
const BODY_MIN_ROWS: u16 = 4;
const STATUS_ROWS: u16 = 1;
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

    // Status row is conditional so it doesn't permanently tax body space for a
    // transient feature. When `refusal_status` is None, the status slot is
    // omitted and BODY_MIN_ROWS reclaims its row.
    let has_status = composer.refusal_status.is_some();
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(CONTEXT_ROWS),
        Constraint::Length(SCOPE_ROWS),
        Constraint::Length(SEVERITY_ROWS),
        Constraint::Min(BODY_MIN_ROWS),
    ];
    if has_status {
        constraints.push(Constraint::Length(STATUS_ROWS));
    }
    constraints.push(Constraint::Length(FOOTER_ROWS));
    let chunks = Layout::vertical(constraints).split(inner);

    render_composer_context(frame, chunks[0], composer, current_view);
    render_scope_picker(frame, chunks[1], composer);
    render_severity_picker(frame, chunks[2], composer);
    render_body_editor(frame, chunks[3], composer);
    if has_status {
        render_composer_status(frame, chunks[4], composer);
        render_composer_footer(frame, chunks[5], composer.editing.is_some());
    } else {
        render_composer_footer(frame, chunks[4], composer.editing.is_some());
    }
}

fn render_composer_status(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let Some(msg) = composer.refusal_status else {
        return;
    };
    let style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let widget = Paragraph::new(TuiLine::from(Span::styled(format!("  {msg}"), style)));
    frame.render_widget(widget, area);
}

/// Test-only mirror of the layout-shape decision in `render_composer_overlay`:
/// returns the number of rows occupied by the status strip (0 or
/// `STATUS_ROWS`). Used to pin the "no permanent tax when `refusal_status` is
/// None" contract without spinning up a Frame.
#[cfg(test)]
pub(super) fn status_row_height(composer: &Composer) -> u16 {
    if composer.refusal_status.is_some() {
        STATUS_ROWS
    } else {
        0
    }
}

#[cfg(test)]
pub(super) const STATUS_ROWS_FOR_TEST: u16 = STATUS_ROWS;

fn render_composer_context(
    frame: &mut Frame<'_>,
    area: Rect,
    composer: &Composer,
    view: Option<&DiffView>,
) {
    match composer.scope {
        ComposerScope::Line => render_line_context(frame, area, composer, view),
        ComposerScope::Change => render_change_context(frame, area, composer),
        ComposerScope::Stack => render_stack_context(frame, area, composer),
        ComposerScope::Description => render_description_context(frame, area, composer),
    }
}

fn render_line_context(
    frame: &mut Frame<'_>,
    area: Rect,
    composer: &Composer,
    view: Option<&DiffView>,
) {
    let line_target = composer.line_target();
    let idx = line_target.rendered_index;

    let context_lines: Vec<TuiLine<'_>> = if let Some(view) = view {
        let start = idx.saturating_sub(2);
        let end = (idx + 2).min(view.lines.len().saturating_sub(1));
        (start..=end)
            .filter_map(|i| view.lines.get(i))
            .map(|l| {
                let marker = if l.source_line == line_target.source_line
                    && l.target_line == line_target.target_line
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

/// Cap descriptions in chrome so a long line cannot wrap into the scope
/// picker row below. Matches the stack overview's ~50-char convention; also
/// honors the available width so very narrow terminals still don't wrap.
const CHROME_DESC_MAX: usize = 50;

fn truncate_for_chrome(s: &str, area_width: u16) -> String {
    let budget = usize::from(area_width).saturating_sub(4);
    let cap = budget.min(CHROME_DESC_MAX);
    if s.chars().count() <= cap {
        return s.to_owned();
    }
    let head = cap.saturating_sub(2);
    let mut out: String = s.chars().take(head).collect();
    out.push_str("..");
    out
}

fn render_change_context(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let ctx = &composer.contexts.change;
    let desc = truncate_for_chrome(&ctx.description, area.width);
    let id_line = TuiLine::from(format!("  change  {}", ctx.change_id));
    let desc_line = TuiLine::from(format!("  {desc}"));
    let widget = Paragraph::new(Text::from(vec![id_line, desc_line]));
    frame.render_widget(widget, area);
}

fn render_stack_context(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let text = match &composer.contexts.stack {
        Some(ctx) => format!("  revset  {}", truncate_for_chrome(&ctx.revset, area.width)),
        None => format!("  {STATUS_STACK_UNAVAILABLE}"),
    };
    let widget = Paragraph::new(text.as_str());
    frame.render_widget(widget, area);
}

/// Render-time cap on context-window rows surrounding the cursor. The on-disk
/// anchor stores the full window (`CONTEXT_MAX` each side); render is the cap
/// point so re-anchoring fidelity is unaffected.
const DESC_CONTEXT_RENDER_CAP: usize = 2;

fn description_context_lines(
    target_text: &str,
    context_before: &[String],
    context_after: &[String],
) -> Vec<String> {
    let mut items = Vec::with_capacity(2 * DESC_CONTEXT_RENDER_CAP + 1);
    let before_truncated = context_before.len() > DESC_CONTEXT_RENDER_CAP;
    let before_skip = context_before.len().saturating_sub(DESC_CONTEXT_RENDER_CAP);
    let mut before_kept: Vec<&String> = context_before.iter().skip(before_skip).collect();
    if before_truncated && !before_kept.is_empty() {
        // Replace the topmost kept line (furthest from the target) with the
        // ellipsis indicator. Keeps total <= cap.
        items.push(ELLIPSIS_BEFORE.to_owned());
        before_kept.remove(0);
    }
    for line in before_kept {
        items.push(format!("  {line}"));
    }
    items.push(format!("▶ {target_text}"));
    let after_truncated = context_after.len() > DESC_CONTEXT_RENDER_CAP;
    let after_take = if after_truncated {
        DESC_CONTEXT_RENDER_CAP - 1
    } else {
        DESC_CONTEXT_RENDER_CAP
    };
    for line in context_after.iter().take(after_take) {
        items.push(format!("  {line}"));
    }
    if after_truncated {
        items.push(ELLIPSIS_AFTER.to_owned());
    }
    items
}

fn render_description_context(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let lines: Vec<TuiLine<'_>> = if let Some(ctx) = &composer.contexts.description {
        description_context_lines(&ctx.target_text, &ctx.context_before, &ctx.context_after)
            .into_iter()
            .map(TuiLine::from)
            .collect()
    } else {
        vec![TuiLine::from("  <description>")]
    };
    let widget = Paragraph::new(lines);
    frame.render_widget(widget, area);
}

/// Short `change_id` used in the scope picker row, where horizontal space is
/// tight. 8 chars is enough to disambiguate within a typical stack while
/// staying inside the 72-col modal even with the radio chrome.
const SCOPE_PICKER_CHANGE_ID_LEN: usize = 8;

fn short_change_id(s: &str) -> String {
    s.chars().take(SCOPE_PICKER_CHANGE_ID_LEN).collect()
}

/// Each tuple is `(mark, label, is_active)`. Mark is `"[x]"`/`"[ ]"`; label
/// is the scope name (Change appends ` · {short_id}`).
fn picker_segments(scope: ComposerScope, change_id: &str) -> [(&'static str, String, bool); 4] {
    let mark = |s: ComposerScope| if scope == s { "[x]" } else { "[ ]" };
    let change_short = short_change_id(change_id);
    [
        (
            mark(ComposerScope::Line),
            "line".to_owned(),
            scope == ComposerScope::Line,
        ),
        (
            mark(ComposerScope::Change),
            format!("change · {change_short}"),
            scope == ComposerScope::Change,
        ),
        (
            mark(ComposerScope::Stack),
            "stack".to_owned(),
            scope == ComposerScope::Stack,
        ),
        (
            mark(ComposerScope::Description),
            "description".to_owned(),
            scope == ComposerScope::Description,
        ),
    ]
}

#[cfg(test)]
fn scope_picker_text(scope: ComposerScope, change_id: &str) -> String {
    let segs = picker_segments(scope, change_id);
    let mut out = String::from("  scope    ");
    for (i, (mark, label, _)) in segs.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(mark);
        out.push(' ');
        out.push_str(label);
    }
    out
}

fn scope_picker_spans(scope: ComposerScope, change_id: &str) -> Vec<Span<'static>> {
    let segs = picker_segments(scope, change_id);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segs.len() * 2 + 1);
    spans.push(Span::raw("  scope    ".to_owned()));
    for (i, (mark, label, is_active)) in segs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  ".to_owned()));
        }
        let text = format!("{mark} {label}");
        if is_active {
            // REVERSED keeps the active label distinguishable on monochrome
            // terminals; severity uses BOLD+color.
            spans.push(Span::styled(
                text,
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        } else {
            spans.push(Span::raw(text));
        }
    }
    spans
}

fn render_scope_picker(frame: &mut Frame<'_>, area: Rect, composer: &Composer) {
    let spans = scope_picker_spans(composer.scope, composer.contexts.change.change_id.as_str());
    let widget = Paragraph::new(TuiLine::from(spans));
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
    let picked_style = Style::default()
        .fg(super::severity_color(composer.severity))
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

// Plain-text footer rendition exposed for width pinning. The renderer reads
// from this to keep tests and rendering aligned.
fn footer_lines(editing: bool) -> [&'static str; 2] {
    let line1 = "  ^L line  ^C change  ^K stack  M-d description";
    let line2 = if editing {
        "  M-r required  M-s suggestion  M-n note  ^D delete  ^X save"
    } else {
        "  M-r required  M-s suggestion  M-n note  ^X save"
    };
    [line1, line2]
}

fn render_composer_footer(frame: &mut Frame<'_>, area: Rect, editing: bool) {
    let [line1, line2] = footer_lines(editing);
    let widget = Paragraph::new(Text::from(vec![TuiLine::from(line1), TuiLine::from(line2)]));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Modal interior in columns: outer width minus the 2 border cells.
    fn interior_width() -> usize {
        usize::from(COMPOSER_OVERLAY_WIDTH - 2)
    }

    // -- B4: scope picker row must fit inside the modal interior in every
    //   scope selection. The 8-char `change_id` placeholder is the worst-case
    //   column (typical real change_ids are 8 chars; longer get truncated by
    //   `short_change_id`).
    #[test]
    fn scope_picker_text_fits_in_modal_interior_for_every_scope() {
        let scopes = [
            ComposerScope::Line,
            ComposerScope::Change,
            ComposerScope::Stack,
            ComposerScope::Description,
        ];
        let interior = interior_width();
        for scope in scopes {
            let text = scope_picker_text(scope, "abcdefgh");
            let cols = text.chars().count();
            assert!(
                cols <= interior,
                "scope picker row for {scope:?} is {cols} cols, must be <= {interior}: {text:?}"
            );
        }
    }

    // -- B6: description-context render output ≤ CONTEXT_ROWS (5 lines) for
    //   any input — even when the on-disk anchor carries the worst-case
    //   3+1+3 window.
    #[test]
    fn description_context_lines_caps_at_five_rows_for_worst_case_input() {
        let before = vec![
            "before-3".to_owned(),
            "before-2".to_owned(),
            "before-1".to_owned(),
        ];
        let after = vec![
            "after-1".to_owned(),
            "after-2".to_owned(),
            "after-3".to_owned(),
        ];
        let lines = description_context_lines("target", &before, &after);
        let cap = usize::from(CONTEXT_ROWS);
        assert!(
            lines.len() <= cap,
            "rendered context must be <= {} rows; got {} for: {lines:?}",
            cap,
            lines.len()
        );
    }

    // -- U1 / E3: when both sides exceed cap, both ellipsis indicators appear
    //   and total rows still fits CONTEXT_ROWS=5. Indicators are
    //   `ELLIPSIS_BEFORE` / `ELLIPSIS_AFTER` strings (distinct from a bare `…`
    //   that may appear in commit messages).
    #[test]
    fn description_context_lines_emits_ellipsis_indicators_when_both_sides_truncated() {
        let before = vec![
            "before-3".to_owned(),
            "before-2".to_owned(),
            "before-1".to_owned(),
        ];
        let after = vec![
            "after-1".to_owned(),
            "after-2".to_owned(),
            "after-3".to_owned(),
        ];
        let lines = description_context_lines("target", &before, &after);
        let cap = usize::from(CONTEXT_ROWS);
        assert!(lines.len() <= cap, "must fit cap {cap}; got {lines:?}");
        assert_eq!(
            lines[0], ELLIPSIS_BEFORE,
            "ELLIPSIS_BEFORE at top when before-side exceeds cap"
        );
        assert_eq!(
            lines[lines.len() - 1],
            ELLIPSIS_AFTER,
            "ELLIPSIS_AFTER at bottom when after-side exceeds cap"
        );
        assert!(lines.iter().any(|l| l == "▶ target"));
    }

    // -- U1: when neither side exceeds cap, no ellipsis indicators appear.
    #[test]
    fn description_context_lines_no_ellipsis_when_sides_within_cap() {
        let before = vec!["b1".to_owned(), "b2".to_owned()];
        let after = vec!["a1".to_owned(), "a2".to_owned()];
        let lines = description_context_lines("target", &before, &after);
        assert!(
            !lines
                .iter()
                .any(|l| l == ELLIPSIS_BEFORE || l == ELLIPSIS_AFTER),
            "no ellipsis expected when within cap; got {lines:?}"
        );
        // 2 before + target + 2 after.
        assert_eq!(lines.len(), 5);
    }

    // -- E3: ellipsis indicators fit inside the modal interior.
    #[test]
    fn ellipsis_indicators_fit_in_modal_interior() {
        let interior = interior_width();
        for marker in [ELLIPSIS_BEFORE, ELLIPSIS_AFTER] {
            let cols = marker.chars().count();
            assert!(
                cols <= interior,
                "ellipsis marker {marker:?} is {cols} cols, must be <= {interior}"
            );
        }
    }

    #[test]
    fn description_context_lines_handles_short_windows() {
        let lines = description_context_lines("only", &[], &[]);
        assert_eq!(lines, vec!["▶ only".to_owned()]);
    }

    // -- E4: scope picker spans REVERSE the active scope's label so it stays
    //   distinguishable on monochrome terminals; severity keeps BOLD+color.
    #[test]
    fn scope_picker_spans_reverses_active_scope_label() {
        let spans = scope_picker_spans(ComposerScope::Description, "abcdefgh");
        let active = spans
            .iter()
            .find(|s| s.content.contains("description"))
            .expect("description span must exist");
        assert!(
            active.style.add_modifier.contains(Modifier::REVERSED),
            "active scope label must be REVERSED; got style {:?}",
            active.style
        );
        let inactive = spans
            .iter()
            .find(|s| s.content.contains(" line"))
            .expect("line span must exist");
        assert!(
            !inactive.style.add_modifier.contains(Modifier::REVERSED),
            "inactive scope label must not be REVERSED; got style {:?}",
            inactive.style
        );
    }

    // -- T-D7: footer text fits inside the modal interior in both edit and
    //   non-edit modes.
    #[test]
    fn composer_footer_text_fits_in_modal_interior_for_both_modes() {
        let interior = interior_width();
        for editing in [false, true] {
            for line in footer_lines(editing) {
                let cols = line.chars().count();
                assert!(
                    cols <= interior,
                    "footer line (editing={editing}) is {cols} cols, must be <= {interior}: {line:?}"
                );
            }
        }
    }
}
