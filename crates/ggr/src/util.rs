//! Shared layout and navigation helpers.
use std::path::{Path, PathBuf};
use std::process::Command;

/// Canonical data directory for a specific PR under `base`.
///
/// Segments that fail [`crate::pr::valid_segment`] fall back to safe
/// placeholders so crafted hostnames or repo slugs cannot escape `base`.
pub(crate) fn pr_data_dir(
    base: &Path,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> PathBuf {
    let host_seg = if crate::pr::valid_segment(host) {
        host
    } else {
        "github.com"
    };
    let owner_seg = if crate::pr::valid_segment(owner) {
        owner
    } else {
        "_invalid"
    };
    let repo_seg = if crate::pr::valid_segment(repo) {
        repo
    } else {
        "_invalid"
    };
    base.join("ggr")
        .join(host_seg)
        .join(owner_seg)
        .join(repo_seg)
        .join(pr_number.to_string())
}

/// Shared by `cursor` and `draft` modules; centralised here so XDG lookup
/// logic is not duplicated across two storage paths.
pub(crate) fn data_home() -> Option<PathBuf> {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("share"))
        })
}

/// Normalise a hostname: return `None` for `github.com` (no override needed),
/// `Some(host.to_owned())` for any other host.
///
/// Used wherever a hostname string must be converted to the `hostname` field of
/// `ParsedPrRef`: github.com is the default and needs no `--hostname` flag.
pub(crate) fn hostname_from_host(host: &str) -> Option<String> {
    if host == "github.com" {
        None
    } else {
        Some(host.to_owned())
    }
}

/// Run `git remote get-url origin` and return `(host, owner/repo)` as owned
/// strings. Returns `None` if git is absent, the remote fails, or the URL
/// can't be parsed.
fn remote_origin_coords() -> Option<(String, String)> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = std::str::from_utf8(&out.stdout).ok()?.trim();
    let (host, slug) = parse_remote_url(url)?;
    Some((host.to_owned(), slug.to_owned()))
}

/// Try to infer the GitHub host from the local git remote.
///
/// Returns the non-github.com hostname when:
/// - `expected_slug` is `Some("owner/repo")` and the remote slug matches, or
/// - `expected_slug` is `None` (bare PR number — use whatever host the remote is on).
///
/// Returns `None` if git is absent, the remote can't be parsed, the slug doesn't
/// match, or the host is `github.com` (no special handling needed there).
pub(crate) fn detect_remote_host(expected_slug: Option<&str>) -> Option<String> {
    let (host, slug) = remote_origin_coords()?;
    if let Some(expected) = expected_slug {
        if !slug.eq_ignore_ascii_case(expected) {
            return None;
        }
    }
    hostname_from_host(&host)
}

/// Detect `(host, owner, repo)` from the local git remote.
///
/// Unlike [`detect_remote_host`], this always returns the full coordinates
/// including `github.com` (not suppressed), making it suitable for locating
/// local storage paths which need the full triple regardless of host.
///
/// Returns `None` if git is absent, the remote fails, or the URL can't be
/// parsed as `owner/repo`.
pub(crate) fn detect_remote_coords() -> Option<(String, String, String)> {
    let (host, slug) = remote_origin_coords()?;
    let (owner, repo) = slug.split_once('/')?;
    Some((host, owner.to_owned(), repo.to_owned()))
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

#[cfg(test)]
mod tests {
    use super::*;

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
