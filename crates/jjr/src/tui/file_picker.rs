//! jjr file-picker overlay.
//!
//! Re-exports pure types and rendering from `local_review_core::tui::file_picker`.
//! Adds a `build_entries` adapter that accepts jjr `Comment` slices and computes
//! the per-view comment counts needed by the core version.

pub(super) use local_review_core::tui::file_picker::{
    move_cursor, render, FilePickerEntry, FilePickerState,
};
#[cfg(test)]
pub(super) use local_review_core::tui::file_picker::{
    reviewed_indicator_text, truncate_path_for_width,
};

use crate::comment::{Anchor, Comment, Status};
use crate::diff::DiffFile;
use std::path::Path;

/// Build file picker entries from diff files and loaded comments.
///
/// Computes the number of active (non-stale, non-orphaned) comments for each
/// view index (0 = description, 1..N = diff file N-1) and delegates to the
/// core's `build_entries` closure-based API.
pub(super) fn build_entries(
    files: &[DiffFile],
    comments: &[Comment],
    is_reviewed: &dyn Fn(usize) -> bool,
) -> Vec<FilePickerEntry> {
    let description_count = count_description_comments(comments);
    let file_paths: Vec<_> = files.iter().map(|f| f.display_path().to_owned()).collect();

    local_review_core::tui::file_picker::build_entries(
        files,
        &|view_idx| {
            if view_idx == 0 {
                description_count
            } else {
                let path = file_paths
                    .get(view_idx - 1)
                    .map(std::path::PathBuf::as_path);
                count_file_comments(comments, path)
            }
        },
        is_reviewed,
    )
}

fn count_description_comments(comments: &[Comment]) -> usize {
    comments
        .iter()
        .filter(|c| {
            if matches!(c.status, Some(Status::Stale | Status::Orphaned)) {
                return false;
            }
            matches!(c.anchor, Anchor::Description { .. })
        })
        .count()
}

fn count_file_comments(comments: &[Comment], file_path: Option<&Path>) -> usize {
    let Some(path) = file_path else {
        return 0;
    };
    comments
        .iter()
        .filter(|c| {
            if matches!(c.status, Some(Status::Stale | Status::Orphaned)) {
                return false;
            }
            let Anchor::Line { location, .. } = &c.anchor else {
                return false;
            };
            location.file.as_path() == path
        })
        .count()
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
        let entries = build_entries(&files, &[], &|view_idx| view_idx == 1);
        assert!(!entries[0].reviewed, "description must be unreviewed");
        assert!(entries[1].reviewed, "foo.rs must be reviewed");
        assert!(!entries[2].reviewed, "bar.rs must be unreviewed");
    }

    #[test]
    fn build_entries_view_indices_start_at_zero_for_description() {
        let files = diff_files();
        let entries = build_entries(&files, &[], &|_| false);
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

    use super::super::scrollbar_test_helpers::{col_contains_scrollbar_glyph, scrollbar_thumb_row};

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

    use ratatui::style::Color;

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

    #[test]
    fn build_entries_comment_count_for_oob_view_index_returns_zero() {
        let files = diff_files();
        let comments = vec![make_comment("foo.rs", Some(Status::Pending))];
        let entries = build_entries(&files, &comments, &|_| false);
        assert!(
            entries.iter().all(|e| e.comment_count <= 1),
            "no panic and counts within expected range even for unusual inputs"
        );
        let count_fn_result = {
            let file_paths: Vec<_> = files.iter().map(|f| f.display_path().to_owned()).collect();
            let path = file_paths.get(999).map(PathBuf::as_path);
            count_file_comments(&comments, path)
        };
        assert_eq!(
            count_fn_result, 0,
            "out-of-bounds view index must return 0 without panicking"
        );
    }

    #[test]
    fn truncate_path_for_width_short_path_returned_unchanged() {
        let path = "src/foo.rs";
        let result = truncate_path_for_width(path, 80);
        assert_eq!(result, path);
    }

    #[test]
    fn truncate_path_for_width_left_truncates_with_ellipsis() {
        let path = "src/some/deep/module/package/thing.rs";
        let result = truncate_path_for_width(path, 20);
        assert_eq!(result.chars().count(), 20);
        assert!(result.starts_with('\u{2026}'));
        assert!(result.ends_with("thing.rs"));
    }
}
