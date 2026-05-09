//! Minimal multi-line text input widget for the comment composer.
//!
//! Replaces the `tui-textarea` crate with a small bespoke widget covering the
//! exact subset of behavior the composer relies on: printable insertion,
//! Enter / Backspace / Delete / arrow / Home / End / Tab dispatch, and a
//! Widget render that reverse-videos the cursor cell. No undo/redo,
//! selection, search, viewport scroll, or word-boundary jumps — those are
//! out of scope.
//!
//! Cursor column uses `chars().count()` not display width — wide chars
//! (CJK, emoji) misalign visually.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

/// Multi-line text buffer with a single cursor.
///
/// Buffer invariants (preserved by every public method):
/// - `lines` is never empty (an empty buffer holds one empty `String`).
/// - `cursor.0 < lines.len()`; `cursor.1 <= lines[cursor.0].chars().count()`.
#[derive(Debug, Clone)]
pub struct TextArea {
    lines: Vec<String>,
    cursor: (usize, usize),
}

impl Default for TextArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }
}

impl TextArea {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Insert `s` at the cursor; `\n` splits the current line.
    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            if ch == '\n' {
                self.insert_newline();
            } else {
                self.insert_char(ch);
            }
        }
    }

    /// Split the current line at the cursor, advancing to (row+1, 0).
    pub fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let split_byte = char_offset_to_byte(&self.lines[row], col);
        let tail = self.lines[row].split_off(split_byte);
        self.lines.insert(row + 1, tail);
        self.cursor = (row + 1, 0);
    }

    /// Dispatch a `KeyEvent`; return whether the buffer was modified.
    ///
    /// Cursor movement that doesn't change buffer contents returns `false`,
    /// matching `tui-textarea`'s contract.
    pub fn input(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Char(c), m) if is_printable_modifier(m) => {
                self.insert_char(c);
                true
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.insert_newline();
                true
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.insert_char('\t');
                true
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => self.delete_before_cursor(),
            (KeyCode::Delete, KeyModifiers::NONE) => self.delete_at_cursor(),
            (KeyCode::Left, KeyModifiers::NONE) => {
                self.move_left();
                false
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                self.move_right();
                false
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.move_up();
                false
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.move_down();
                false
            }
            (KeyCode::Home, KeyModifiers::NONE) => {
                self.cursor.1 = 0;
                false
            }
            (KeyCode::End, KeyModifiers::NONE) => {
                self.cursor.1 = self.lines[self.cursor.0].chars().count();
                false
            }
            _ => false,
        }
    }

    fn insert_char(&mut self, ch: char) {
        let (row, col) = self.cursor;
        let byte = char_offset_to_byte(&self.lines[row], col);
        self.lines[row].insert(byte, ch);
        self.cursor.1 = col + 1;
    }

    fn delete_before_cursor(&mut self) -> bool {
        let (row, col) = self.cursor;
        if col > 0 {
            let line = &mut self.lines[row];
            let prev_byte = char_offset_to_byte(line, col - 1);
            let cur_byte = char_offset_to_byte(line, col);
            line.replace_range(prev_byte..cur_byte, "");
            self.cursor.1 = col - 1;
            return true;
        }
        if row > 0 {
            let merged = self.lines.remove(row);
            let prev_len = self.lines[row - 1].chars().count();
            self.lines[row - 1].push_str(&merged);
            self.cursor = (row - 1, prev_len);
            return true;
        }
        false
    }

    fn delete_at_cursor(&mut self) -> bool {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].chars().count();
        if col < line_len {
            let line = &mut self.lines[row];
            let cur_byte = char_offset_to_byte(line, col);
            let next_byte = char_offset_to_byte(line, col + 1);
            line.replace_range(cur_byte..next_byte, "");
            return true;
        }
        if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
            return true;
        }
        false
    }

    fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.cursor.1 = col - 1;
        } else if row > 0 {
            let prev_len = self.lines[row - 1].chars().count();
            self.cursor = (row - 1, prev_len);
        }
    }

    fn move_right(&mut self) {
        let (row, col) = self.cursor;
        let line_len = self.lines[row].chars().count();
        if col < line_len {
            self.cursor.1 = col + 1;
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
    }

    fn move_up(&mut self) {
        let (row, col) = self.cursor;
        if row > 0 {
            let prev_len = self.lines[row - 1].chars().count();
            self.cursor = (row - 1, col.min(prev_len));
        }
    }

    fn move_down(&mut self) {
        let (row, col) = self.cursor;
        if row + 1 < self.lines.len() {
            let next_len = self.lines[row + 1].chars().count();
            self.cursor = (row + 1, col.min(next_len));
        }
    }

    /// Clamp-and-store cursor setter for test code only.
    #[cfg(test)]
    pub fn set_cursor_for_test(&mut self, row: usize, col: usize) {
        let row = row.min(self.lines.len().saturating_sub(1));
        let line_len = self.lines[row].chars().count();
        self.cursor = (row, col.min(line_len));
    }
}

/// Capital letters and shifted symbols arrive with `SHIFT` held; Ctrl/Alt/
/// Super/Meta are not printable insertions.
fn is_printable_modifier(m: KeyModifiers) -> bool {
    m.is_empty() || m == KeyModifiers::SHIFT
}

/// Translate a char offset within `line` to a byte index. `offset` may equal
/// `line.chars().count()` (one-past-the-end) and resolves to `line.len()`.
fn char_offset_to_byte(line: &str, offset: usize) -> usize {
    line.char_indices()
        .nth(offset)
        .map_or_else(|| line.len(), |(b, _)| b)
}

impl Widget for &TextArea {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let reverse = Style::default().add_modifier(Modifier::REVERSED);
        for (row_idx, line) in self.lines.iter().enumerate().take(usize::from(area.height)) {
            let y = area.y.saturating_add(u16_from_usize(row_idx));
            let cursor_col = (row_idx == self.cursor.0).then_some(self.cursor.1);
            let mut line_chars = 0usize;
            for (col_idx, ch) in line.chars().enumerate() {
                if col_idx >= usize::from(area.width) {
                    line_chars = col_idx;
                    break;
                }
                let x = area.x.saturating_add(u16_from_usize(col_idx));
                let cell = &mut buf[(x, y)];
                cell.set_symbol(&ch.to_string());
                if cursor_col == Some(col_idx) {
                    cell.set_style(reverse);
                }
                line_chars = col_idx + 1;
            }
            // Cursor sitting past the last char (one-past-the-end position)
            // gets a blank reversed cell so it remains visible.
            if let Some(col) = cursor_col {
                if col >= line_chars && line_chars < usize::from(area.width) {
                    let x = area.x.saturating_add(u16_from_usize(line_chars));
                    let cell = &mut buf[(x, y)];
                    cell.set_symbol(" ");
                    cell.set_style(reverse);
                }
            }
        }
    }
}

/// The surrounding `Rect` already bounds inputs to `u16` in practice.
fn u16_from_usize(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn insert_str_with_embedded_newline_splits_into_two_lines() {
        let mut ta = TextArea::default();
        ta.insert_str("a\nb");
        assert_eq!(ta.lines(), &["a".to_owned(), "b".to_owned()]);
        assert_eq!(ta.cursor, (1, 1));
    }

    #[test]
    fn insert_str_with_embedded_empty_line_round_trips() {
        let mut ta = TextArea::default();
        ta.insert_str("a\n\nb");
        assert_eq!(ta.lines(), &["a".to_owned(), String::new(), "b".to_owned()]);
        assert_eq!(ta.cursor, (2, 1));

        let mut ta2 = TextArea::default();
        ta2.insert_str("\n");
        assert_eq!(ta2.lines(), &[String::new(), String::new()]);
        assert_eq!(ta2.cursor, (1, 0));
    }

    #[test]
    fn insert_newline_mid_line_splits_at_cursor() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.set_cursor_for_test(0, 1);
        ta.insert_newline();
        assert_eq!(ta.lines(), &["a".to_owned(), "bc".to_owned()]);
        assert_eq!(ta.cursor, (1, 0));
    }

    #[test]
    fn backspace_in_middle_removes_prior_char() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.set_cursor_for_test(0, 1);
        let modified = ta.input(key(KeyCode::Backspace));
        assert!(modified);
        assert_eq!(ta.lines(), &["bc".to_owned()]);
        assert_eq!(ta.cursor, (0, 0));
    }

    #[test]
    fn backspace_at_col_zero_merges() {
        let mut ta = TextArea::default();
        ta.insert_str("a\nbc");
        ta.set_cursor_for_test(1, 0);
        let modified = ta.input(key(KeyCode::Backspace));
        assert!(modified);
        assert_eq!(ta.lines(), &["abc".to_owned()]);
        assert_eq!(ta.cursor, (0, 1));
    }

    #[test]
    fn backspace_at_origin_is_noop() {
        let mut ta = TextArea::default();
        let modified = ta.input(key(KeyCode::Backspace));
        assert!(!modified);
        assert_eq!(ta.lines(), &[String::new()]);
        assert_eq!(ta.cursor, (0, 0));
    }

    #[test]
    fn delete_in_middle_removes_char_at_cursor() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.set_cursor_for_test(0, 1);
        let modified = ta.input(key(KeyCode::Delete));
        assert!(modified);
        assert_eq!(ta.lines(), &["ac".to_owned()]);
        assert_eq!(ta.cursor, (0, 1));
    }

    #[test]
    fn delete_at_eol_merges_with_next() {
        let mut ta = TextArea::default();
        ta.insert_str("a\nb");
        ta.set_cursor_for_test(0, 1);
        let modified = ta.input(key(KeyCode::Delete));
        assert!(modified);
        assert_eq!(ta.lines(), &["ab".to_owned()]);
        assert_eq!(ta.cursor, (0, 1));
    }

    #[test]
    fn delete_at_buffer_origin_is_noop() {
        let mut ta = TextArea::default();
        let modified = ta.input(key(KeyCode::Delete));
        assert!(!modified);
        assert_eq!(ta.lines(), &[String::new()]);
        assert_eq!(ta.cursor, (0, 0));
    }

    #[test]
    fn left_at_col_zero_wraps_to_eol_above() {
        let mut ta = TextArea::default();
        ta.insert_str("ab\ncd");
        ta.set_cursor_for_test(1, 0);
        ta.input(key(KeyCode::Left));
        assert_eq!(ta.cursor, (0, 2));
    }

    #[test]
    fn right_at_eol_wraps_to_col_zero_below() {
        let mut ta = TextArea::default();
        ta.insert_str("ab\ncd");
        ta.set_cursor_for_test(0, 2);
        ta.input(key(KeyCode::Right));
        assert_eq!(ta.cursor, (1, 0));
    }

    #[test]
    fn up_clamps_to_shorter_prior_line() {
        let mut ta = TextArea::default();
        ta.insert_str("ab\ncdef");
        ta.set_cursor_for_test(1, 4);
        ta.input(key(KeyCode::Up));
        assert_eq!(ta.cursor, (0, 2));
    }

    #[test]
    fn down_clamps_to_shorter_next_line() {
        let mut ta = TextArea::default();
        ta.insert_str("abcd\nef");
        ta.set_cursor_for_test(0, 4);
        ta.input(key(KeyCode::Down));
        assert_eq!(ta.cursor, (1, 2));
    }

    #[test]
    fn up_at_first_row_is_noop() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.input(key(KeyCode::Up));
        assert_eq!(ta.cursor, (0, 3));
        assert_eq!(ta.lines(), &["abc".to_owned()]);
    }

    #[test]
    fn down_at_last_row_is_noop() {
        let mut ta = TextArea::default();
        ta.insert_str("a\nb");
        ta.input(key(KeyCode::Down));
        assert_eq!(ta.cursor, (1, 1));
    }

    #[test]
    fn home_and_end_jump_to_line_extremes() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.input(key(KeyCode::Home));
        assert_eq!(ta.cursor, (0, 0));
        ta.input(key(KeyCode::End));
        assert_eq!(ta.cursor, (0, 3));
    }

    #[test]
    fn tab_inserts_literal_tab() {
        let mut ta = TextArea::default();
        let modified = ta.input(key(KeyCode::Tab));
        assert!(modified);
        assert_eq!(ta.lines(), &["\t".to_owned()]);
        assert_eq!(ta.cursor, (0, 1));
    }

    #[test]
    fn ctrl_modified_char_is_noop() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        let before = ta.lines().to_owned();
        let cursor_before = ta.cursor;
        let modified = ta.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(!modified);
        assert_eq!(ta.lines(), before.as_slice());
        assert_eq!(ta.cursor, cursor_before);
    }

    #[test]
    fn shift_printable_inserts_uppercase() {
        let mut ta = TextArea::default();
        let modified = ta.input(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert!(modified);
        assert_eq!(ta.lines(), &["A".to_owned()]);
    }

    #[test]
    fn non_ascii_inserts_advance_cursor_one_per_char() {
        let mut ta = TextArea::default();
        ta.insert_str("é");
        assert_eq!(ta.cursor, (0, 1));
        ta.insert_str("中");
        assert_eq!(ta.cursor, (0, 2));
        ta.insert_str("🎉");
        assert_eq!(ta.cursor, (0, 3));
        assert_eq!(ta.lines(), &["é中🎉".to_owned()]);

        let mut ta2 = TextArea::default();
        ta2.insert_str("é");
        let modified = ta2.input(key(KeyCode::Backspace));
        assert!(modified);
        assert_eq!(ta2.lines(), &[String::new()]);
        assert_eq!(ta2.cursor, (0, 0));
    }

    #[test]
    fn out_of_range_cursor_is_clamped_and_backspace_is_correct() {
        let mut ta = TextArea::default();
        ta.insert_str("abc");
        ta.set_cursor_for_test(0, 99);
        assert_eq!(ta.cursor, (0, 3));
        let modified = ta.input(key(KeyCode::Backspace));
        assert!(modified);
        assert_eq!(ta.lines(), &["ab".to_owned()]);
        assert_eq!(ta.cursor, (0, 2));
    }

    #[test]
    fn render_reverse_videos_cursor_cell() {
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut ta = TextArea::default();
        ta.insert_str("hi");
        ta.set_cursor_for_test(0, 1);
        term.draw(|f| {
            let area = f.area();
            f.render_widget(&ta, area);
        })
        .expect("draw");
        let buf = term.backend().buffer();
        assert!(buf[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn render_places_each_line_on_its_own_row() {
        let backend = TestBackend::new(10, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut ta = TextArea::default();
        ta.insert_str("aa\nbb\ncc");
        ta.input(key(KeyCode::Enter));
        assert_eq!(ta.cursor, (3, 0));
        term.draw(|f| {
            let area = f.area();
            f.render_widget(&ta, area);
        })
        .expect("draw");
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "a");
        assert_eq!(buf[(0, 1)].symbol(), "b");
        assert_eq!(buf[(1, 1)].symbol(), "b");
        assert_eq!(buf[(0, 2)].symbol(), "c");
        assert_eq!(buf[(1, 2)].symbol(), "c");
    }

    #[test]
    fn render_cursor_at_eol_paints_blank_reverse_cell() {
        let backend = TestBackend::new(10, 1);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut ta = TextArea::default();
        ta.insert_str("ab");
        assert_eq!(ta.cursor, (0, 2));
        term.draw(|f| {
            let area = f.area();
            f.render_widget(&ta, area);
        })
        .expect("draw");
        let buf = term.backend().buffer();
        let cell = &buf[(2, 0)];
        assert_eq!(cell.symbol(), " ");
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }
}
