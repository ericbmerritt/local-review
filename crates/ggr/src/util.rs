//! Shared layout and navigation helpers.
use std::process::Command;

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
    hostname_from_host(host)
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
