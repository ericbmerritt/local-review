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
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let mut result: String = input.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

/// Append `s` to `word` when `count != 1`. English plurals only; deliberately
/// simple — the only words this serves are short, regular nouns ("comment",
/// "change", "suggestion", "note").
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
    fn clamp_with_delta_moves_forward() {
        assert_eq!(clamp_with_delta(3, 2, 10), 5);
    }

    #[test]
    fn clamp_with_delta_clamps_at_max() {
        assert_eq!(clamp_with_delta(8, 5, 10), 10);
    }

    #[test]
    fn clamp_with_delta_clamps_at_zero() {
        assert_eq!(clamp_with_delta(2, -5, 10), 0);
    }

    #[test]
    fn clamp_with_delta_stays_at_zero() {
        assert_eq!(clamp_with_delta(0, -1, 10), 0);
    }

    #[test]
    fn clamp_with_delta_exact_zero() {
        assert_eq!(clamp_with_delta(0, 0, 10), 0);
    }

    #[test]
    fn page_size_normal() {
        assert_eq!(page_size(20), 19);
    }

    #[test]
    fn page_size_minimum_one() {
        assert_eq!(page_size(0), 1);
        assert_eq!(page_size(1), 1);
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate("hello world", 8);
        assert_eq!(result, "hello w…");
        assert_eq!(result.chars().count(), 8);
    }

    #[test]
    fn truncate_empty_string_unchanged() {
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn truncate_max_zero_produces_ellipsis() {
        let result = truncate("hi", 0);
        assert_eq!(result, "…");
    }

    #[test]
    fn pluralize_count_one_is_singular() {
        assert_eq!(pluralize("note", 1), "note");
        assert_eq!(pluralize("suggestion", 1), "suggestion");
    }

    #[test]
    fn pluralize_count_zero_is_plural() {
        // We only ever call pluralize with count > 0 in practice (we skip the
        // span when the count is zero), but the rule "anything other than 1
        // is plural" is the safer default.
        assert_eq!(pluralize("note", 0), "notes");
    }

    #[test]
    fn pluralize_count_two_is_plural() {
        assert_eq!(pluralize("note", 2), "notes");
        assert_eq!(pluralize("suggestion", 3), "suggestions");
    }
}
