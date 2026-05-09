//! Shared layout and navigation helpers.
use crate::error::{GgrError, Result};

/// Locate the git repo root by walking up from the process's current directory.
pub(crate) fn find_git_root() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().map_err(|source| GgrError::Io { source })?;
    find_git_root_from(&cwd)
}

fn find_git_root_from(start: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut current = start;
    loop {
        if current.join(".git").is_dir() {
            return Ok(current.to_owned());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => {
                return Err(GgrError::NotInGitRepo {
                    cwd: start.to_owned(),
                })
            }
        }
    }
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

    #[test]
    fn find_git_root_finds_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let root = find_git_root_from(dir.path()).unwrap();
        assert_eq!(
            std::fs::canonicalize(root).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_git_root_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        let root = find_git_root_from(&sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(root).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_git_root_fails_without_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_git_root_from(dir.path());
        assert!(matches!(result, Err(GgrError::NotInGitRepo { .. })));
    }
}
