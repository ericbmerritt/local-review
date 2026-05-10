//! Utility functions shared across the local-review-core crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

pub(crate) fn format_age_secs(elapsed: u64) -> String {
    if elapsed < 60 {
        "just now".to_owned()
    } else if elapsed < 3_600 {
        let m = elapsed / 60;
        format!("{m} min ago")
    } else if elapsed < 86_400 {
        let h = elapsed / 3_600;
        format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
    } else {
        let d = elapsed / 86_400;
        format!("{d} day{} ago", if d == 1 { "" } else { "s" })
    }
}

/// Format a GitHub ISO 8601 timestamp as a human-readable relative age.
///
/// GitHub's format is always `"YYYY-MM-DDTHH:MM:SSZ"` (UTC, Z suffix).
/// Returns "just now", "N min ago", "N hour(s) ago", or "N day(s) ago".
/// Manual parsing avoids constructing `OffsetDateTime` at call sites in `ggr`,
/// where timestamps arrive as raw `&str` from the GitHub API.
/// Falls back to `strip_controls(iso_ts)` when the timestamp cannot be parsed.
pub fn format_age_from_iso_str(now: SystemTime, iso_ts: &str) -> String {
    let bytes = iso_ts.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return strip_controls(iso_ts);
    }

    let parse_decimal_u64_2 = |s: &[u8]| -> Option<u64> {
        let a = u64::from(char::from(s[0]).to_digit(10)?);
        let b = u64::from(char::from(s[1]).to_digit(10)?);
        Some(a * 10 + b)
    };
    let parse_decimal_u64_4 = |s: &[u8]| -> Option<u64> {
        let a = u64::from(char::from(s[0]).to_digit(10)?);
        let b = u64::from(char::from(s[1]).to_digit(10)?);
        let c = u64::from(char::from(s[2]).to_digit(10)?);
        let d = u64::from(char::from(s[3]).to_digit(10)?);
        Some(a * 1_000 + b * 100 + c * 10 + d)
    };

    let Some(year) = parse_decimal_u64_4(&bytes[0..4]) else {
        return strip_controls(iso_ts);
    };
    let Some(month) = parse_decimal_u64_2(&bytes[5..7]) else {
        return strip_controls(iso_ts);
    };
    let Some(day) = parse_decimal_u64_2(&bytes[8..10]) else {
        return strip_controls(iso_ts);
    };
    let Some(hour) = parse_decimal_u64_2(&bytes[11..13]) else {
        return strip_controls(iso_ts);
    };
    let Some(min) = parse_decimal_u64_2(&bytes[14..16]) else {
        return strip_controls(iso_ts);
    };
    let Some(sec) = parse_decimal_u64_2(&bytes[17..19]) else {
        return strip_controls(iso_ts);
    };

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return strip_controls(iso_ts);
    }

    if hour > 23 || min > 59 || sec > 59 {
        return strip_controls(iso_ts);
    }

    if year < 1970 {
        return strip_controls(iso_ts);
    }

    let is_leap = |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);

    let years_since_epoch = year.saturating_sub(1970);
    let leap_count = |y: u64| -> u64 {
        let y = y.saturating_sub(1);
        y / 4 - y / 100 + y / 400
    };
    let leaps_before_year = leap_count(year).saturating_sub(leap_count(1970));
    let days_before_year = years_since_epoch * 365 + leaps_before_year;

    let days_per_month: [u64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let Some(month_idx) = usize::try_from(month).ok() else {
        return strip_controls(iso_ts);
    };
    let max_day = days_per_month[month_idx] + u64::from(is_leap(year) && month == 2);
    if day > max_day {
        return strip_controls(iso_ts);
    }
    let leap_extra = u64::from(is_leap(year) && month > 2);
    let days_before_month: u64 = days_per_month[..month_idx].iter().sum::<u64>() + leap_extra;

    let total_days = days_before_year + days_before_month + day.saturating_sub(1);
    let ts_secs = total_days * 86_400 + hour * 3_600 + min * 60 + sec;

    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    let elapsed = now_secs.saturating_sub(ts_secs);

    format_age_secs(elapsed)
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

    // ── format_age tests ──────────────────────────────────────────────────────

    fn epoch_plus(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn format_age_just_now_for_30_seconds_ago() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(100_030);
        assert_eq!(format_age_from_iso_str(now, ts), "just now");
    }

    #[test]
    fn format_age_minutes_ago() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(100_300);
        assert_eq!(format_age_from_iso_str(now, ts), "5 min ago");
    }

    #[test]
    fn format_age_hours_ago() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(110_800);
        assert_eq!(format_age_from_iso_str(now, ts), "3 hours ago");
    }

    #[test]
    fn format_age_one_hour_ago_singular() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(103_600);
        assert_eq!(format_age_from_iso_str(now, ts), "1 hour ago");
    }

    #[test]
    fn format_age_days_ago() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(100_000 + 3 * 86_400);
        assert_eq!(format_age_from_iso_str(now, ts), "3 days ago");
    }

    #[test]
    fn format_age_one_day_ago_singular() {
        let ts = "1970-01-02T03:46:40Z"; // 100_000 secs
        let now = epoch_plus(100_000 + 86_400);
        assert_eq!(format_age_from_iso_str(now, ts), "1 day ago");
    }

    #[test]
    fn format_age_fallback_for_malformed_timestamp() {
        let now = epoch_plus(0);
        let result = format_age_from_iso_str(now, "not-a-timestamp");
        // strip_controls of "not-a-timestamp" is "not-a-timestamp" (no controls)
        assert_eq!(result, "not-a-timestamp");
    }

    #[test]
    fn format_age_strips_control_chars_in_fallback() {
        // A string with control chars but wrong format — falls back to strip_controls.
        let now = epoch_plus(0);
        let input = "\x1b[31mbad-ts\x1b[0m";
        let result = format_age_from_iso_str(now, input);
        assert!(
            !result.chars().any(char::is_control),
            "fallback must strip control chars; got: {result:?}"
        );
    }

    #[test]
    fn format_age_future_timestamp_returns_just_now() {
        assert_eq!(
            format_age_from_iso_str(UNIX_EPOCH, "2024-01-15T10:30:00Z"),
            "just now"
        );
    }

    #[test]
    fn format_age_known_timestamp_2024_01_15() {
        let ts_secs: u64 = 1_705_314_600;
        // 2 days later
        let now = epoch_plus(ts_secs + 2 * 86_400);
        assert_eq!(
            format_age_from_iso_str(now, "2024-01-15T10:30:00Z"),
            "2 days ago"
        );
    }

    #[test]
    fn format_age_leap_year_march_date() {
        let ts_secs: u64 = 1_709_251_200;
        let now = epoch_plus(ts_secs + 86_400);
        assert_eq!(
            format_age_from_iso_str(now, "2024-03-01T00:00:00Z"),
            "1 day ago"
        );
    }

    #[test]
    fn format_age_just_now_upper_boundary() {
        // elapsed=59: last second before "just now" flips to "1 min ago"
        let now = epoch_plus(100_059);
        assert_eq!(
            format_age_from_iso_str(now, "1970-01-02T03:46:40Z"),
            "just now"
        );
    }

    #[test]
    fn format_age_minutes_lower_boundary() {
        // elapsed=60: first second of minutes bucket
        let now = epoch_plus(100_060);
        assert_eq!(
            format_age_from_iso_str(now, "1970-01-02T03:46:40Z"),
            "1 min ago"
        );
    }

    #[test]
    fn format_age_minutes_upper_boundary() {
        // elapsed=3599: last second before "59 min ago" flips to hours
        let now = epoch_plus(103_599);
        assert_eq!(
            format_age_from_iso_str(now, "1970-01-02T03:46:40Z"),
            "59 min ago"
        );
    }

    #[test]
    fn format_age_hours_upper_boundary() {
        // elapsed=86399: last second before "23 hours ago" flips to days
        let now = epoch_plus(186_399);
        assert_eq!(
            format_age_from_iso_str(now, "1970-01-02T03:46:40Z"),
            "23 hours ago"
        );
    }

    #[test]
    fn format_age_century_leap_year_march() {
        let ts_secs: u64 = 951_868_800;
        let now = epoch_plus(ts_secs + 86_400);
        assert_eq!(
            format_age_from_iso_str(now, "2000-03-01T00:00:00Z"),
            "1 day ago"
        );
    }

    #[test]
    fn format_age_pre_epoch_year_falls_back() {
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "1969-12-31T23:59:59Z"),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn format_age_invalid_month_zero_falls_back() {
        // month=00 is out of range; must fall back to strip_controls output
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "1970-00-01T00:00:00Z"),
            "1970-00-01T00:00:00Z"
        );
    }

    #[test]
    fn format_age_invalid_day_zero_falls_back() {
        // day=00 is out of range; must fall back to strip_controls output
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "1970-01-00T00:00:00Z"),
            "1970-01-00T00:00:00Z"
        );
    }

    #[test]
    fn format_age_invalid_day_feb_31_falls_back() {
        let now = epoch_plus(0);
        // Feb has at most 29 days; day=31 must fall back
        assert_eq!(
            format_age_from_iso_str(now, "2024-02-31T00:00:00Z"),
            "2024-02-31T00:00:00Z"
        );
    }

    #[test]
    fn format_age_invalid_day_feb_29_non_leap_falls_back() {
        let now = epoch_plus(0);
        // 2023 is not a leap year; day=29 for Feb must fall back
        assert_eq!(
            format_age_from_iso_str(now, "2023-02-29T00:00:00Z"),
            "2023-02-29T00:00:00Z"
        );
    }

    #[test]
    fn format_age_valid_day_feb_29_leap_year() {
        let ts_secs: u64 = 1_709_164_800;
        let now = epoch_plus(ts_secs + 86_400);
        assert_eq!(
            format_age_from_iso_str(now, "2024-02-29T00:00:00Z"),
            "1 day ago"
        );
    }

    #[test]
    fn format_age_invalid_day_apr_31_falls_back() {
        let now = epoch_plus(0);
        // April has 30 days; day=31 must fall back
        assert_eq!(
            format_age_from_iso_str(now, "2024-04-31T00:00:00Z"),
            "2024-04-31T00:00:00Z"
        );
    }

    #[test]
    fn format_age_invalid_hour_falls_back() {
        // hour=24 is out of range (0..=23); must fall back to strip_controls output
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "2024-01-15T24:00:00Z"),
            "2024-01-15T24:00:00Z"
        );
    }

    #[test]
    fn format_age_invalid_minute_falls_back() {
        // min=60 is out of range (0..=59); must fall back to strip_controls output
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "2024-01-15T00:60:00Z"),
            "2024-01-15T00:60:00Z"
        );
    }

    #[test]
    fn format_age_invalid_second_falls_back() {
        // sec=60 is out of range (0..=59); must fall back to strip_controls output
        let now = epoch_plus(0);
        assert_eq!(
            format_age_from_iso_str(now, "2024-01-15T00:00:60Z"),
            "2024-01-15T00:00:60Z"
        );
    }
}
