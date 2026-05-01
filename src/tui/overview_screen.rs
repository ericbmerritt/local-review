use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::comment::Comment;
use crate::stack::StackEntry;
use crate::util::truncate;

use super::{
    render_dots_mixed, render_view_scrollbar, scrollbar_layout_for_view, severity_color,
    severity_label, App, SeverityHistogram,
};

/// Box-drawing Unicode block: U+2500..=U+257F.
const BOX_DRAWING_START: char = '\u{2500}';
const BOX_DRAWING_END: char = '\u{257F}';

/// Footer text for the stack overview screen.
pub(super) const OVERVIEW_FOOTER_TEXT: &str =
    " \u{2191}\u{2193} select  Enter open  c new comment  q back  ?";

/// Render the right-edge dot + count column string for a given histogram.
///
/// Shape: `●●  2` (mixed-severity dots then count). Stale comments are
/// expected to be excluded by the caller (via `SeverityHistogram::from_comments`,
/// which filters them).
#[must_use]
pub(super) fn dot_string(hist: SeverityHistogram) -> String {
    if hist.total() == 0 {
        return String::new();
    }
    let dots = render_dots_mixed(hist);
    format!("{}  {}", dots, hist.total())
}

/// Strip box-drawing characters (U+2500..=U+257F), ANSI escape sequences
/// (CSI/OSC/DCS/PM/APC), and `BiDi` override marks from `s`.
///
/// CSI (`\x1b[ ... <letter>`) is terminated by the first ASCII letter.
/// OSC/DCS/PM/APC are terminated by either BEL (`\x07`) or ST (`\x1b\\`).
#[must_use]
pub(super) fn strip_box_drawing_and_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => drain_escape(&mut chars),
            BOX_DRAWING_START..=BOX_DRAWING_END
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => {}
            c if c.is_control() && c != '\t' => {}
            _ => result.push(c),
        }
    }
    result
}

/// Drain an ANSI escape sequence after `\x1b` has been consumed. Recognizes
/// CSI (`[`), OSC (`]`), DCS (`P`), PM (`^`), APC (`_`); other introducers
/// drop the next character (the typical 2-char `ESC <byte>` sequence).
fn drain_escape<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    let Some(&intro) = chars.peek() else {
        return;
    };
    match intro {
        '[' => {
            chars.next();
            for inner in chars.by_ref() {
                if inner.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        ']' | 'P' | '^' | '_' => {
            chars.next();
            // Drain until BEL or ST (`\x1b\\`). ST needs a two-byte lookahead.
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    return;
                }
                if c == '\x1b' {
                    if let Some(&'\\') = chars.peek() {
                        chars.next();
                    }
                    return;
                }
            }
        }
        _ => {
            chars.next();
        }
    }
}

/// Column budget computed from the terminal width.
#[derive(Debug, Clone, Copy)]
pub(super) struct ColumnBudget {
    /// Inner area width (terminal width minus 2 border chars).
    pub(super) inner_width: usize,
    /// Whether the index column (`idx`) is rendered (drops below 100 cols).
    pub(super) show_idx: bool,
    /// Whether inset preview body text is shown (drops below 80 cols).
    pub(super) show_inset_body: bool,
}

/// Fixed column widths (in characters) that don't depend on terminal width.
/// sp(2) idx(2) sp(2) change-id(8) sp(2) description(filled) sp(2) dots(3) sp(2) count(2) sp(1)
const IDX_WIDTH: usize = 2;
const CHANGE_ID_WIDTH: usize = 8;
/// Derive the column budget for the given terminal area width.
///
/// Thresholds are on terminal (outer) width per the spec resize ladder:
/// - ≥120: full layout with idx column and inset body previews.
/// - 100–119: drop idx column; previews still shown.
/// - 80–99: drop idx column; previews still shown.
/// - <80: drop inset preview text entirely.
#[must_use]
pub(super) fn column_layout(area_width: u16) -> ColumnBudget {
    let inner_width = usize::from(area_width.saturating_sub(2));
    ColumnBudget {
        inner_width,
        show_idx: area_width >= 100,
        show_inset_body: area_width >= 80,
    }
}

/// A single navigable or display row in the overview list.
#[derive(Debug, Clone)]
pub(super) enum OverviewRow {
    /// The header / sentinel row for stack-level comments.
    StackHeader,
    /// A stack-level comment at the given index into the cache's `stack_level` vec.
    StackComment(usize),
    /// Visual separator between the stack-level section and the change table.
    Separator,
    /// A change row at the given index into the stack entries list.
    ChangeRow(usize),
    /// A change-level comment inset under a change row.
    ChangeComment {
        change_idx: usize,
        comment_idx: usize,
    },
    /// The `Stale comments    N   (press S)` footer line.
    SummaryFooterStale,
    /// The `Total comments    N` footer line.
    SummaryFooterTotal,
}

impl OverviewRow {
    /// Whether cursor navigation should stop on this row.
    pub(super) fn is_navigable(&self) -> bool {
        !matches!(
            self,
            Self::Separator | Self::SummaryFooterStale | Self::SummaryFooterTotal
        )
    }
}

/// Screen state for the stack overview.
pub(super) struct OverviewScreenState {
    /// Index of the selected row in the unified row list.
    pub(super) selected_row: usize,
    pub(super) scroll_offset: u16,
}

impl OverviewScreenState {
    pub(super) fn new() -> Self {
        Self {
            selected_row: 0,
            scroll_offset: 0,
        }
    }
}

/// All comments needed for the overview, loaded once and cached.
pub(super) struct OverviewCommentSet {
    pub(super) stack_level: Vec<Comment>,
    /// Per-change comments in stack order (parallel to `ResolvedStack.entries`).
    pub(super) per_change: Vec<Vec<Comment>>,
    /// Comments from change files whose `change_id` is no longer in the resolved
    /// revset. Loaded with `status = Orphaned`. Not rendered anywhere in the
    /// current UI; held for future `jjr orphans` surfacing.
    #[expect(dead_code, reason = "held for future jjr orphans surfacing")]
    pub(super) orphaned: Vec<Comment>,
}

impl OverviewCommentSet {
    pub(super) fn stale_count(&self) -> usize {
        self.stack_level
            .iter()
            .chain(self.per_change.iter().flatten())
            .filter(|c| c.status == Some(crate::comment::Status::Stale))
            .count()
    }

    pub(super) fn total_count(&self) -> usize {
        self.stack_level.len() + self.per_change.iter().map(Vec::len).sum::<usize>()
    }
}

/// Build the ordered row list from the cached comment set and the stack entries.
pub(super) fn build_rows(
    cache: &OverviewCommentSet,
    stack_entries: &[StackEntry],
    stale_count: usize,
    total_count: usize,
) -> Vec<OverviewRow> {
    let mut rows: Vec<OverviewRow> = Vec::new();

    rows.push(OverviewRow::StackHeader);
    for i in 0..cache.stack_level.len() {
        rows.push(OverviewRow::StackComment(i));
    }

    rows.push(OverviewRow::Separator);

    for (change_idx, _entry) in stack_entries.iter().enumerate() {
        rows.push(OverviewRow::ChangeRow(change_idx));
        let comments = cache
            .per_change
            .get(change_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for comment_idx in 0..comments.len() {
            rows.push(OverviewRow::ChangeComment {
                change_idx,
                comment_idx,
            });
        }
    }

    if stale_count > 0 || total_count > 0 {
        rows.push(OverviewRow::SummaryFooterStale);
        rows.push(OverviewRow::SummaryFooterTotal);
    }

    rows
}

fn clamp_selected(rows: &[OverviewRow], mut idx: usize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    idx = idx.min(rows.len() - 1);
    while idx < rows.len() && !rows[idx].is_navigable() {
        idx += 1;
    }
    if idx >= rows.len() {
        for i in (0..rows.len()).rev() {
            if rows[i].is_navigable() {
                return i;
            }
        }
        return 0;
    }
    idx
}

/// Move the cursor by `delta` (+1 or -1), skipping non-navigable rows.
pub(super) fn move_cursor(rows: &[OverviewRow], current: usize, delta: isize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let max = rows.len().saturating_sub(1);
    let mut next = if delta > 0 {
        current.saturating_add(1).min(max)
    } else {
        current.saturating_sub(1)
    };

    let step: isize = if delta >= 0 { 1 } else { -1 };
    loop {
        if rows[next].is_navigable() {
            return next;
        }
        let Ok(next_signed) = isize::try_from(next) else {
            break;
        };
        let candidate = next_signed + step;
        let Ok(candidate_usize) = usize::try_from(candidate) else {
            break;
        };
        if candidate_usize > max {
            break;
        }
        next = candidate_usize;
    }

    current
}

/// Recompute `scroll_offset` so `selected_row` stays visible.
pub(super) fn compute_scroll_offset(
    selected_row: usize,
    inner_rows: u16,
    current_offset: u16,
) -> u16 {
    let selected = u16::try_from(selected_row).unwrap_or(u16::MAX);
    if selected < current_offset {
        return selected;
    }
    let last_visible = current_offset.saturating_add(inner_rows).saturating_sub(1);
    if selected > last_visible {
        return selected.saturating_sub(inner_rows.saturating_sub(1));
    }
    current_offset
}

/// Selection cursor glyph (only one row at a time).
const SELECTED_CURSOR: &str = "\u{25b6} ";
/// Indicator for the change loaded in the main view (distinct from the
/// selection cursor so both can be visible without ambiguity).
const CURRENT_CHANGE_MARK: &str = "\u{25b8} ";
const NO_CURSOR: &str = "  ";

/// Render the stack-level comment header line (with aggregate dot column).
fn render_stack_header_line<'a>(
    stack_comments: &[Comment],
    budget: ColumnBudget,
    is_selected: bool,
) -> TuiLine<'a> {
    let cursor = if is_selected {
        SELECTED_CURSOR
    } else {
        NO_CURSOR
    };
    let label = "STACK-LEVEL COMMENTS";
    let hist = SeverityHistogram::from_comments(stack_comments);
    let ds = dot_string(hist);

    if ds.is_empty() {
        let text = format!("{cursor}{label}");
        let style = if is_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        return TuiLine::from(Span::styled(text, style));
    }

    let ds_len = ds.chars().count();
    let header_base = format!("{cursor}{label}");
    let header_len = header_base.chars().count();
    let pad = budget
        .inner_width
        .saturating_sub(header_len)
        .saturating_sub(ds_len);
    let padding = " ".repeat(pad);
    let full = format!("{header_base}{padding}{ds}");

    let style = if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    TuiLine::from(Span::styled(full, style))
}

/// Render a single stack-level comment preview row.
fn render_stack_comment_line<'a>(
    comment: &Comment,
    budget: ColumnBudget,
    is_selected: bool,
) -> TuiLine<'a> {
    let cursor = if is_selected {
        SELECTED_CURSOR
    } else {
        NO_CURSOR
    };
    let dot = "● ";
    let sev = severity_label(comment.severity);
    let color = severity_color(comment.severity);

    let prefix = format!("{cursor}{dot}{sev}   ");
    let prefix_len = prefix.chars().count();

    if !budget.show_inset_body {
        return TuiLine::from(vec![
            Span::raw(cursor.to_owned()),
            Span::styled(format!("{dot}{sev}"), Style::default().fg(color)),
        ]);
    }

    let body_first_line = comment.body.lines().next().unwrap_or("");
    let body_clean = strip_box_drawing_and_ansi(body_first_line);
    let available = budget.inner_width.saturating_sub(prefix_len);
    let body_preview = truncate(&body_clean, available);

    let style_base = if is_selected {
        Style::default().fg(color).add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(color)
    };

    TuiLine::from(vec![
        Span::raw(cursor.to_owned()),
        Span::styled(format!("{dot}{sev}   {body_preview}"), style_base),
    ])
}

#[derive(Clone, Copy)]
struct ChangeRowArgs<'e> {
    entry: &'e StackEntry,
    change_idx: usize,
    per_change_comments: &'e [Vec<Comment>],
    budget: ColumnBudget,
    is_current: bool,
    is_selected: bool,
}

/// Render a change row in the table.
fn render_change_row_line(args: ChangeRowArgs<'_>) -> TuiLine<'_> {
    let cursor = if args.is_selected {
        SELECTED_CURSOR
    } else if args.is_current {
        CURRENT_CHANGE_MARK
    } else {
        NO_CURSOR
    };

    let idx_str = if args.budget.show_idx {
        format!("{:width$}", args.change_idx + 1, width = IDX_WIDTH)
    } else {
        String::new()
    };

    let change_id_str: String = args
        .entry
        .change_id
        .as_str()
        .chars()
        .take(CHANGE_ID_WIDTH)
        .collect();
    let comments = args
        .per_change_comments
        .get(args.change_idx)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let hist = SeverityHistogram::from_comments(comments);
    let ds = dot_string(hist);

    let fixed_before_desc =
        2 + if args.budget.show_idx {
            IDX_WIDTH + 2
        } else {
            0
        } + CHANGE_ID_WIDTH
            + 2;
    let right_fixed = if ds.is_empty() {
        3
    } else {
        2 + ds.chars().count() + 1
    };
    let desc_budget = args
        .budget
        .inner_width
        .saturating_sub(fixed_before_desc)
        .saturating_sub(right_fixed);
    let desc = truncate(&args.entry.description, desc_budget);

    let right_col = if ds.is_empty() {
        "\u{2713}".to_owned()
    } else {
        ds.clone()
    };

    let left_part = if args.budget.show_idx {
        format!("{cursor}{idx_str}  {change_id_str}  {desc}")
    } else {
        format!("{cursor}{change_id_str}  {desc}")
    };
    let left_len = left_part.chars().count();
    let right_len = right_col.chars().count();
    // Pad with " ".repeat(N) plus the literal "  " separator. Subtract 2 to
    // account for the literal separator so total line length equals
    // `inner_width` exactly. Without the subtraction the line is 2 chars
    // longer than the budget and ratatui clips the trailing dots/count.
    let pad = args
        .budget
        .inner_width
        .saturating_sub(left_len)
        .saturating_sub(right_len)
        .saturating_sub(2);
    let padding = " ".repeat(pad);
    let text = format!("{left_part}{padding}  {right_col}");

    let style = if args.is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    TuiLine::from(Span::styled(text, style))
}

/// Render a change-level comment inset row (`◆ change · severity · body`).
fn render_change_inset_line<'a>(
    comment: &Comment,
    budget: ColumnBudget,
    is_selected: bool,
) -> TuiLine<'a> {
    let cursor = if is_selected {
        SELECTED_CURSOR
    } else {
        NO_CURSOR
    };
    let diamond = "\u{25c6}";
    let sev = severity_label(comment.severity);
    let color = severity_color(comment.severity);

    if !budget.show_inset_body {
        return TuiLine::from(vec![
            Span::raw(format!("{cursor}   ")),
            Span::styled(
                format!("{diamond} change \u{00b7} {sev}"),
                Style::default().fg(color),
            ),
        ]);
    }

    let prefix = format!("{cursor}   {diamond} change \u{00b7} {sev} \u{00b7} ");
    let prefix_len = prefix.chars().count();

    let body_first_line = comment.body.lines().next().unwrap_or("");
    let body_clean = strip_box_drawing_and_ansi(body_first_line);
    let available = budget.inner_width.saturating_sub(prefix_len);
    let body_preview = truncate(&body_clean, available);

    let style = if is_selected {
        Style::default().fg(color).add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(color)
    };

    TuiLine::from(vec![
        Span::raw(format!("{cursor}   ")),
        Span::styled(
            format!("{diamond} change \u{00b7} {sev} \u{00b7} {body_preview}"),
            style,
        ),
    ])
}

fn render_separator_line(budget: ColumnBudget) -> TuiLine<'static> {
    let sep_width = budget.inner_width.saturating_sub(4);
    let sep = format!("   {} ", "\u{2500}".repeat(sep_width));
    TuiLine::from(Span::styled(sep, Style::default().fg(Color::DarkGray)))
}

fn render_summary_footer_stale_line<'a>(stale_count: usize) -> TuiLine<'a> {
    TuiLine::from(Span::styled(
        format!("   Stale comments    {stale_count}   (press S)"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn render_summary_footer_total_line<'a>(total_count: usize) -> TuiLine<'a> {
    TuiLine::from(Span::styled(
        format!("   Total comments    {total_count}"),
        Style::default().fg(Color::DarkGray),
    ))
}

struct RowsCtx<'a> {
    selected_row: usize,
    cache: &'a OverviewCommentSet,
    entries: &'a [StackEntry],
    current_change_idx: usize,
    budget: ColumnBudget,
    stale_count: usize,
    total_count: usize,
}

fn rows_to_lines<'a>(rows: &'a [OverviewRow], ctx: &'a RowsCtx<'a>) -> Vec<TuiLine<'a>> {
    rows.iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let is_selected = row_idx == ctx.selected_row;
            match row {
                OverviewRow::StackHeader => {
                    render_stack_header_line(&ctx.cache.stack_level, ctx.budget, is_selected)
                }
                OverviewRow::StackComment(ci) => ctx
                    .cache
                    .stack_level
                    .get(*ci)
                    .map_or_else(TuiLine::default, |c| {
                        render_stack_comment_line(c, ctx.budget, is_selected)
                    }),
                OverviewRow::Separator => render_separator_line(ctx.budget),
                OverviewRow::ChangeRow(ci) => {
                    ctx.entries.get(*ci).map_or_else(TuiLine::default, |e| {
                        render_change_row_line(ChangeRowArgs {
                            entry: e,
                            change_idx: *ci,
                            per_change_comments: &ctx.cache.per_change,
                            budget: ctx.budget,
                            is_current: *ci == ctx.current_change_idx,
                            is_selected,
                        })
                    })
                }
                OverviewRow::ChangeComment {
                    change_idx,
                    comment_idx,
                } => ctx
                    .cache
                    .per_change
                    .get(*change_idx)
                    .and_then(|v| v.get(*comment_idx))
                    .map_or_else(TuiLine::default, |c| {
                        render_change_inset_line(c, ctx.budget, is_selected)
                    }),
                OverviewRow::SummaryFooterStale => {
                    render_summary_footer_stale_line(ctx.stale_count)
                }
                OverviewRow::SummaryFooterTotal => {
                    render_summary_footer_total_line(ctx.total_count)
                }
            }
        })
        .collect()
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    state: &mut OverviewScreenState,
    app: &App,
    cache: &OverviewCommentSet,
) {
    let area = frame.area();
    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let Some(ctx) = app.stack.as_ref() else {
        let msg = Paragraph::new("  Stack overview is not available in single-change mode.");
        frame.render_widget(msg, area);
        return;
    };

    let title = format!("Stack: {}", ctx.revset);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(layout[0]);
    frame.render_widget(block, layout[0]);

    let stale_count = cache.stale_count();
    let total_count = cache.total_count();

    let rows = build_rows(cache, &ctx.entries, stale_count, total_count);

    state.selected_row = clamp_selected(&rows, state.selected_row);
    state.scroll_offset =
        compute_scroll_offset(state.selected_row, inner.height, state.scroll_offset);

    // `column_layout` takes the OUTER terminal width and derives inner-width
    // and threshold decisions internally (see its doc). Pass `area.width`
    // directly so the column-budget decision is independent of whether the
    // scrollbar is visible — otherwise the show_idx / show_inset_body
    // thresholds would flicker as content crosses the overflow boundary.
    let budget = column_layout(area.width);
    let (body_area, scrollbar_area, mut sb_state) =
        scrollbar_layout_for_view(inner, rows.len(), state.scroll_offset);

    let rows_ctx = RowsCtx {
        selected_row: state.selected_row,
        cache,
        entries: &ctx.entries,
        current_change_idx: ctx.current_index,
        budget,
        stale_count,
        total_count,
    };
    let lines = rows_to_lines(&rows, &rows_ctx);

    let widget = Paragraph::new(lines).scroll((state.scroll_offset, 0));
    frame.render_widget(widget, body_area);
    render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);

    frame.render_widget(Paragraph::new(OVERVIEW_FOOTER_TEXT), layout[1]);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::comment::{Anchor, Comment, SchemaVersion, Severity, Status};
    use crate::stack::{RevsetHash, StackEntry};

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn make_comment(change_id: &ChangeId, severity: Severity, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: change_id.clone(),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    fn make_stale_comment(change_id: &ChangeId, severity: Severity, body: &str) -> Comment {
        let mut c = make_comment(change_id, severity, body);
        c.status = Some(Status::Stale);
        c
    }

    fn make_stack_comment(severity: Severity, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: RevsetHash::from_revset("@"),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
        }
    }

    fn make_entry(id: &str, desc: &str) -> StackEntry {
        StackEntry {
            change_id: cid(id),
            commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
            description: desc.to_owned(),
        }
    }

    fn make_cache(stack_level: Vec<Comment>, per_change: Vec<Vec<Comment>>) -> OverviewCommentSet {
        OverviewCommentSet {
            stack_level,
            per_change,
            orphaned: vec![],
        }
    }

    #[test]
    fn severity_histogram_excludes_stale() {
        let id = cid("abc11111");
        let comments = vec![
            make_comment(&id, Severity::Required, "active"),
            make_stale_comment(&id, Severity::Required, "stale"),
        ];
        let hist = SeverityHistogram::from_comments(&comments);
        assert_eq!(hist.required, 1, "stale should not be counted");
        assert_eq!(hist.total(), 1);
    }

    #[test]
    fn dot_string_zero_is_empty() {
        assert_eq!(dot_string(SeverityHistogram::default()), "");
    }

    #[test]
    fn dot_string_two_required_renders_two_dots_and_count() {
        let hist = SeverityHistogram {
            required: 2,
            suggestion: 0,
            note: 0,
        };
        let s = dot_string(hist);
        assert!(s.contains("●●"), "expected two dots, got: {s:?}");
        assert!(s.contains('2'), "expected count 2, got: {s:?}");
    }

    #[test]
    fn dot_string_mixed_severities_caps_total_globally() {
        // (3, 3, 0) totals 6 — over the cap of 5. Must emit at most 5 dots
        // before the `…`, total of 6 glyphs (5 dots + 1 ellipsis).
        let hist = SeverityHistogram {
            required: 3,
            suggestion: 3,
            note: 0,
        };
        let s = dot_string(hist);
        let dot_count = s.chars().filter(|&c| c == '●').count();
        assert!(
            dot_count <= 5,
            "global cap is 5 dots; got {dot_count}: {s:?}"
        );
        assert!(s.contains('…'), "overflow indicator missing: {s:?}");
        assert!(s.contains('6'), "true count must still be present: {s:?}");
    }

    #[test]
    fn dot_string_at_cap_no_ellipsis() {
        let hist = SeverityHistogram {
            required: 3,
            suggestion: 2,
            note: 0,
        };
        let s = dot_string(hist);
        assert_eq!(s.chars().filter(|&c| c == '●').count(), 5);
        assert!(!s.contains('…'));
    }

    #[test]
    fn strip_removes_vertical_bar() {
        let s = "hello │ world";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains('│'), "got: {out:?}");
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn strip_removes_various_box_drawing() {
        let s = "┌──┐ text ├──┤";
        let out = strip_box_drawing_and_ansi(s);
        for c in out.chars() {
            assert!(!('\u{2500}'..='\u{257f}').contains(&c), "got: {out:?}");
        }
        assert!(out.contains("text"));
    }

    #[test]
    fn strip_removes_csi_escape() {
        let s = "plain \x1b[31mred\x1b[0m text";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains('\x1b'), "ESC must be removed; got: {out:?}");
        assert!(out.contains("plain"));
        assert!(out.contains("red"));
        assert!(out.contains("text"));
    }

    #[test]
    fn strip_removes_osc_with_bel_terminator() {
        // OSC 8 hyperlink with BEL terminator. The payload between `]` and BEL
        // must be dropped along with the introducer and terminator.
        let s = "before\x1b]8;;https://evil/\x07click\x1b]8;;\x07after";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains('\x1b'), "ESC must be removed: {out:?}");
        assert!(
            !out.contains("https"),
            "OSC payload must be removed: {out:?}"
        );
        assert!(out.contains("before"));
        assert!(out.contains("click"));
        assert!(out.contains("after"));
    }

    #[test]
    fn strip_removes_osc_with_st_terminator() {
        // ST is `ESC \`. Payload includes printable text that must be removed.
        let s = "a\x1b]0;title text\x1b\\b";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains("title"), "OSC payload leaks: {out:?}");
        assert_eq!(out, "ab");
    }

    #[test]
    fn strip_removes_dcs_sequence() {
        let s = "x\x1bP1;2qpayload\x1b\\y";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains("payload"), "DCS payload leaks: {out:?}");
        assert_eq!(out, "xy");
    }

    #[test]
    fn strip_removes_apc_sequence() {
        let s = "x\x1b_anything\x07y";
        let out = strip_box_drawing_and_ansi(s);
        assert_eq!(out, "xy");
    }

    #[test]
    fn strip_removes_bidi_marks() {
        let s = "foo\u{202e}bar";
        let out = strip_box_drawing_and_ansi(s);
        assert!(!out.contains('\u{202e}'), "got: {out:?}");
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn strip_preserves_plain_text() {
        let s = "normal comment body text";
        assert_eq!(strip_box_drawing_and_ansi(s), s);
    }

    #[test]
    fn column_layout_120_shows_idx_and_inset() {
        let b = column_layout(120);
        assert!(b.show_idx);
        assert!(b.show_inset_body);
    }

    #[test]
    fn column_layout_100_shows_idx() {
        let b = column_layout(100);
        assert!(b.show_idx, "≥100 → show idx");
    }

    #[test]
    fn column_layout_99_no_idx() {
        let b = column_layout(99);
        assert!(!b.show_idx, "<100 → drop idx");
    }

    #[test]
    fn column_layout_80_shows_inset_body() {
        let b = column_layout(80);
        assert!(b.show_inset_body, "≥80 → show inset body");
    }

    #[test]
    fn column_layout_79_no_inset_body() {
        let b = column_layout(79);
        assert!(!b.show_inset_body, "<80 → drop inset text");
    }

    #[test]
    fn build_rows_empty_stack_has_header_and_separator() {
        let rows = build_rows(&make_cache(vec![], vec![]), &[], 0, 0);
        assert!(rows.iter().any(|r| matches!(r, OverviewRow::StackHeader)));
        assert!(rows.iter().any(|r| matches!(r, OverviewRow::Separator)));
    }

    #[test]
    fn build_rows_skips_summary_footer_when_empty_counts() {
        let rows = build_rows(&make_cache(vec![], vec![]), &[], 0, 0);
        assert!(!rows
            .iter()
            .any(|r| matches!(r, OverviewRow::SummaryFooterStale)));
        assert!(!rows
            .iter()
            .any(|r| matches!(r, OverviewRow::SummaryFooterTotal)));
    }

    #[test]
    fn build_rows_summary_footer_emits_two_lines() {
        let rows = build_rows(&make_cache(vec![], vec![]), &[], 0, 1);
        assert!(rows
            .iter()
            .any(|r| matches!(r, OverviewRow::SummaryFooterStale)));
        assert!(rows
            .iter()
            .any(|r| matches!(r, OverviewRow::SummaryFooterTotal)));
    }

    #[test]
    fn build_rows_stack_comments_appear_before_separator() {
        let comments = vec![make_stack_comment(Severity::Note, "note")];
        let rows = build_rows(&make_cache(comments, vec![]), &[], 0, 0);
        let sep_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::Separator))
            .unwrap();
        let comment_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::StackComment(0)))
            .unwrap();
        assert!(comment_pos < sep_pos);
    }

    #[test]
    fn build_rows_change_rows_after_separator() {
        let entries = vec![make_entry("abc11111", "first change")];
        let rows = build_rows(&make_cache(vec![], vec![vec![]]), &entries, 0, 0);
        let sep_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::Separator))
            .unwrap();
        let cr_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::ChangeRow(0)))
            .unwrap();
        assert!(cr_pos > sep_pos);
    }

    #[test]
    fn build_rows_change_comment_follows_change_row() {
        let id = cid("abc11111");
        let entry = make_entry("abc11111", "first change");
        let comment = make_comment(&id, Severity::Note, "note body");
        let rows = build_rows(&make_cache(vec![], vec![vec![comment]]), &[entry], 0, 1);
        let change_row_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::ChangeRow(0)))
            .unwrap();
        let inset_pos = rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    OverviewRow::ChangeComment {
                        change_idx: 0,
                        comment_idx: 0
                    }
                )
            })
            .unwrap();
        assert_eq!(inset_pos, change_row_pos + 1);
    }

    #[test]
    fn move_cursor_skips_separator() {
        let entries = vec![make_entry("abc11111", "first")];
        let rows = build_rows(&make_cache(vec![], vec![vec![]]), &entries, 0, 0);
        let sep_pos = rows
            .iter()
            .position(|r| matches!(r, OverviewRow::Separator))
            .unwrap();
        let mut cur = 0;
        for _ in 0..sep_pos + 2 {
            cur = move_cursor(&rows, cur, 1);
        }
        assert!(!matches!(rows[cur], OverviewRow::Separator));
    }

    #[test]
    fn move_cursor_skips_summary_footer_lines() {
        let entries = vec![make_entry("abc11111", "first")];
        let rows = build_rows(&make_cache(vec![], vec![vec![]]), &entries, 0, 1);
        // Walk down past every navigable row; cursor should never land on
        // either summary footer line.
        let mut cur = 0;
        for _ in 0..rows.len() * 2 {
            cur = move_cursor(&rows, cur, 1);
            assert!(
                !matches!(
                    rows[cur],
                    OverviewRow::SummaryFooterStale | OverviewRow::SummaryFooterTotal
                ),
                "cursor must not land on a summary footer row"
            );
        }
    }

    #[test]
    fn move_cursor_up_from_first_stays() {
        let rows = build_rows(&make_cache(vec![], vec![]), &[], 0, 0);
        let start = clamp_selected(&rows, 0);
        let new = move_cursor(&rows, start, -1);
        assert_eq!(new, start);
    }

    #[test]
    fn compute_scroll_offset_selected_in_view() {
        let offset = compute_scroll_offset(3, 10, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn compute_scroll_offset_selected_below_viewport() {
        let offset = compute_scroll_offset(15, 10, 0);
        assert!(offset > 0);
    }

    #[test]
    fn compute_scroll_offset_selected_above_current_offset() {
        let offset = compute_scroll_offset(2, 10, 10);
        assert_eq!(offset, 2);
    }

    #[test]
    fn overview_footer_fits_within_80_cols() {
        let max = 80usize;
        assert!(
            OVERVIEW_FOOTER_TEXT.chars().count() <= max,
            "footer {:?} ({} chars) exceeds {max} cols",
            OVERVIEW_FOOTER_TEXT,
            OVERVIEW_FOOTER_TEXT.chars().count()
        );
    }

    /// At 80 cols (`inner_width` = 78), the rendered change-row line must be
    /// exactly `inner_width` chars wide. Earlier the line was 2 chars long
    /// because the literal `"  "` separator was uncounted, causing ratatui
    /// to clip the trailing dots/count.
    #[test]
    fn render_change_row_line_width_matches_inner_width_at_80_cols() {
        let entry = make_entry("abc11111", "short desc");
        let id = cid("abc11111");
        let per_change = vec![vec![make_comment(&id, Severity::Required, "r")]];
        let budget = column_layout(80);
        let line = render_change_row_line(ChangeRowArgs {
            entry: &entry,
            change_idx: 0,
            per_change_comments: &per_change,
            budget,
            is_current: false,
            is_selected: false,
        });
        let span = &line.spans[0];
        let len = span.content.chars().count();
        assert_eq!(
            len, budget.inner_width,
            "rendered line must be exactly inner_width ({}) chars; got {}: {:?}",
            budget.inner_width, len, span.content
        );
    }

    #[test]
    fn render_change_row_line_width_matches_inner_width_at_120_cols() {
        let entry = make_entry("abc11111", "longer change description here");
        let per_change: Vec<Vec<Comment>> = vec![vec![]];
        let budget = column_layout(120);
        let line = render_change_row_line(ChangeRowArgs {
            entry: &entry,
            change_idx: 0,
            per_change_comments: &per_change,
            budget,
            is_current: false,
            is_selected: false,
        });
        let span = &line.spans[0];
        assert_eq!(span.content.chars().count(), budget.inner_width);
        // Zero-comment row should show ✓.
        assert!(span.content.contains('\u{2713}'));
    }

    #[test]
    fn render_change_row_line_uses_distinct_glyphs_for_selected_and_current() {
        let entry = make_entry("abc11111", "desc");
        let per_change: Vec<Vec<Comment>> = vec![vec![]];
        let budget = column_layout(120);

        let selected_only = render_change_row_line(ChangeRowArgs {
            entry: &entry,
            change_idx: 0,
            per_change_comments: &per_change,
            budget,
            is_current: false,
            is_selected: true,
        });
        let current_only = render_change_row_line(ChangeRowArgs {
            entry: &entry,
            change_idx: 0,
            per_change_comments: &per_change,
            budget,
            is_current: true,
            is_selected: false,
        });
        let s_text = &selected_only.spans[0].content;
        let c_text = &current_only.spans[0].content;
        // Selection cursor is U+25B6 (▶); current-change indicator is U+25B8 (▸).
        assert!(
            s_text.starts_with('\u{25b6}'),
            "selected row must start with ▶: {s_text:?}"
        );
        assert!(
            c_text.starts_with('\u{25b8}'),
            "current-change row must start with ▸: {c_text:?}"
        );
    }
}
