/// Parse a confirmation response from the user.
///
/// Returns `true` for any casing of `y` or `yes`. Anything else — including
/// an empty string — is treated as rejection and returns `false`.
#[must_use]
pub fn confirm_response(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

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
///
/// When `max == 0` returns an empty string — the ellipsis itself would
/// overflow the budget. Callers that want a non-empty indicator at zero
/// budget must allocate at least one column.
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

/// Emit a `warning: <msg>` line to stderr, locked for the duration of the
/// write so concurrent calls do not interleave. Mirrors `store.rs`'s prior
/// in-place helper; centralizing here keeps the wire format ("warning: …")
/// in one place and gives reviewed-state load failures the same surface.
pub(crate) fn log_warning(msg: &str) {
    use std::io::Write as _;
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "warning: {msg}");
}

/// Atomically write `bytes` to `path`, creating `dir` if needed.
///
/// Three call sites (cursor, comment store, reviewed-state) used to inline
/// the same `create_dir_all + tempfile + write_all + flush + persist`
/// sequence; centralizing the idiom here keeps the crash-safety contract in
/// one named place. `dir` must be the parent directory of `path` (passed
/// explicitly so the caller can hold an owned `PathBuf` for both without an
/// extra `parent()` round-trip).
///
/// Crash safety: writes go to a randomized sibling temp file, which `persist`
/// renames into place; same-directory placement keeps the rename on a single
/// filesystem so the OS can guarantee atomicity.
pub(crate) fn atomic_write_bytes(
    dir: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> crate::error::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir).map_err(|source| crate::error::JjrError::Io { source })?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.write_all(bytes)
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.flush()
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.persist(path).map_err(|e| crate::error::JjrError::Io {
        source: std::io::Error::other(e),
    })?;
    Ok(())
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
    fn truncate_max_zero_returns_empty() {
        // At max==0 even the `…` indicator would overflow the budget; the
        // overview's column-fitting depends on this precise behavior.
        assert_eq!(truncate("hi", 0), "");
        assert_eq!(truncate("", 0), "");
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

    #[test]
    fn confirm_response_accepts_y() {
        assert!(confirm_response("y"));
        assert!(confirm_response("Y"));
    }

    #[test]
    fn confirm_response_accepts_yes() {
        assert!(confirm_response("yes"));
        assert!(confirm_response("YES"));
        assert!(confirm_response("Yes"));
    }

    #[test]
    fn confirm_response_rejects_empty() {
        assert!(!confirm_response(""));
    }

    #[test]
    fn confirm_response_rejects_no() {
        assert!(!confirm_response("n"));
        assert!(!confirm_response("no"));
    }

    #[test]
    fn confirm_response_rejects_anything_else() {
        assert!(!confirm_response("nope"));
        assert!(!confirm_response("sure"));
        assert!(!confirm_response("1"));
    }

    #[test]
    fn atomic_write_bytes_writes_payload_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("subdir");
        let target = nested.join("out.txt");
        atomic_write_bytes(&nested, &target, b"hello").unwrap();
        let read = std::fs::read(&target).unwrap();
        assert_eq!(read, b"hello");
    }

    #[test]
    fn atomic_write_bytes_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        std::fs::write(&target, b"old").unwrap();
        atomic_write_bytes(dir.path(), &target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_bytes_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        atomic_write_bytes(dir.path(), &target, b"x").unwrap();
        let extras: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(extras.is_empty(), "stray files: {extras:?}");
    }

    #[test]
    fn confirm_response_trims_surrounding_whitespace() {
        assert!(confirm_response("  y  "));
        assert!(confirm_response("  yes\n"));
        assert!(!confirm_response("  n  "));
    }
}
