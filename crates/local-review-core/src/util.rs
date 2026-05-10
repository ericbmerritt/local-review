//! Utility functions shared across the local-review-core crate.

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
