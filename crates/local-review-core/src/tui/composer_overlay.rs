//! Composer modal overlay rendering.
//!
//! Pure ratatui rendering; no IO, no subprocess. All state is passed by
//! reference; the caller owns the `Frame` and `Composer`.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::severity::Severity;

use super::composer::{
    Composer, ComposerScope, DescriptionContext, LineTarget, ScopeTag, StackContextSnapshot,
};
use super::diff_view::DiffView;
use super::severity_color;
use super::textarea::TextArea;

/// Two-space indent applied to status lines in the overlay.
const INDENT: &str = "  ";

/// Truncation indicators surface visibly so a reviewer can tell context was
/// trimmed.
pub const ELLIPSIS_BEFORE: &str = "  ⋯ (more above)";
pub const ELLIPSIS_AFTER: &str = "  ⋯ (more below)";

/// Composer modal overlay width (columns), capped to terminal width.
pub const COMPOSER_OVERLAY_WIDTH: u16 = 72;

/// Composer modal overlay height (rows), capped to terminal height.
pub const COMPOSER_OVERLAY_HEIGHT: u16 = 22;

const CONTEXT_ROWS: u16 = 5;
const SCOPE_ROWS: u16 = 2;
const SEVERITY_ROWS: u16 = 2;
const BODY_MIN_ROWS: u16 = 4;
const STATUS_ROWS: u16 = 1;
const FOOTER_ROWS: u16 = 2;

/// Borrowed view of only the fields that the composer overlay renderer needs.
///
/// Both `local_review_core::tui::composer::Composer` and
/// `jjr::tui::composer::Composer` carry an `EditingContext` with different
/// fields; the renderer only needs to know whether edit mode is active.
/// Constructing this view flattens that difference so all render functions
/// share a single implementation.
pub struct ComposerRenderView<'a> {
    pub title: String,
    pub scope: &'a ComposerScope,
    pub severity: Severity,
    pub body: &'a TextArea,
    pub refusal_status: Option<&'static str>,
    pub change_id: &'a str,
    pub change_description: &'a str,
    pub editing_is_some: bool,
    pub focus: crate::tui::composer::ComposerFocus,
}

impl<'a> ComposerRenderView<'a> {
    pub fn from_composer(composer: &'a Composer) -> Self {
        Self {
            title: composer.title(),
            scope: &composer.scope,
            severity: composer.severity,
            body: &composer.body,
            refusal_status: composer.refusal_status,
            change_id: composer.change_id.as_str(),
            change_description: &composer.change_description,
            editing_is_some: composer.editing.is_some(),
            focus: composer.focus,
        }
    }
}

pub fn render_composer_overlay(
    frame: &mut Frame<'_>,
    composer: &Composer,
    current_view: Option<&DiffView>,
) {
    let view = ComposerRenderView::from_composer(composer);
    render_composer_overlay_view(frame, &view, current_view);
}

/// Render the composer overlay from a [`ComposerRenderView`].
///
/// Callers that build a `ComposerRenderView` from any composer type — core or
/// tool-specific — use this entry point rather than [`render_composer_overlay`],
/// which only accepts the core `Composer` directly.
pub fn render_composer_overlay_view(
    frame: &mut Frame<'_>,
    view: &ComposerRenderView<'_>,
    current_view: Option<&DiffView>,
) {
    let area = frame.area();
    let overlay = centered_rect(area, COMPOSER_OVERLAY_WIDTH, COMPOSER_OVERLAY_HEIGHT);

    frame.render_widget(Clear, overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Double)
        .title(view.title.as_str());

    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let has_status = view.refusal_status.is_some();
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

    render_composer_context(frame, chunks[0], view, current_view);
    render_scope_picker(frame, chunks[1], view);
    render_severity_picker(frame, chunks[2], view);
    render_body_editor(frame, chunks[3], view);
    if has_status {
        render_composer_status(frame, chunks[4], view);
        render_composer_footer(frame, chunks[5], view.editing_is_some);
    } else {
        render_composer_footer(frame, chunks[4], view.editing_is_some);
    }
}

fn render_composer_status(frame: &mut Frame<'_>, area: Rect, view: &ComposerRenderView<'_>) {
    let Some(msg) = view.refusal_status else {
        return;
    };
    let style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let widget = Paragraph::new(TuiLine::from(Span::styled(format!("{INDENT}{msg}"), style)));
    frame.render_widget(widget, area);
}

/// Test-only mirror of the layout-shape decision: returns the number of rows
/// occupied by the status strip (0 or `STATUS_ROWS`).
#[cfg(test)]
pub fn status_row_height(composer: &Composer) -> u16 {
    if composer.refusal_status.is_some() {
        STATUS_ROWS
    } else {
        0
    }
}

#[cfg(test)]
pub const STATUS_ROWS_FOR_TEST: u16 = STATUS_ROWS;

fn render_composer_context(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ComposerRenderView<'_>,
    diff_view: Option<&DiffView>,
) {
    match view.scope {
        ComposerScope::Line(line) => render_line_context(frame, area, line, diff_view),
        ComposerScope::Change => render_change_context(frame, area, view),
        ComposerScope::Stack(stack) => render_stack_context(frame, area, stack),
        ComposerScope::Description(ctx) => render_description_context(frame, area, ctx),
    }
}

fn render_line_context(
    frame: &mut Frame<'_>,
    area: Rect,
    line_target: &LineTarget,
    view: Option<&DiffView>,
) {
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
/// picker row below.
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

fn render_change_context(frame: &mut Frame<'_>, area: Rect, view: &ComposerRenderView<'_>) {
    let desc = truncate_for_chrome(view.change_description, area.width);
    let id_line = TuiLine::from(format!("  change  {}", view.change_id));
    let desc_line = TuiLine::from(format!("  {desc}"));
    let widget = Paragraph::new(Text::from(vec![id_line, desc_line]));
    frame.render_widget(widget, area);
}

fn render_stack_context(frame: &mut Frame<'_>, area: Rect, stack: &StackContextSnapshot) {
    let text = format!(
        "  revset  {}",
        truncate_for_chrome(&stack.revset, area.width)
    );
    let widget = Paragraph::new(text.as_str());
    frame.render_widget(widget, area);
}

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

fn render_description_context(frame: &mut Frame<'_>, area: Rect, ctx: &DescriptionContext) {
    let lines: Vec<TuiLine<'_>> =
        description_context_lines(&ctx.target_text, &ctx.context_before, &ctx.context_after)
            .into_iter()
            .map(TuiLine::from)
            .collect();
    let widget = Paragraph::new(lines);
    frame.render_widget(widget, area);
}

/// Number of change ID characters shown in the scope picker dropdown row.
/// Eight characters is enough to disambiguate jj short IDs in typical stacks.
const SCOPE_PICKER_CHANGE_ID_LEN: usize = 8;

fn short_change_id(s: &str) -> String {
    s.chars().take(SCOPE_PICKER_CHANGE_ID_LEN).collect()
}

fn picker_segments(active: ScopeTag, change_id: &str) -> [(&'static str, String, bool); 4] {
    let mark = |tag: ScopeTag| if active == tag { "[x]" } else { "[ ]" };
    let change_short = short_change_id(change_id);
    [
        (
            mark(ScopeTag::Line),
            "line".to_owned(),
            active == ScopeTag::Line,
        ),
        (
            mark(ScopeTag::Change),
            format!("change · {change_short}"),
            active == ScopeTag::Change,
        ),
        (
            mark(ScopeTag::Stack),
            "stack".to_owned(),
            active == ScopeTag::Stack,
        ),
        (
            mark(ScopeTag::Description),
            "description".to_owned(),
            active == ScopeTag::Description,
        ),
    ]
}

#[cfg(test)]
fn scope_picker_text(active: ScopeTag, change_id: &str) -> String {
    let segs = picker_segments(active, change_id);
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

fn scope_picker_spans(active: ScopeTag, change_id: &str, focused: bool) -> Vec<Span<'static>> {
    let segs = picker_segments(active, change_id);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segs.len() * 2 + 1);
    // `▸ ` marker when focused, two spaces otherwise — preserves alignment.
    let label = if focused {
        "▸ scope    ".to_owned()
    } else {
        "  scope    ".to_owned()
    };
    let label_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    spans.push(Span::styled(label, label_style));
    for (i, (mark, label, is_active)) in segs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  ".to_owned()));
        }
        let text = format!("{mark} {label}");
        if is_active {
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

fn render_scope_picker(frame: &mut Frame<'_>, area: Rect, view: &ComposerRenderView<'_>) {
    let focused = view.focus == crate::tui::composer::ComposerFocus::Scope;
    let spans = scope_picker_spans(ScopeTag::of(view.scope), view.change_id, focused);
    let widget = Paragraph::new(TuiLine::from(spans));
    frame.render_widget(widget, area);
}

fn render_severity_picker(frame: &mut Frame<'_>, area: Rect, view: &ComposerRenderView<'_>) {
    let note_mark = if view.severity == Severity::Note {
        "[x]"
    } else {
        "[ ]"
    };
    let sug_mark = if view.severity == Severity::Suggestion {
        "[x]"
    } else {
        "[ ]"
    };
    let req_mark = if view.severity == Severity::Required {
        "[x]"
    } else {
        "[ ]"
    };
    let picked_style = Style::default()
        .fg(severity_color(view.severity))
        .add_modifier(Modifier::BOLD);

    let focused = view.focus == crate::tui::composer::ComposerFocus::Severity;
    let label = if focused {
        "▸ severity  "
    } else {
        "  severity  "
    };
    let label_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let mut spans: Vec<Span<'_>> = Vec::with_capacity(8);
    spans.push(Span::styled(label, label_style));
    if view.severity == Severity::Note {
        spans.push(Span::styled(format!("{note_mark} note"), picked_style));
    } else {
        spans.push(Span::raw(format!("{note_mark} note")));
    }
    spans.push(Span::raw("    "));
    if view.severity == Severity::Suggestion {
        spans.push(Span::styled(format!("{sug_mark} suggestion"), picked_style));
    } else {
        spans.push(Span::raw(format!("{sug_mark} suggestion")));
    }
    spans.push(Span::raw("    "));
    if view.severity == Severity::Required {
        spans.push(Span::styled(format!("{req_mark} required"), picked_style));
    } else {
        spans.push(Span::raw(format!("{req_mark} required")));
    }

    let widget = Paragraph::new(TuiLine::from(spans));
    frame.render_widget(widget, area);
}

fn render_body_editor(frame: &mut Frame<'_>, area: Rect, view: &ComposerRenderView<'_>) {
    frame.render_widget(view.body, area);
}

fn footer_lines(editing: bool) -> [&'static str; 2] {
    // Primary line: the multiplexer-safe pattern that works in every
    // terminal. Secondary line: the Alt-chord direct selectors (faster
    // when not intercepted by a multiplexer like zellij/tmux).
    let line1 = if editing {
        "  Tab focus  Space cycle  ^X save  ^D delete  Esc cancel"
    } else {
        "  Tab focus  Space cycle  ^X save  Esc cancel"
    };
    let line2 = "  alt: M-r/M-s/M-n severity  M-l/M-c/M-k/M-d scope";
    [line1, line2]
}

fn render_composer_footer(frame: &mut Frame<'_>, area: Rect, editing: bool) {
    let [line1, line2] = footer_lines(editing);
    let widget = Paragraph::new(Text::from(vec![TuiLine::from(line1), TuiLine::from(line2)]));
    frame.render_widget(widget, area);
}

pub fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
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
    use crate::change_id::ChangeId;
    use crate::revset_hash::RevsetHash;
    use crate::tui::composer::{ComposerInit, ComposerScope, LineTarget, StackContextSnapshot};
    use std::path::PathBuf;

    fn make_composer_for_overlay() -> Composer {
        let target = LineTarget {
            file: PathBuf::from("src/lib.rs"),
            rendered_index: 0,
            source_line: None,
            target_line: Some(1),
            target_text: "fn foo() {}".to_owned(),
            hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
            context_before: vec![],
            context_after: vec![],
        };
        Composer::new(ComposerInit {
            scope: ComposerScope::Line(target.clone()),
            severity: Severity::Note,
            change_id: ChangeId::parse("abc12345").unwrap(),
            change_description: "test change".to_owned(),
            line_available: Some(target),
            stack_available: Some(StackContextSnapshot {
                revset: "trunk()..@".to_owned(),
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            }),
            description_available: None,
        })
    }

    #[test]
    fn status_row_height_is_zero_when_no_refusal() {
        let composer = make_composer_for_overlay();
        assert_eq!(
            status_row_height(&composer),
            0,
            "no refusal_status means height 0"
        );
    }

    #[test]
    fn status_row_height_is_status_rows_when_refusal_set() {
        let mut composer = make_composer_for_overlay();
        composer.refusal_status =
            Some("line scope unavailable: cursor is not on a commentable line");
        assert_eq!(
            status_row_height(&composer),
            STATUS_ROWS_FOR_TEST,
            "refusal_status present means height equals STATUS_ROWS"
        );
    }

    fn interior_width() -> usize {
        usize::from(COMPOSER_OVERLAY_WIDTH - 2)
    }

    #[test]
    fn scope_picker_text_fits_in_modal_interior_for_every_scope() {
        let tags = [
            ScopeTag::Line,
            ScopeTag::Change,
            ScopeTag::Stack,
            ScopeTag::Description,
        ];
        let interior = interior_width();
        for tag in tags {
            let text = scope_picker_text(tag, "abcdefgh");
            let cols = text.chars().count();
            assert!(
                cols <= interior,
                "scope picker row for {tag:?} is {cols} cols, must be <= {interior}: {text:?}"
            );
        }
    }

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
        assert_eq!(lines.len(), 5);
    }

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

    #[test]
    fn scope_picker_spans_reverses_active_scope_label() {
        let spans = scope_picker_spans(ScopeTag::Description, "abcdefgh", false);
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

    #[test]
    fn status_row_height_round_trip_refused_chord_then_key_clears() {
        use crate::tui::composer::handle_composer_key;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut composer = make_composer_for_overlay();

        assert_eq!(
            status_row_height(&composer),
            0,
            "height must be 0 before any refusal"
        );

        // Set refusal_status directly rather than via a key event to keep
        // focus on status_row_height; the chord paths are tested elsewhere.
        composer.refusal_status =
            Some("line scope unavailable: cursor is not on a commentable line");

        assert_eq!(
            status_row_height(&composer),
            STATUS_ROWS_FOR_TEST,
            "height must equal STATUS_ROWS_FOR_TEST when refusal_status is set"
        );

        // Any subsequent key clears refusal_status at the top of handle_composer_key.
        handle_composer_key(
            &mut composer,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );

        assert_eq!(
            status_row_height(&composer),
            0,
            "height must return to 0 after the next keypress clears refusal_status"
        );
    }
}
