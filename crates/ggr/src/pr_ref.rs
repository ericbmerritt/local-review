//! PR reference parsing — four input forms, one resolved struct.
//!
//! | Form                                          | Example                                                          |
//! |-----------------------------------------------|------------------------------------------------------------------|
//! | Bare number                                   | `42`                                                             |
//! | `owner/repo#number`                           | `acme/myrepo#2429`                                  |
//! | `--url <host>` + `owner/repo#number`          | `--url https://github.example.com acme/myrepo#2429`   |
//! | Full GitHub pull URL                          | `https://github.example.com/acme/myrepo/pull/2429`    |
//!
//! `ParsedPrRef` carries enough information to build any `gh` invocation.

use crate::error::{GgrError, Result};
use crate::pr::RepoName;

/// Resolved PR reference, ready to drive `gh pr view` and `gh api`.
#[derive(Debug, Clone)]
pub(crate) struct ParsedPrRef {
    /// PR number.
    pub(crate) number: u64,
    /// Value for `--repo` (may carry a `HOST/` prefix for GHE repos).
    /// `None` → let `gh` auto-detect from the current git remote.
    pub(crate) repo_flag: Option<String>,
    /// GHE hostname for `gh api --hostname`. `None` → github.com.
    pub(crate) hostname: Option<String>,
}

/// Parse a PR reference string, with an optional `--url <base>` flag.
///
/// Accepted forms:
/// - `"42"` or `"#42"` — bare PR number
/// - `"owner/repo#42"` — explicit repo + number (any directory)
/// - `"owner/repo#42"` with `url = Some("https://github.example.com")` — GHE host + short form
/// - `"https://host/owner/repo/pull/42"` — full pull URL (paste from browser)
pub(crate) fn parse(input: &str, url_flag: Option<&str>) -> Result<ParsedPrRef> {
    // ── Form 4: full URL ──────────────────────────────────────────────────────
    if input.starts_with("https://") || input.starts_with("http://") {
        return parse_url(input);
    }

    // ── Form 2 / 3: owner/repo#number ────────────────────────────────────────
    if let Some((repo, num_str)) = input.split_once('#') {
        if RepoName::try_from(repo).is_ok() {
            let number = num_str.parse::<u64>().map_err(|_| GgrError::InvalidPrRef {
                raw: input.to_owned(),
            })?;

            let (repo_flag, hostname) = match url_flag {
                Some(url) => {
                    let host = extract_host(url, input)?;
                    (Some(format!("{host}/{repo}")), Some(host))
                }
                None => (Some(repo.to_owned()), None),
            };

            return Ok(ParsedPrRef {
                number,
                repo_flag,
                hostname,
            });
        }
    }

    // ── Form 1: bare number (optional leading #) ──────────────────────────────
    let digits = input.trim_start_matches('#');
    if let Ok(number) = digits.parse::<u64>() {
        if url_flag.is_some() {
            // --url without an explicit repo makes no sense: gh can't know
            // which repo to query on a remote host without being told.
            return Err(GgrError::InvalidPrRef {
                raw: format!(
                    "--url requires an explicit repo (use owner/repo#{digits} instead of {digits})"
                ),
            });
        }
        return Ok(ParsedPrRef {
            number,
            repo_flag: None,
            hostname: None,
        });
    }

    Err(GgrError::InvalidPrRef {
        raw: input.to_owned(),
    })
}

/// Parse `https://host/owner/repo/pull/number` into a `ParsedPrRef`.
fn parse_url(url: &str) -> Result<ParsedPrRef> {
    // Strip scheme: "https://host/owner/repo/pull/42"
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    // Split off host from the rest.
    let (host, path) = without_scheme
        .split_once('/')
        .ok_or_else(|| GgrError::InvalidPrRef {
            raw: url.to_owned(),
        })?;

    // Expect path = "owner/repo/pull/number"
    let parts: Vec<&str> = path.splitn(4, '/').collect();
    if parts.len() != 4 || parts[2] != "pull" {
        return Err(GgrError::InvalidPrRef {
            raw: url.to_owned(),
        });
    }

    let owner = parts[0];
    let repo = parts[1];
    let number = parts[3]
        .parse::<u64>()
        .map_err(|_| GgrError::InvalidPrRef {
            raw: url.to_owned(),
        })?;

    let repo_str = format!("{owner}/{repo}");

    // Standard github.com — no hostname needed.
    let hostname = if host == "github.com" {
        None
    } else {
        Some(host.to_owned())
    };

    let repo_flag = match &hostname {
        Some(h) => Some(format!("{h}/{repo_str}")),
        None => Some(repo_str),
    };

    Ok(ParsedPrRef {
        number,
        repo_flag,
        hostname,
    })
}

/// Extract the bare hostname from a base URL string (e.g. `https://github.example.com`
/// → `github.example.com`). The raw PR ref string is included in error messages.
fn extract_host(url: &str, pr_raw: &str) -> Result<String> {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty() {
        return Err(GgrError::InvalidPrRef {
            raw: pr_raw.to_owned(),
        });
    }
    Ok(host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_number() {
        let r = parse("42", None).unwrap();
        assert_eq!(r.number, 42);
        assert!(r.repo_flag.is_none());
        assert!(r.hostname.is_none());
    }

    #[test]
    fn bare_number_with_hash_prefix() {
        let r = parse("#42", None).unwrap();
        assert_eq!(r.number, 42);
    }

    #[test]
    fn repo_and_number() {
        let r = parse("acme/myrepo#2429", None).unwrap();
        assert_eq!(r.number, 2429);
        assert_eq!(r.repo_flag.as_deref(), Some("acme/myrepo"));
        assert!(r.hostname.is_none());
    }

    #[test]
    fn repo_and_number_with_url_flag() {
        let r = parse("acme/myrepo#2429", Some("https://github.example.com")).unwrap();
        assert_eq!(r.number, 2429);
        assert_eq!(
            r.repo_flag.as_deref(),
            Some("github.example.com/acme/myrepo")
        );
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn full_github_url() {
        let r = parse("https://github.com/owner/repo/pull/99", None).unwrap();
        assert_eq!(r.number, 99);
        assert_eq!(r.repo_flag.as_deref(), Some("owner/repo"));
        assert!(r.hostname.is_none());
    }

    #[test]
    fn full_ghe_url() {
        let r = parse("https://github.example.com/acme/myrepo/pull/2429", None).unwrap();
        assert_eq!(r.number, 2429);
        assert_eq!(
            r.repo_flag.as_deref(),
            Some("github.example.com/acme/myrepo")
        );
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn bare_number_with_url_flag_is_error() {
        assert!(parse("42", Some("https://github.example.com")).is_err());
    }

    #[test]
    fn invalid_ref_is_error() {
        assert!(parse("not-a-ref", None).is_err());
    }

    #[test]
    fn malformed_url_is_error() {
        assert!(parse("https://github.com/nopull", None).is_err());
    }
}
