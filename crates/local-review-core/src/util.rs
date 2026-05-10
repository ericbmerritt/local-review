//! Utility functions shared across the local-review-core crate.

/// Strip ASCII/Unicode control characters from a string.
///
/// Used when embedding untrusted input (e.g. diff hunk text from a GitHub PR)
/// into terminal output to prevent ANSI/OSC escape injection.
pub fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Strip ANSI/OSC injection control characters from a string, preserving `\t`,
/// `\n`, and `\r`.
///
/// Diff line text may contain tab indentation (Go, Makefile, Python, Rust).
/// Tab is not an ANSI/OSC injection vector — the threat is ESC (U+001B) and
/// the C1 block (U+0080–U+009F). Stripping tabs from diff text corrupts
/// displayed indentation; this function preserves them.
/// `\r` and `\n` are preserved because ratatui renders text through a buffered
/// widget system; control bytes inside a `Span` are not relayed raw to the
/// terminal.
///
/// Use this function for diff display text. Use [`strip_controls`] for error
/// message sanitization where stripping all control characters is correct.
pub fn strip_injection_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// Strip ASCII/Unicode control characters from a string, preserving `\n`.
///
/// Multi-line PR fields (body, comment bodies) must retain newlines so the
/// description page renders with correct line breaks. Bare `\r` (carriage
/// return not part of a `\r\n` sequence) is stripped: `str::lines()` handles
/// `\r\n` pairs by discarding the `\r`, so preserving bare `\r` would only
/// allow it to reach the terminal as U+000D, overwriting the current line.
pub fn strip_controls_preserve_newlines(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

/// Clamp `index + delta` into `[0, max]`.
pub fn clamp_with_delta(index: usize, delta: isize, max: usize) -> usize {
    if delta >= 0 {
        let d = usize::try_from(delta).unwrap_or(usize::MAX);
        index.saturating_add(d).min(max)
    } else {
        let d = delta.unsigned_abs();
        index.saturating_sub(d)
    }
}

/// Compute a sensible page size from the viewport height.
pub fn page_size(viewport_rows: u16) -> usize {
    usize::from(viewport_rows.saturating_sub(1)).max(1)
}

/// Truncate `s` to at most `max` Unicode scalar values, appending `…` when truncated.
///
/// When `max == 0` returns an empty string — the ellipsis itself would
/// overflow the budget. Callers that want a non-empty indicator at zero
/// budget must allocate at least one column.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}\u{2026}")
}

/// Plural form helper: returns `"word"` or `"words"` depending on `count`.
pub fn pluralize(word: &str, count: usize) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_controls_empty_string() {
        assert_eq!(strip_controls(""), "");
    }

    #[test]
    fn strip_controls_ascii_only_unchanged() {
        assert_eq!(strip_controls("hello world"), "hello world");
    }

    #[test]
    fn strip_controls_removes_embedded_esc_sequence() {
        let result = strip_controls("\x1b[31mred\x1b[0m");
        assert_eq!(result, "[31mred[0m");
    }

    #[test]
    fn strip_controls_removes_newlines() {
        let result = strip_controls("line1\nline2");
        assert_eq!(result, "line1line2");
    }

    #[test]
    fn strip_controls_removes_unicode_control_characters() {
        // U+0085 NEXT LINE is a Unicode control character (C1 block).
        let result = strip_controls("before\u{0085}after");
        assert_eq!(result, "beforeafter");
    }

    #[test]
    fn strip_injection_controls_preserves_tab() {
        assert_eq!(strip_injection_controls("col1\tcol2"), "col1\tcol2");
    }

    #[test]
    fn strip_injection_controls_strips_esc() {
        let result = strip_injection_controls("\x1b[31mred\x1b[0m");
        assert_eq!(result, "[31mred[0m");
    }

    #[test]
    fn strip_injection_controls_strips_other_c0_controls() {
        // BEL (U+0007), BS (U+0008), VT (U+000B) are stripped.
        let result = strip_injection_controls("\x07\x08\x0b");
        assert_eq!(result, "");
    }

    #[test]
    fn strip_injection_controls_preserves_newline_and_cr() {
        assert_eq!(strip_injection_controls("a\nb\rc"), "a\nb\rc");
    }

    #[test]
    fn strip_controls_preserve_newlines_strips_carriage_returns() {
        // Bare \r reaches the terminal as U+000D (carriage return), overwriting
        // the current line. str::lines() already strips \r from \r\n pairs, so
        // preserving \r in the output serves no purpose and is harmful.
        let result = strip_controls_preserve_newlines("line\r\n");
        assert!(
            !result.contains('\r'),
            "strip_controls_preserve_newlines must strip \\r; got: {result:?}"
        );
        assert!(
            result.contains('\n'),
            "strip_controls_preserve_newlines must preserve \\n; got: {result:?}"
        );
    }

    #[test]
    fn strip_controls_preserve_newlines_strips_tabs() {
        let result = strip_controls_preserve_newlines("col1\tcol2");
        assert!(
            !result.contains('\t'),
            "strip_controls_preserve_newlines strips tabs; got: {result:?}"
        );
    }

    #[test]
    fn strip_controls_preserve_newlines_empty_string() {
        let result = strip_controls_preserve_newlines("");
        assert_eq!(result, "", "empty input must produce empty output");
    }

    #[test]
    fn strip_controls_preserve_newlines_only_non_newline_controls_removed() {
        let result = strip_controls_preserve_newlines("\x01\x02\x03");
        assert_eq!(
            result, "",
            "non-newline control characters must all be stripped"
        );
    }

    #[test]
    fn strip_controls_preserve_newlines_bare_cr_stripped() {
        let result = strip_controls_preserve_newlines("bare\rcarriage");
        assert_eq!(result, "barecarriage");
    }

    #[test]
    fn clamp_with_delta_zero_delta() {
        assert_eq!(clamp_with_delta(5, 0, 10), 5);
    }

    #[test]
    fn clamp_with_delta_positive_clamped_at_max() {
        assert_eq!(clamp_with_delta(8, 5, 10), 10);
    }

    #[test]
    fn clamp_with_delta_negative_clamped_at_zero() {
        assert_eq!(clamp_with_delta(2, -5, 10), 0);
    }

    #[test]
    fn clamp_with_delta_positive_within_bounds() {
        assert_eq!(clamp_with_delta(3, 2, 10), 5);
    }

    #[test]
    fn clamp_with_delta_exactly_at_max() {
        assert_eq!(clamp_with_delta(10, 0, 10), 10);
    }

    #[test]
    fn clamp_with_delta_exactly_at_zero() {
        assert_eq!(clamp_with_delta(0, 0, 10), 0);
    }

    #[test]
    fn clamp_with_delta_at_zero_negative() {
        assert_eq!(clamp_with_delta(0, -1, 10), 0);
    }

    #[test]
    fn clamp_with_delta_at_max_positive() {
        assert_eq!(clamp_with_delta(10, 1, 10), 10);
    }
}
