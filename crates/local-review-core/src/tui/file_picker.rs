//! File picker overlay — jump-to-file / description navigation.
//!
//! Pure ratatui rendering with no IO. The caller provides per-view comment
//! counts and reviewed state via closures; this module never touches a concrete
//! comment type.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::composer_overlay::centered_rect;
use super::{render_view_scrollbar, scrollbar_layout_for_view};

const PICKER_WIDTH: u16 = 72;
const PICKER_HEIGHT: u16 = 20;

pub const FILE_PICKER_FOOTER: &str = " \u{2191}\u{2193} select  Enter open  q back";

#[derive(Debug, Clone)]
pub struct FilePickerState {
    pub selected_index: usize,
    pub scroll_offset: u16,
    pub entries: Vec<FilePickerEntry>,
}

#[derive(Debug, Clone)]
pub struct FilePickerEntry {
    pub display_path: PathBuf,
    /// Index into the surface's rendered view list.
    /// Index 0 is the synthetic description view; indices 1+ are diff files.
    pub view_index: usize,
    pub comment_count: usize,
    /// Whether the user has already landed on this view in the current
    /// `(change_id, commit_id)` pair. Drives the `✓` glyph on the row;
    /// unreviewed rows render an equivalent-width blank so the path column
    /// stays aligned across states.
    pub reviewed: bool,
}

/// Build file picker entries from diff file metadata.
///
/// `comment_count_for_view` is called with the view index (0 = description,
/// 1..N = diff file N-1) and returns the number of active (non-stale,
/// non-orphaned) comments for that view.
///
/// `is_reviewed` is called with the view index and returns whether the view
/// has been marked reviewed.
pub fn build_entries(
    files: &[crate::diff::DiffFile],
    comment_count_for_view: &dyn Fn(usize) -> usize,
    is_reviewed: &dyn Fn(usize) -> bool,
) -> Vec<FilePickerEntry> {
    let mut entries = Vec::with_capacity(files.len() + 1);

    entries.push(FilePickerEntry {
        display_path: PathBuf::from("<description>"),
        view_index: 0,
        comment_count: comment_count_for_view(0),
        reviewed: is_reviewed(0),
    });

    for (diff_file_index, file) in files.iter().enumerate() {
        let display_path = file.display_path().to_owned();
        let view_index = diff_file_index + 1;
        entries.push(FilePickerEntry {
            display_path,
            view_index,
            comment_count: comment_count_for_view(view_index),
            reviewed: is_reviewed(view_index),
        });
    }

    entries
}

pub fn render(frame: &mut Frame<'_>, state: &FilePickerState) {
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
        let indicator_part = reviewed_indicator_text(entry.reviewed);
        let count_part = if entry.comment_count > 0 {
            format!("  [{}]", entry.comment_count)
        } else {
            String::new()
        };
        let cursor_chars = cursor.chars().count();
        let indicator_chars = indicator_part.chars().count();
        let count_chars = count_part.chars().count();
        let path_budget = inner_width
            .saturating_sub(cursor_chars)
            .saturating_sub(indicator_chars)
            .saturating_sub(count_chars);
        let raw_path = entry.display_path.display().to_string();
        let path = truncate_path_for_width(&raw_path, path_budget);

        let base_style = if idx == state.selected_index {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let indicator_style = if entry.reviewed {
            base_style.fg(Color::DarkGray)
        } else {
            base_style
        };
        lines.push(TuiLine::from(vec![
            Span::styled(cursor.to_owned(), base_style),
            Span::styled(indicator_part, indicator_style),
            Span::styled(format!("{path}{count_part}"), base_style),
        ]));
    }

    let widget = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(widget, text_area);
    render_view_scrollbar(frame, sb_state.as_mut(), scrollbar_area);

    let footer_widget = Paragraph::new(FILE_PICKER_FOOTER);
    frame.render_widget(footer_widget, footer_area);
}

/// `✓ ` when reviewed, two spaces when not. Both variants are exactly two
/// columns wide so the path column stays aligned across rows regardless of
/// reviewed state.
pub fn reviewed_indicator_text(reviewed: bool) -> String {
    if reviewed {
        "\u{2713} ".to_owned()
    } else {
        "  ".to_owned()
    }
}

/// Move cursor up or down, clamping to valid range.
pub fn move_cursor(state: &mut FilePickerState, delta: isize) {
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
/// preserving the rightmost characters. When the path already fits, returned
/// unchanged. `available_width` of 0 returns empty string; 1 returns just `…`.
pub fn truncate_path_for_width(path: &str, available_width: usize) -> String {
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

    #[test]
    fn build_entries_empty_comments_all_zero() {
        let files = diff_files();
        let entries = build_entries(&files, &|_| 0, &|_| false);
        assert!(entries.iter().all(|e| e.comment_count == 0));
    }

    #[test]
    fn build_entries_comment_counts_from_closure() {
        let files = diff_files();
        // view 0 = description (2 comments), view 1 = foo.rs (3), view 2 = bar.rs (1)
        let entries = build_entries(&files, &|v| [2usize, 3, 1][v], &|_| false);
        assert_eq!(entries[0].comment_count, 2);
        assert_eq!(entries[1].comment_count, 3);
        assert_eq!(entries[2].comment_count, 1);
    }

    #[test]
    fn build_entries_reviewed_flag_propagates_per_view_index() {
        let files = diff_files();
        let entries = build_entries(&files, &|_| 0, &|view_idx| view_idx == 1);
        assert!(!entries[0].reviewed, "description must be unreviewed");
        assert!(entries[1].reviewed, "foo.rs must be reviewed");
        assert!(!entries[2].reviewed, "bar.rs must be unreviewed");
    }

    #[test]
    fn build_entries_view_indices_start_at_zero_for_description() {
        let files = diff_files();
        let entries = build_entries(&files, &|_| 0, &|_| false);
        assert_eq!(entries[0].view_index, 0);
        for (i, entry) in entries[1..].iter().enumerate() {
            assert_eq!(entry.view_index, i + 1);
        }
    }

    #[test]
    fn reviewed_indicator_text_uses_check_when_reviewed() {
        assert_eq!(reviewed_indicator_text(true), "\u{2713} ");
    }

    #[test]
    fn reviewed_indicator_text_uses_blank_when_unreviewed() {
        assert_eq!(reviewed_indicator_text(false), "  ");
    }

    #[test]
    fn reviewed_indicator_text_widths_match_across_states() {
        assert_eq!(
            reviewed_indicator_text(true).chars().count(),
            reviewed_indicator_text(false).chars().count(),
            "indicator widths must match so the path column lines up"
        );
    }

    #[test]
    fn truncate_path_for_width_short_path_returned_unchanged() {
        let path = "src/foo.rs";
        let result = truncate_path_for_width(path, 80);
        assert_eq!(result, path);
    }

    #[test]
    fn truncate_path_for_width_exact_fit_returned_unchanged() {
        let path = "src/foo.rs";
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

    #[test]
    fn move_cursor_down_increments_index() {
        let files = diff_files();
        let mut state = FilePickerState {
            selected_index: 0,
            scroll_offset: 0,
            entries: build_entries(&files, &|_| 0, &|_| false),
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
            entries: build_entries(&files, &|_| 0, &|_| false),
        };
        move_cursor(&mut state, -1);
        assert_eq!(state.selected_index, 0);
    }

    #[test]
    fn move_cursor_down_clamps_at_last() {
        let files = diff_files();
        let entries = build_entries(&files, &|_| 0, &|_| false);
        let entry_count = entries.len();
        let mut state = FilePickerState {
            selected_index: entry_count - 1,
            scroll_offset: 0,
            entries,
        };
        move_cursor(&mut state, 1);
        assert_eq!(state.selected_index, entry_count - 1);
    }

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

    const PICKER_RIGHTMOST_INNER_COL: u16 = 74;

    #[test]
    fn scrollbar_renders_when_entries_overflow_picker_body() {
        let state = make_state_with_entries(30, 0, 0);
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert!(
            col_contains_scrollbar_glyph(&buf, PICKER_RIGHTMOST_INNER_COL),
            "scrollbar glyphs must appear in the rightmost picker body column when entries overflow"
        );
    }

    #[test]
    fn scrollbar_hidden_when_entries_fit_picker_body() {
        let state = make_state_with_entries(5, 0, 0);
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert!(
            !col_contains_scrollbar_glyph(&buf, PICKER_RIGHTMOST_INNER_COL),
            "scrollbar must be hidden when entries fit the body"
        );
    }

    #[test]
    fn scrollbar_thumb_position_reflects_scroll_offset() {
        let state_top = make_state_with_entries(60, 0, 0);
        let buf_top = render_picker_to_buffer(&state_top, 80, 30);
        let thumb_top = scrollbar_thumb_row(&buf_top, PICKER_RIGHTMOST_INNER_COL)
            .expect("thumb glyph must appear when entries overflow");

        let state_bot = make_state_with_entries(60, 59, 50);
        let buf_bot = render_picker_to_buffer(&state_bot, 80, 30);
        let thumb_bot = scrollbar_thumb_row(&buf_bot, PICKER_RIGHTMOST_INNER_COL)
            .expect("thumb glyph must appear when entries overflow");

        assert!(
            thumb_top < thumb_bot,
            "thumb must move down as scroll_offset advances; top={thumb_top}, bottom={thumb_bot}"
        );
    }

    const PICKER_INNER_X: u16 = 5;
    const PICKER_INNER_Y: u16 = 6;
    const INDICATOR_INNER_COL: u16 = 2;
    const PATH_INNER_COL: u16 = 4;

    fn buf_symbol_at(buf: &ratatui::buffer::Buffer, inner_x: u16, inner_y: u16) -> String {
        buf[(PICKER_INNER_X + inner_x, PICKER_INNER_Y + inner_y)]
            .symbol()
            .to_owned()
    }

    #[test]
    fn render_reviewed_row_shows_check_glyph_before_path() {
        let mut state = make_state_with_entries(3, 0, 0);
        state.entries[1].reviewed = true;
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert_eq!(buf_symbol_at(&buf, INDICATOR_INNER_COL, 1), "\u{2713}");
        assert_eq!(
            buf_symbol_at(&buf, INDICATOR_INNER_COL + 1, 1),
            " ",
            "the cell after ✓ must be a space so path stays aligned"
        );
    }

    #[test]
    fn render_unreviewed_row_keeps_indicator_slot_blank() {
        let state = make_state_with_entries(3, 0, 0);
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert_eq!(buf_symbol_at(&buf, INDICATOR_INNER_COL, 1), " ");
        assert_eq!(buf_symbol_at(&buf, INDICATOR_INNER_COL + 1, 1), " ");
    }

    #[test]
    fn render_path_starts_at_same_column_for_both_reviewed_states() {
        let mut state = make_state_with_entries(3, 0, 0);
        state.entries[1].reviewed = true;
        let buf = render_picker_to_buffer(&state, 80, 30);
        assert_eq!(buf_symbol_at(&buf, PATH_INNER_COL, 1), "f");
        assert_eq!(buf_symbol_at(&buf, PATH_INNER_COL, 2), "f");
    }

    #[test]
    fn render_reviewed_indicator_cell_is_dark_gray() {
        let mut state = make_state_with_entries(3, 0, 0);
        state.entries[1].reviewed = true;
        let buf = render_picker_to_buffer(&state, 80, 30);
        let cell = &buf[(PICKER_INNER_X + INDICATOR_INNER_COL, PICKER_INNER_Y + 1)];
        assert_eq!(cell.symbol(), "\u{2713}");
        assert_eq!(
            cell.fg,
            Color::DarkGray,
            "reviewed indicator must be DarkGray, not Green"
        );
    }
}
