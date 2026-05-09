//! Shared layout and navigation helpers.

/// Clamp `value + delta` to `[0, max]`, saturating at each bound.
pub(crate) fn clamp_with_delta(value: usize, delta: isize, max: usize) -> usize {
    let signed_value: isize = isize::try_from(value).unwrap_or(isize::MAX);
    let target = signed_value.saturating_add(delta);
    if target <= 0 {
        0
    } else {
        let unsigned: usize = usize::try_from(target).unwrap_or(0);
        unsigned.min(max)
    }
}

/// Number of lines to scroll for a page movement given the current viewport height.
pub(crate) fn page_size(viewport_rows: u16) -> usize {
    usize::from(viewport_rows.saturating_sub(1)).max(1)
}

/// Truncate `input` to at most `max` characters, appending `…` if truncated.
pub(crate) fn truncate(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let mut result: String = input.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_forward() {
        assert_eq!(clamp_with_delta(3, 2, 10), 5);
    }

    #[test]
    fn clamp_at_max() {
        assert_eq!(clamp_with_delta(8, 5, 10), 10);
    }

    #[test]
    fn clamp_at_zero() {
        assert_eq!(clamp_with_delta(2, -5, 10), 0);
    }

    #[test]
    fn page_size_normal() {
        assert_eq!(page_size(20), 19);
    }

    #[test]
    fn page_size_minimum_one() {
        assert_eq!(page_size(0), 1);
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_adds_ellipsis() {
        let result = truncate("hello world", 8);
        assert_eq!(result, "hello w…");
    }

    #[test]
    fn truncate_max_zero_empty() {
        assert_eq!(truncate("hi", 0), "");
    }
}
