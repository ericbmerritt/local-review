//! Shared layout and navigation helpers.
use std::process::Command;

/// Try to infer the GitHub host from the local git remote.
///
/// Runs `git remote get-url origin`, parses SSH (`git@HOST:OWNER/REPO.git`) or
/// HTTPS (`https://HOST/OWNER/REPO`) format, and returns the host when:
/// - `expected_slug` is `Some("owner/repo")` and the remote slug matches, or
/// - `expected_slug` is `None` (bare PR number — use whatever host the remote is on).
///
/// Returns `None` if git is absent, the remote can't be parsed, the slug doesn't
/// match, or the host is `github.com` (no special handling needed there).
pub(crate) fn detect_remote_host(expected_slug: Option<&str>) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = std::str::from_utf8(&out.stdout).ok()?.trim();
    let (host, slug) = parse_remote_url(url)?;
    if let Some(expected) = expected_slug {
        if !slug.eq_ignore_ascii_case(expected) {
            return None;
        }
    }
    if host == "github.com" {
        None
    } else {
        Some(host.to_owned())
    }
}

/// Parse `(host, owner/repo)` from an SSH or HTTPS git remote URL.
fn parse_remote_url(url: &str) -> Option<(&str, &str)> {
    // SSH: git@HOST:OWNER/REPO.git
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some((host, path.trim_end_matches(".git")));
    }
    // HTTPS: https://HOST/OWNER/REPO[.git]
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    Some((host, path.trim_end_matches(".git")))
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
    fn parse_remote_ssh() {
        let (host, slug) = parse_remote_url("git@github.example.com:acme/myrepo.git").unwrap();
        assert_eq!(host, "github.example.com");
        assert_eq!(slug, "acme/myrepo");
    }

    #[test]
    fn parse_remote_https() {
        let (host, slug) = parse_remote_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(slug, "owner/repo");
    }

    #[test]
    fn parse_remote_https_no_dot_git() {
        let (host, slug) = parse_remote_url("https://github.example.com/owner/repo").unwrap();
        assert_eq!(host, "github.example.com");
        assert_eq!(slug, "owner/repo");
    }

    #[test]
    fn parse_remote_invalid_returns_none() {
        assert!(parse_remote_url("not-a-url").is_none());
    }
}
