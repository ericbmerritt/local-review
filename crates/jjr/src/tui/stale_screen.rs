use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::comment::{Anchor, Comment, MismatchReason, Side, Status};
use crate::diff::Diff;
use crate::util::truncate;

use super::{render_view_scrollbar, scrollbar_layout_for_view, severity_color, severity_label};

const STALE_MIN_COLS: u16 = 60;

/// Inner-area width below which we drop the right-edge mismatch-reason label
/// and render it on its own indented line.
const WIDE_INNER_THRESHOLD: u16 = 80;

/// Maximum body characters shown in the single-line truncated preview. Sized
/// so `  "<body>"` fits in a 78-col inner area (2 indent + 2 quotes + 74).
const BODY_PREVIEW_MAX: usize = 74;

/// Maximum `was:` / `now:` text characters shown inline.
const LINE_TEXT_MAX: usize = 60;

/// Rendered row count for an `Anchor::Line` stale entry. Wide layout: header,
/// separator, severity, body, was, now, blank = 7. Narrow layout: same plus
/// a dedicated reason row = 8. Must match `render_entries`'s emit-count for
/// the line-anchor branch.
const ENTRY_LINES_WIDE: u16 = 7;
const ENTRY_LINES_NARROW: u16 = 8;

/// Rendered row count for a non-`Anchor::Line` stale entry. `render_entries`
/// early-continues for these after the body row, so it emits header,
/// separator, severity, body, blank = 5 (wide) and the same plus a reason
/// row = 6 (narrow). Must match the non-line render path's emit-count.
const NON_LINE_ENTRY_LINES_WIDE: u16 = 5;
const NON_LINE_ENTRY_LINES_NARROW: u16 = 6;

pub(super) const STALE_FOOTER_TEXT: &str =
    " \u{2191}\u{2193} select  Enter view in source  d delete  e edit & re-anchor  q back";

pub(super) struct StaleScreenState {
    pub(super) selected_index: usize,
    /// Indices into `App.loaded_comments` for the stale comments shown here.
    pub(super) stale_indices: Vec<usize>,
    /// First entry-row visible at the top of the rendered list. Recomputed at
    /// render time to keep `selected_index` in view.
    pub(super) scroll_offset: u16,
}

pub(super) enum NowLine {
    Text(String),
    NotPresent,
}

pub(super) fn stale_comment_indices(comments: &[Comment]) -> Vec<usize> {
    comments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.status == Some(Status::Stale))
        .map(|(i, _)| i)
        .collect()
}

/// Look up the current diff text at the comment's anchor location.
pub(super) fn current_line_at(comment: &Comment, diff: &Diff) -> NowLine {
    let Anchor::Line { location, .. } = &comment.anchor else {
        return NowLine::NotPresent;
    };

    let diff_file = diff
        .files
        .iter()
        .find(|f| f.display_path() == location.file.as_path());

    let Some(diff_file) = diff_file else {
        return NowLine::NotPresent;
    };

    for hunk in diff_file.hunks() {
        for line in &hunk.lines {
            let matches = match location.side {
                Side::Old => location.old_line.is_some() && line.source_line == location.old_line,
                Side::New => location.new_line.is_some() && line.target_line == location.new_line,
            };
            if matches {
                return NowLine::Text(line.text.clone());
            }
        }
    }

    NowLine::NotPresent
}

/// Rendered row count for one stale entry, branching on its anchor shape.
/// `Anchor::Line` is the full render path; everything else (Description /
/// Change / Stack) early-continues after the body row, dropping the `was:`
/// and `now:` lines. The scrollbar's `content_length` and the scroll-offset
/// computation must use this so the thumb reflects what the user actually
/// sees.
pub(super) fn rendered_rows_for_anchor(anchor: &Anchor, is_wide: bool) -> u16 {
    if matches!(anchor, Anchor::Line { .. }) {
        if is_wide {
            ENTRY_LINES_WIDE
        } else {
            ENTRY_LINES_NARROW
        }
    } else if is_wide {
        NON_LINE_ENTRY_LINES_WIDE
    } else {
        NON_LINE_ENTRY_LINES_NARROW
    }
}

pub(super) fn compute_scroll_offset<'a, I>(
    anchors: I,
    selected_index: usize,
    inner_rows: u16,
    is_wide: bool,
    current_offset: u16,
) -> u16
where
    I: IntoIterator<Item = &'a Anchor>,
{
    let mut entry_top: u16 = 0;
    let mut entry_height: u16 = 0;
    let mut found = false;
    for (i, anchor) in anchors.into_iter().enumerate() {
        let h = rendered_rows_for_anchor(anchor, is_wide);
        if i == selected_index {
            entry_height = h;
            found = true;
            break;
        }
        entry_top = entry_top.saturating_add(h);
    }
    if !found {
        // selected_index out of range — leave offset as-is rather than scroll
        // to a phantom location.
        return current_offset;
    }
    let entry_bottom = entry_top.saturating_add(entry_height);

    if entry_top < current_offset {
        return entry_top;
    }
    let last_visible = current_offset.saturating_add(inner_rows);
    if entry_bottom > last_visible {
        return entry_bottom.saturating_sub(inner_rows);
    }
    current_offset
}

/// Total rendered row count for the given stale-entry anchors at the given
/// layout. The scrollbar takes this as `total_lines` so the thumb's length
/// reflects rendered rows, matching `scroll_offset`'s row-based semantics.
pub(super) fn total_rendered_rows<'a, I>(anchors: I, is_wide: bool) -> usize
where
    I: IntoIterator<Item = &'a Anchor>,
{
    anchors
        .into_iter()
        .map(|a| usize::from(rendered_rows_for_anchor(a, is_wide)))
        .sum()
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    state: &mut StaleScreenState,
    loaded_comments: &[Comment],
    diff: &Diff,
) {
    let area = frame.area();

    if area.width < STALE_MIN_COLS {
        let msg = Paragraph::new("Terminal too narrow to render stale comments view.");
        frame.render_widget(msg, area);
        return;
    }

    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let count = state.stale_indices.len();
    let title = format!("Stale comments \u{00b7} {count}");
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(layout[0]);
    frame.render_widget(block, layout[0]);

    if count == 0 {
        let empty_msg = Paragraph::new("  No stale comments.");
        frame.render_widget(empty_msg, inner);
    } else {
        let is_wide = inner.width >= WIDE_INNER_THRESHOLD;
        // Collect the anchors of the visible stale entries once. Both the
        // scroll-offset calc and the scrollbar's content_length need to walk
        // these to honor variable per-entry row counts (Description / Change
        // / Stack anchors render shorter than Line anchors).
        let anchors: Vec<&Anchor> = state
            .stale_indices
            .iter()
            .filter_map(|&idx| loaded_comments.get(idx))
            .map(|c| &c.anchor)
            .collect();

        state.scroll_offset = compute_scroll_offset(
            anchors.iter().copied(),
            state.selected_index,
            inner.height,
            is_wide,
            state.scroll_offset,
        );

        let total_rows = total_rendered_rows(anchors.iter().copied(), is_wide);
        let (body_area, scrollbar_area, mut sb_state) =
            scrollbar_layout_for_view(inner, total_rows, state.scroll_offset);
        render_entries(frame, body_area, state, loaded_comments, diff, is_wide);
        render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);
    }

    let footer = Paragraph::new(STALE_FOOTER_TEXT);
    frame.render_widget(footer, layout[1]);
}

/// Build the ratatui line list `render_entries` would push to the paragraph
/// for the given state/comments/diff/layout. Pure (no `Frame`) so tests can
/// pin the emit count against [`total_rendered_rows`] without a `TestBackend`.
pub(super) fn build_entry_lines<'a>(
    width: u16,
    state: &StaleScreenState,
    loaded_comments: &'a [Comment],
    diff: &'a Diff,
    is_wide: bool,
) -> Vec<TuiLine<'a>> {
    let mut lines: Vec<TuiLine<'a>> = Vec::new();

    for (display_idx, &comment_idx) in state.stale_indices.iter().enumerate() {
        let Some(comment) = loaded_comments.get(comment_idx) else {
            continue;
        };

        let is_selected = display_idx == state.selected_index;
        let cursor = if is_selected { "\u{25b6} " } else { "  " };

        let (file_label, line_label) = anchor_labels(comment);
        let reason_label = mismatch_label(comment.mismatch_reason);

        if is_wide {
            let header_body = format!("{cursor}{file_label} \u{00b7} {line_label}");
            let header_used = header_body.chars().count();
            let reason_chars = reason_label.chars().count();
            let pad = usize::from(width)
                .saturating_sub(header_used)
                .saturating_sub(reason_chars);
            let padding = " ".repeat(pad);
            let header_text = format!("{header_body}{padding}{reason_label}");

            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(TuiLine::from(Span::styled(header_text, header_style)));
        } else {
            let header_text = format!("{cursor}{file_label} \u{00b7} {line_label}");
            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(TuiLine::from(Span::styled(header_text, header_style)));
            lines.push(TuiLine::from(Span::raw(format!("      {reason_label}"))));
        }

        let sep_width = usize::from(width).saturating_sub(2);
        let sep = format!(
            "  \u{2576}{}\u{2574}",
            "\u{2500}".repeat(sep_width.saturating_sub(2))
        );
        lines.push(TuiLine::from(Span::raw(sep)));

        let dot_color = severity_color(comment.severity);
        let sev_label = severity_label(comment.severity);
        let sev_line = TuiLine::from(vec![
            Span::raw("  "),
            Span::styled("\u{25cf} ", Style::default().fg(dot_color)),
            Span::styled(sev_label, Style::default().fg(dot_color)),
        ]);
        lines.push(sev_line);

        let body_preview = truncate(comment.body.lines().next().unwrap_or(""), BODY_PREVIEW_MAX);
        lines.push(TuiLine::from(Span::raw(format!("  \"{body_preview}\""))));

        let Anchor::Line { location, .. } = &comment.anchor else {
            lines.push(TuiLine::default());
            continue;
        };
        let was_text = truncate(&location.target_text, LINE_TEXT_MAX);
        lines.push(TuiLine::from(Span::raw(format!("  was:    {was_text}"))));

        let now_text = match current_line_at(comment, diff) {
            NowLine::Text(t) => truncate(&t, LINE_TEXT_MAX),
            NowLine::NotPresent => "(line not present in current diff)".to_owned(),
        };
        lines.push(TuiLine::from(Span::raw(format!("  now:    {now_text}"))));

        lines.push(TuiLine::default());
    }

    lines
}

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are distinct rendering inputs; no natural grouping avoids the count"
)]
fn render_entries(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    state: &StaleScreenState,
    loaded_comments: &[Comment],
    diff: &Diff,
    is_wide: bool,
) {
    let lines = build_entry_lines(area.width, state, loaded_comments, diff, is_wide);
    let widget = Paragraph::new(lines).scroll((state.scroll_offset, 0));
    frame.render_widget(widget, area);
}

fn anchor_labels(comment: &Comment) -> (String, String) {
    let Anchor::Line { location, .. } = &comment.anchor else {
        return ("(non-line comment)".to_owned(), String::new());
    };
    let file = location.file.display().to_string();
    let line_num = location
        .new_line
        .or(location.old_line)
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    (file, format!("was line {line_num}"))
}

fn mismatch_label(reason: Option<MismatchReason>) -> &'static str {
    match reason {
        Some(
            MismatchReason::TargetTextChanged
            | MismatchReason::ContextBeforeChanged
            | MismatchReason::ContextAfterChanged,
        ) => "target_text changed",
        Some(MismatchReason::FileNotInDiff) => "file not in diff",
        Some(MismatchReason::AnchorNotFound) | None => "anchor not found",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::ChangeId;
    use crate::comment::{
        Anchor, Comment, LineAnchor, MismatchReason, SchemaVersion, Severity, Side, Status,
    };
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};

    fn cid() -> ChangeId {
        ChangeId::parse("abc12345").unwrap()
    }

    fn make_comment(status: Option<Status>, file: &str, new_line: u32) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: cid(),
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(new_line),
                    hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
                    target_text: "original text".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "comment body".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_stale(file: &str, line: u32) -> Comment {
        Comment {
            status: Some(Status::Stale),
            mismatch_reason: Some(MismatchReason::TargetTextChanged),
            ..make_comment(Some(Status::Stale), file, line)
        }
    }

    fn diff_with_file(path: &str, line_text: &str, target_line: u32) -> Diff {
        Diff {
            files: vec![DiffFile::Modified {
                path: PathBuf::from(path),
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".to_owned(),
                    function_context: None,
                    source_start: 1,
                    source_length: 1,
                    target_start: 1,
                    target_length: 1,
                    lines: vec![Line {
                        kind: LineKind::Context,
                        text: line_text.to_owned(),
                        source_line: Some(target_line),
                        target_line: Some(target_line),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn stale_comment_indices_empty_list() {
        assert!(stale_comment_indices(&[]).is_empty());
    }

    #[test]
    fn stale_comment_indices_no_stale() {
        let comments = vec![
            make_comment(Some(Status::Pending), "a.rs", 1),
            make_comment(None, "b.rs", 2),
        ];
        assert!(stale_comment_indices(&comments).is_empty());
    }

    #[test]
    fn stale_comment_indices_all_stale() {
        let comments = vec![make_stale("a.rs", 1), make_stale("b.rs", 2)];
        assert_eq!(stale_comment_indices(&comments), vec![0, 1]);
    }

    #[test]
    fn stale_comment_indices_mixed_status() {
        let comments = vec![
            make_comment(Some(Status::Pending), "a.rs", 1),
            make_stale("b.rs", 2),
            make_comment(None, "c.rs", 3),
            make_stale("d.rs", 4),
        ];
        assert_eq!(stale_comment_indices(&comments), vec![1, 3]);
    }

    #[test]
    fn stale_comment_indices_orphaned_excluded() {
        let comments = vec![
            Comment {
                status: Some(Status::Orphaned),
                ..make_comment(Some(Status::Orphaned), "a.rs", 1)
            },
            make_stale("b.rs", 2),
        ];
        assert_eq!(stale_comment_indices(&comments), vec![1]);
    }

    #[test]
    fn current_line_at_file_present_matching_anchor_returns_text() {
        let comment = make_stale("foo.rs", 1);
        let diff = diff_with_file("foo.rs", "current line text", 1);
        match current_line_at(&comment, &diff) {
            NowLine::Text(t) => assert_eq!(t, "current line text"),
            NowLine::NotPresent => panic!("expected Text"),
        }
    }

    #[test]
    fn current_line_at_file_present_anchor_line_absent_returns_not_present() {
        let comment = make_stale("foo.rs", 99);
        let diff = diff_with_file("foo.rs", "line at 1", 1);
        assert!(matches!(
            current_line_at(&comment, &diff),
            NowLine::NotPresent
        ));
    }

    #[test]
    fn current_line_at_file_not_in_diff_returns_not_present() {
        let comment = make_stale("foo.rs", 1);
        let diff = diff_with_file("bar.rs", "line", 1);
        assert!(matches!(
            current_line_at(&comment, &diff),
            NowLine::NotPresent
        ));
    }

    #[test]
    fn current_line_at_renamed_file_matches_to_path() {
        let comment = make_stale("new.rs", 1);
        let diff = Diff {
            files: vec![DiffFile::Renamed {
                from: PathBuf::from("old.rs"),
                to: PathBuf::from("new.rs"),
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".to_owned(),
                    function_context: None,
                    source_start: 1,
                    source_length: 1,
                    target_start: 1,
                    target_length: 1,
                    lines: vec![Line {
                        kind: LineKind::Context,
                        text: "renamed content".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    }],
                }],
            }],
        };
        match current_line_at(&comment, &diff) {
            NowLine::Text(t) => assert_eq!(t, "renamed content"),
            NowLine::NotPresent => panic!("expected Text for renamed file"),
        }
    }

    #[test]
    fn current_line_at_for_non_line_anchor_returns_not_present() {
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: crate::stack::RevsetHash::from_revset("@"),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "stack comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status: Some(Status::Stale),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let diff = diff_with_file("foo.rs", "line", 1);
        assert!(matches!(
            current_line_at(&comment, &diff),
            NowLine::NotPresent
        ));
    }

    #[test]
    fn stale_footer_fits_within_reasonable_width() {
        let max_width = 80usize;
        assert!(
            STALE_FOOTER_TEXT.chars().count() <= max_width,
            "footer {:?} ({} chars) exceeds {max_width} cols",
            STALE_FOOTER_TEXT,
            STALE_FOOTER_TEXT.chars().count()
        );
    }

    /// Build `n` line-anchor stale comments so scroll-offset and total-rows
    /// tests can pass a real `&[Anchor]` slice into the row-walking helpers.
    fn line_anchors(n: usize) -> Vec<Anchor> {
        (0..n)
            .map(|i| Anchor::Line {
                change_id: cid(),
                location: LineAnchor {
                    file: PathBuf::from(format!("file_{i}.rs")),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(u32::try_from(i + 1).unwrap_or(1)),
                    hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
                    target_text: "t".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            })
            .collect()
    }

    fn description_anchor() -> Anchor {
        use crate::comment::DescriptionAnchor;
        Anchor::Description {
            change_id: cid(),
            location: DescriptionAnchor {
                display_line: Some(1),
                target_text: "t".to_owned(),
                context_before: vec![],
                context_after: vec![],
            },
        }
    }

    #[test]
    fn compute_scroll_offset_first_entry_no_scroll() {
        let anchors = line_anchors(10);
        let offset = compute_scroll_offset(anchors.iter(), 0, 24, true, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn compute_scroll_offset_below_viewport_scrolls_down() {
        let anchors = line_anchors(10);
        let offset = compute_scroll_offset(anchors.iter(), 5, 14, true, 0);
        assert!(offset > 0, "expected scroll > 0 for entry 5, got {offset}");
        assert!(offset >= 28, "offset {offset} should be >= 28");
    }

    #[test]
    fn compute_scroll_offset_above_viewport_scrolls_up() {
        let anchors = line_anchors(10);
        let offset = compute_scroll_offset(anchors.iter(), 0, 14, true, 30);
        assert_eq!(offset, 0, "selecting entry 0 should snap offset to 0");
    }

    #[test]
    fn compute_scroll_offset_already_visible_no_change() {
        let anchors = line_anchors(10);
        let offset = compute_scroll_offset(anchors.iter(), 1, 14, true, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn compute_scroll_offset_narrow_layout_uses_taller_per_entry() {
        let anchors = line_anchors(10);
        let wide = compute_scroll_offset(anchors.iter(), 5, 14, true, 0);
        let narrow = compute_scroll_offset(anchors.iter(), 5, 14, false, 0);
        assert!(
            narrow > wide,
            "narrow ({narrow}) should require more scroll than wide ({wide})"
        );
    }

    #[test]
    fn rendered_rows_for_anchor_line_matches_constants() {
        let line = &line_anchors(1)[0];
        assert_eq!(rendered_rows_for_anchor(line, true), ENTRY_LINES_WIDE);
        assert_eq!(rendered_rows_for_anchor(line, false), ENTRY_LINES_NARROW);
    }

    #[test]
    fn rendered_rows_for_anchor_description_drops_was_now_lines() {
        let desc = description_anchor();
        // Wide non-line: 5 rows (header, sep, sev, body, blank). Narrow: 6
        // (extra reason row).
        assert_eq!(rendered_rows_for_anchor(&desc, true), 5);
        assert_eq!(rendered_rows_for_anchor(&desc, false), 6);
    }

    #[test]
    fn total_rendered_rows_sums_per_anchor_height_for_mixed_entries() {
        let mut anchors = line_anchors(2);
        anchors.push(description_anchor());
        // Wide: 7 + 7 + 5 = 19.
        assert_eq!(total_rendered_rows(anchors.iter(), true), 19);
        // Narrow: 8 + 8 + 6 = 22.
        assert_eq!(total_rendered_rows(anchors.iter(), false), 22);
    }
}
