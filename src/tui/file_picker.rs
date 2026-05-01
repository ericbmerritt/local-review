use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::comment::{Comment, Status};
use crate::diff::DiffFile;

use super::composer_overlay::centered_rect;
use super::{render_view_scrollbar, scrollbar_layout_for_view};

const PICKER_WIDTH: u16 = 72;
const PICKER_HEIGHT: u16 = 20;

pub(super) const FILE_PICKER_FOOTER: &str = " \u{2191}\u{2193} select  Enter open  q back";

#[derive(Debug, Clone)]
pub(super) struct FilePickerState {
    pub(super) selected_index: usize,
    pub(super) scroll_offset: u16,
    pub(super) entries: Vec<FilePickerEntry>,
}

#[derive(Debug, Clone)]
pub(super) struct FilePickerEntry {
    pub(super) display_path: PathBuf,
    /// Index into `App::rendered_per_file` / `App::annotated_per_file`.
    /// Index 0 is the synthetic description view; indices 1+ are diff files.
    pub(super) view_index: usize,
    pub(super) comment_count: usize,
    /// Whether the user has already landed on this view in the current
    /// `(change_id, commit_id)` pair. Drives the `[✓]` / `[ ]` indicator on
    /// the right edge of each row.
    pub(super) reviewed: bool,
}

pub(super) fn build_entries(
    files: &[DiffFile],
    comments: &[Comment],
    is_reviewed: &dyn Fn(usize) -> bool,
) -> Vec<FilePickerEntry> {
    let mut entries = Vec::with_capacity(files.len() + 1);

    let description_comments = comments
        .iter()
        .filter(|c| {
            if matches!(c.status, Some(Status::Stale | Status::Orphaned)) {
                return false;
            }
            matches!(c.anchor, crate::comment::Anchor::Description { .. })
        })
        .count();
    entries.push(FilePickerEntry {
        display_path: PathBuf::from("<description>"),
        view_index: 0,
        comment_count: description_comments,
        reviewed: is_reviewed(0),
    });

    for (diff_file_index, file) in files.iter().enumerate() {
        let display_path = file.display_path().to_owned();
        let comment_count = comments
            .iter()
            .filter(|c| {
                if matches!(c.status, Some(Status::Stale | Status::Orphaned)) {
                    return false;
                }
                let crate::comment::Anchor::Line { location, .. } = &c.anchor else {
                    return false;
                };
                location.file.as_path() == display_path.as_path()
            })
            .count();
        let view_index = diff_file_index + 1;
        entries.push(FilePickerEntry {
            display_path,
            view_index,
            comment_count,
            reviewed: is_reviewed(view_index),
        });
    }

    entries
}

pub(super) fn render(frame: &mut Frame<'_>, state: &FilePickerState) {
    let area = frame.area();
    let overlay = centered_rect(area, PICKER_WIDTH, PICKER_HEIGHT);
    frame.render_widget(Clear, overlay);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Files in change");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let body_height = inner.height.saturating_sub(1);
    let scroll = state.scroll_offset;
    let visible = &state.entries;

    let body_area = ratatui::layout::Rect {
        height: body_height,
        ..inner
    };
    let footer_area = ratatui::layout::Rect {
        y: inner.y + body_height,
        height: 1,
        ..inner
    };

    let (text_area, scrollbar_area, mut sb_state) =
        scrollbar_layout_for_view(body_area, visible.len(), scroll);
    let inner_width = usize::from(text_area.width);

    let mut lines: Vec<TuiLine<'_>> = Vec::with_capacity(visible.len());
    for (idx, entry) in visible.iter().enumerate() {
        let cursor = if idx == state.selected_index {
            "\u{25b6} "
        } else {
            "  "
        };
        let reviewed_part = format!("  {}", reviewed_indicator_text(entry.reviewed));
        let count_part = if entry.comment_count > 0 {
            format!("  [{}]", entry.comment_count)
        } else {
            String::new()
        };
        let cursor_chars = cursor.chars().count();
        let reviewed_chars = reviewed_part.chars().count();
        let count_chars = count_part.chars().count();
        let path_budget = inner_width
            .saturating_sub(cursor_chars)
            .saturating_sub(reviewed_chars)
            .saturating_sub(count_chars);
        let raw_path = entry.display_path.display().to_string();
        let path = truncate_path_for_width(&raw_path, path_budget);

        let base_style = if idx == state.selected_index {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let reviewed_style = if entry.reviewed {
            base_style.fg(Color::Green)
        } else {
            base_style
        };
        lines.push(TuiLine::from(vec![
            Span::styled(format!("{cursor}{path}"), base_style),
            Span::styled(reviewed_part, reviewed_style),
            Span::styled(count_part, base_style),
        ]));
    }

    let widget = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(widget, text_area);
    render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);

    let footer_widget = Paragraph::new(FILE_PICKER_FOOTER);
    frame.render_widget(footer_widget, footer_area);
}

/// `[✓]` when reviewed, `[ ]` when not — both variants render brackets so
/// the column stays aligned across rows.
pub(super) fn reviewed_indicator_text(reviewed: bool) -> String {
    if reviewed {
        "[\u{2713}]".to_owned()
    } else {
        "[ ]".to_owned()
    }
}

/// Move cursor up or down, clamping to valid range.
pub(super) fn move_cursor(state: &mut FilePickerState, delta: isize) {
    let count = state.entries.len();
    if count == 0 {
        return;
    }
    let max_index = count - 1;
    if delta < 0 {
        state.selected_index = state.selected_index.saturating_sub(1);
    } else {
        state.selected_index = (state.selected_index + 1).min(max_index);
    }
    adjust_scroll(state);
}

fn adjust_scroll(state: &mut FilePickerState) {
    let sel = u16::try_from(state.selected_index).unwrap_or(u16::MAX);
    let visible_rows = PICKER_HEIGHT.saturating_sub(3);
    if sel < state.scroll_offset {
        state.scroll_offset = sel;
    }
    let last_visible = state.scroll_offset.saturating_add(visible_rows);
    if sel >= last_visible {
        state.scroll_offset = sel.saturating_sub(visible_rows.saturating_sub(1));
    }
}

/// Left-truncate a path with `…` so it fits in `available_width` columns,
/// preserving the rightmost characters (filename + nearest dirs read better
/// than a truncated tail). When the path already fits, returned unchanged.
/// `available_width` of 0 returns empty string; 1 returns just `…`. Otherwise
/// reserves 1 cell for the leading `…` and takes the last
/// `available_width - 1` characters of the path.
pub(super) fn truncate_path_for_width(path: &str, available_width: usize) -> String {
    let path_chars = path.chars().count();
    if path_chars <= available_width {
        return path.to_owned();
    }
    if available_width == 0 {
        return String::new();
    }
    if available_width == 1 {
        return "\u{2026}".to_owned();
    }
    let take = available_width - 1;
    let skip = path_chars - take;
    let tail: String = path.chars().skip(skip).collect();
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_id::ChangeId;
    use crate::comment::{Anchor, LineAnchor, SchemaVersion, Severity, Side, Status};
    use crate::diff::{DiffFile, Hunk, Line, LineKind};
    use std::path::PathBuf;

    fn diff_files() -> Vec<DiffFile> {
        vec![
            DiffFile::Modified {
                path: PathBuf::from("foo.rs"),
                hunks: vec![Hunk {
                    header: "@@ -1,1 +1,1 @@".to_owned(),
                    function_context: None,
                    source_start: 1,
                    source_length: 1,
                    target_start: 1,
                    target_length: 1,
                    lines: vec![Line {
                        kind: LineKind::Context,
                        text: "ctx".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    }],
                }],
            },
            DiffFile::Modified {
                path: PathBuf::from("bar.rs"),
                hunks: vec![],
            },
        ]
    }

    fn make_comment(file: &str, status: Option<Status>) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
                    target_text: "ctx".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "hello".to_owned(),
            severity: Severity::Note,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status,
            mismatch_reason: None,
        }
    }

    #[test]
    fn build_entries_counts_active_comments_per_file() {
        let files = diff_files();
        let comments = vec![
            make_comment("foo.rs", Some(Status::Pending)),
            make_comment("foo.rs", Some(Status::Pending)),
            make_comment("bar.rs", Some(Status::Pending)),
            make_comment("foo.rs", Some(Status::Stale)),
        ];
        let entries = build_entries(&files, &comments, &|_| false);
        // entry 0 is the description view
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].display_path, PathBuf::from("<description>"));
        assert_eq!(entries[0].view_index, 0);
        assert_eq!(entries[1].display_path, PathBuf::from("foo.rs"));
        assert_eq!(entries[1].view_index, 1);
        assert_eq!(entries[1].comment_count, 2);
        assert_eq!(entries[2].display_path, PathBuf::from("bar.rs"));
        assert_eq!(entries[2].view_index, 2);
        assert_eq!(entries[2].comment_count, 1);
    }

    #[test]
    fn build_entries_excludes_stale_and_orphaned() {
        let files = diff_files();
        let comments = vec![
            make_comment("foo.rs", Some(Status::Stale)),
            make_comment("foo.rs", Some(Status::Orphaned)),
        ];
        let entries = build_entries(&files, &comments, &|_| false);
        // entry 0 is description, entries 1+ are diff files
        assert_eq!(entries[1].comment_count, 0);
    }

    #[test]
    fn build_entries_empty_comments_all_zero() {
        let files = diff_files();
        let entries = build_entries(&files, &[], &|_| false);
        assert!(entries.iter().all(|e| e.comment_count == 0));
    }

    #[test]
    fn build_entries_reviewed_flag_propagates_per_view_index() {
        let files = diff_files();
        // Mark only `foo.rs` (view_index=1) reviewed.
        let entries = build_entries(&files, &[], &|view_idx| view_idx == 1);
        assert!(!entries[0].reviewed, "description must be unreviewed");
        assert!(entries[1].reviewed, "foo.rs must be reviewed");
        assert!(!entries[2].reviewed, "bar.rs must be unreviewed");
    }

    #[test]
    fn reviewed_indicator_text_uses_check_when_reviewed() {
        assert_eq!(reviewed_indicator_text(true), "[\u{2713}]");
    }

    #[test]
    fn reviewed_indicator_text_uses_blank_when_unreviewed() {
        // Both brackets must render even when unmarked, so the ✓/space
        // column lines up across rows.
        assert_eq!(reviewed_indicator_text(false), "[ ]");
    }

    #[test]
    fn move_cursor_down_increments_index() {
        let files = diff_files();
        let mut state = FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: build_entries(&files, &[], &|_| false),
        };
        move_cursor(&mut state, 1);
        assert_eq!(state.selected_index, 1);
    }

    #[test]
    fn move_cursor_up_clamps_at_zero() {
        let files = diff_files();
        let mut state = FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: build_entries(&files, &[], &|_| false),
        };
        move_cursor(&mut state, -1);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn move_cursor_down_clamps_at_last() {
        let files = diff_files();
        let entries = build_entries(&files, &[], &|_| false);
        let entry_count = entries.len();
        let mut state = FilePickerState {
            selected_index: entry_count - 1,
            scroll_offset: 0,
            entries,
        };
        move_cursor(&mut state, 1);
        assert_eq!(state.selected_index, entry_count - 1);
    }

    #[test]
    fn build_entries_view_indices_start_at_zero_for_description() {
        let files = diff_files();
        let entries = build_entries(&files, &[], &|_| false);
        // entry 0 is description (view_index 0); diff files follow at 1, 2, ...
        assert_eq!(entries[0].view_index, 0);
        for (i, entry) in entries[1..].iter().enumerate() {
            assert_eq!(entry.view_index, i + 1);
        }
    }

    #[test]
    fn truncate_path_for_width_short_path_returned_unchanged() {
        let path = "src/foo.rs";
        let result = truncate_path_for_width(path, 80);
        assert_eq!(result, path);
    }

    #[test]
    fn truncate_path_for_width_exact_fit_returned_unchanged() {
        let path = "src/foo.rs"; // 10 chars
        let result = truncate_path_for_width(path, 10);
        assert_eq!(result, path);
    }

    #[test]
    fn truncate_path_for_width_left_truncates_with_ellipsis() {
        let path = "src/some/deep/module/package/thing.rs"; // 37 chars
        let result = truncate_path_for_width(path, 20);
        assert_eq!(
            result.chars().count(),
            20,
            "result must fit exactly in budget; got: {result:?}"
        );
        assert!(
            result.starts_with('\u{2026}'),
            "result must begin with ellipsis; got: {result:?}"
        );
        assert!(
            result.ends_with("thing.rs"),
            "rightmost chars (filename) must be preserved; got: {result:?}"
        );
    }

    #[test]
    fn truncate_path_for_width_zero_budget_returns_empty() {
        let result = truncate_path_for_width("anything", 0);
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_path_for_width_one_budget_returns_just_ellipsis() {
        let result = truncate_path_for_width("anything", 1);
        assert_eq!(result, "\u{2026}");
    }

    #[test]
    fn truncate_path_for_width_two_budget_keeps_one_char_after_ellipsis() {
        let result = truncate_path_for_width("foo/bar.rs", 2);
        assert_eq!(result.chars().count(), 2);
        assert!(result.starts_with('\u{2026}'));
        assert!(result.ends_with('s'), "got: {result:?}");
    }

    /// Build a [`FilePickerState`] with `entry_count` synthetic entries so
    /// scrollbar tests can pick a count that overflows or fits the picker
    /// body. The first entry mirrors the synthetic `<description>` row to
    /// match what `build_entries` produces.
    fn make_state_with_entries(
        entry_count: usize,
        selected_index: usize,
        scroll_offset: u16,
    ) -> FilePickerState {
        let entries = (0..entry_count)
            .map(|i| FilePickerEntry {
                display_path: if i == 0 {
                    PathBuf::from("<description>")
                } else {
                    PathBuf::from(format!("file_{i}.rs"))
                },
                view_index: i,
                comment_count: 0,
                reviewed: false,
            })
            .collect();
        FilePickerState {
            selected_index,
            scroll_offset,
            entries,
        }
    }

    /// Render the file picker to a [`ratatui::buffer::Buffer`] sized
    /// `cols x rows` so tests can inspect glyph placement.
    fn render_picker_to_buffer(
        state: &FilePickerState,
        cols: u16,
        rows: u16,
    ) -> ratatui::buffer::Buffer {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(cols, rows);
        let mut terminal = Terminal::new(backend).expect("test terminal must construct");
        terminal
            .draw(|frame| render(frame, state))
            .expect("test draw must succeed");
        terminal.backend().buffer().clone()
    }

    use super::super::scrollbar_test_helpers::{col_contains_scrollbar_glyph, scrollbar_thumb_row};

    /// Picker body geometry. `PICKER_WIDTH` = 72, `PICKER_HEIGHT` = 20. With
    /// an 80x30 terminal, `centered_rect` anchors the overlay at x=4, y=5;
    /// the block's inner is x=5, width=70 -> rightmost inner column = 74.
    const PICKER_RIGHTMOST_INNER_COL: u16 = 74;

    #[test]
    fn scrollbar_renders_when_entries_overflow_picker_body() {
        // Body height is PICKER_HEIGHT(20) - borders(2) - footer(1) = 17.
        // 30 entries far exceeds that → scrollbar must render.
        let state = make_state_with_entries(30, 0, 0);
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert!(
            col_contains_scrollbar_glyph(&buf, PICKER_RIGHTMOST_INNER_COL),
            "scrollbar glyphs must appear in the rightmost picker body column when entries overflow"
        );
    }

    #[test]
    fn scrollbar_hidden_when_entries_fit_picker_body() {
        // 5 entries comfortably fit the 17-row body.
        let state = make_state_with_entries(5, 0, 0);
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert!(
            !col_contains_scrollbar_glyph(&buf, PICKER_RIGHTMOST_INNER_COL),
            "scrollbar must be hidden when entries fit the body"
        );
    }

    #[test]
    fn scrollbar_thumb_position_reflects_scroll_offset() {
        // Scroll near the top: thumb sits in the upper half of the body.
        let state_top = make_state_with_entries(60, 0, 0);
        let buf_top = render_picker_to_buffer(&state_top, 80, 30);
        let thumb_top = scrollbar_thumb_row(&buf_top, PICKER_RIGHTMOST_INNER_COL)
            .expect("thumb glyph must appear when entries overflow");

        // Scroll near the bottom: thumb sits in the lower half.
        let state_bot = make_state_with_entries(60, 59, 50);
        let buf_bot = render_picker_to_buffer(&state_bot, 80, 30);
        let thumb_bot = scrollbar_thumb_row(&buf_bot, PICKER_RIGHTMOST_INNER_COL)
            .expect("thumb glyph must appear when entries overflow");

        assert!(
            thumb_top < thumb_bot,
            "thumb must move down as scroll_offset advances; top={thumb_top}, bottom={thumb_bot}"
        );
    }
}
