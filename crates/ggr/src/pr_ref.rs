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
use crate::pr::{strip_controls, RepoName};

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
        if let Ok(repo_name) = RepoName::try_from(repo) {
            let number = num_str.parse::<u64>().map_err(|_| GgrError::InvalidPrRef {
                raw: strip_controls(input),
            })?;
            if number == 0 {
                return Err(GgrError::InvalidPrRef {
                    raw: strip_controls(input),
                });
            }

            let (repo_flag, hostname) = match url_flag {
                Some(url) => {
                    let host = extract_host(url, input)?;
                    (Some(format!("{host}/{}", repo_name.as_str())), Some(host))
                }
                None => (Some(repo_name.as_str().to_owned()), None),
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
        if number == 0 {
            return Err(GgrError::InvalidPrRef {
                raw: strip_controls(input),
            });
        }
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
        raw: strip_controls(input),
    })
}

/// Parse `https://host/owner/repo/pull/number` into a `ParsedPrRef`.
fn parse_url(url: &str) -> Result<ParsedPrRef> {
    // Strip scheme: "https://host/owner/repo/pull/42"
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| GgrError::InvalidPrRef {
            raw: strip_controls(url),
        })?;

    // Split off host from the rest.
    let (host, path) = without_scheme
        .split_once('/')
        .ok_or_else(|| GgrError::InvalidPrRef {
            raw: strip_controls(url),
        })?;

    // Hostname character rules match repo-segment rules.
    if !crate::pr::valid_segment(host) {
        return Err(GgrError::InvalidPrRef {
            raw: strip_controls(url),
        });
    }

    // Expect path = "owner/repo/pull/number" — exactly 4 segments, no trailing
    // path or fragment.  Split into at most 5 parts; a 5th part means there is
    // a trailing segment (e.g. "/files") or fragment ("#...") after the number,
    // both of which are invalid.
    let parts: Vec<&str> = path.splitn(5, '/').collect();
    if parts.len() != 4 || parts[2] != "pull" {
        return Err(GgrError::InvalidPrRef {
            raw: strip_controls(url),
        });
    }

    let owner = parts[0];
    let repo = parts[1];
    // Reject fragments embedded in the number segment (e.g. "42#issuecomment-123").
    let number_str = parts[3];
    if number_str.contains('#') {
        return Err(GgrError::InvalidPrRef {
            raw: strip_controls(url),
        });
    }
    let number = number_str
        .parse::<u64>()
        .map_err(|_| GgrError::InvalidPrRef {
            raw: strip_controls(url),
        })?;
    if number == 0 {
        return Err(GgrError::InvalidPrRef {
            raw: strip_controls(url),
        });
    }

    let repo_str = format!("{owner}/{repo}");
    let repo_name = RepoName::try_from(repo_str.as_str()).map_err(|_| GgrError::InvalidPrRef {
        raw: strip_controls(url),
    })?;

    // Standard github.com — no hostname needed.
    let hostname = if host == "github.com" {
        None
    } else {
        Some(host.to_owned())
    };

    let repo_flag = match &hostname {
        Some(h) => Some(format!("{h}/{}", repo_name.as_str())),
        None => Some(repo_name.as_str().to_owned()),
    };

    Ok(ParsedPrRef {
        number,
        repo_flag,
        hostname,
    })
}

/// Extract the bare hostname from a base URL string (e.g. `https://github.example.com`
/// → `github.example.com`). Bare hostnames without a scheme (e.g.
/// `github.example.com`) are also accepted — `unwrap_or(url)` passes them
/// through directly, so `--url github.example.com` works as well as
/// `--url https://github.example.com`. The raw PR ref string is included in
/// error messages.
fn extract_host(url: &str, pr_raw: &str) -> Result<String> {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .trim_end_matches('/');
    // Hostname character rules match repo-segment rules.
    if host.is_empty() || !crate::pr::valid_segment(host) {
        return Err(GgrError::InvalidPrRef {
            raw: strip_controls(pr_raw),
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

    #[test]
    fn url_with_dotdot_owner_is_error() {
        assert!(parse("https://github.com/../etc/passwd/pull/42", None).is_err());
    }

    #[test]
    fn url_flag_with_dotdot_host_is_error() {
        assert!(parse("owner/repo#42", Some("https://../etc/passwd")).is_err());
    }

    #[test]
    fn url_with_underscore_hostname_is_accepted() {
        let r = parse("https://github_internal.corp.com/owner/repo/pull/42", None).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(
            r.repo_flag.as_deref(),
            Some("github_internal.corp.com/owner/repo")
        );
        assert_eq!(r.hostname.as_deref(), Some("github_internal.corp.com"));
    }

    #[test]
    fn url_flag_with_underscore_hostname_is_accepted() {
        let r = parse("owner/repo#42", Some("https://github_corp.internal")).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(
            r.repo_flag.as_deref(),
            Some("github_corp.internal/owner/repo")
        );
        assert_eq!(r.hostname.as_deref(), Some("github_corp.internal"));
    }

    #[test]
    fn short_form_with_dotdot_owner_is_error() {
        assert!(parse("../repo#42", None).is_err());
    }

    #[test]
    fn url_with_all_underscore_hostname_is_error() {
        assert!(parse("owner/repo#42", Some("https://_")).is_err());
    }

    #[test]
    fn url_flag_bare_hostname_is_accepted() {
        let r = parse("owner/repo#42", Some("github.example.com")).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn url_with_trailing_path_segment_is_error() {
        assert!(parse("https://github.com/owner/repo/pull/42/files", None).is_err());
    }

    #[test]
    fn url_with_fragment_is_error() {
        assert!(parse(
            "https://github.com/owner/repo/pull/42#issuecomment-123",
            None
        )
        .is_err());
    }

    #[test]
    fn bare_number_zero_is_error() {
        assert!(parse("0", None).is_err());
    }

    #[test]
    fn repo_and_number_zero_is_error() {
        assert!(parse("owner/repo#0", None).is_err());
    }

    #[test]
    fn url_with_zero_pr_is_error() {
        assert!(parse("https://github.com/owner/repo/pull/0", None).is_err());
    }

    #[test]
    fn url_flag_with_path_suffix_is_error() {
        assert!(parse(
            "owner/repo#42",
            Some("https://github.example.com/extra/path")
        )
        .is_err());
    }

    #[test]
    fn full_http_url_is_accepted() {
        let r = parse("http://github.com/owner/repo/pull/42", None).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(r.repo_flag.as_deref(), Some("owner/repo"));
        // github.com → no hostname override
        assert!(r.hostname.is_none());
    }

    #[test]
    fn url_flag_http_is_accepted() {
        let r = parse("owner/repo#42", Some("http://github.example.com")).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(
            r.repo_flag.as_deref(),
            Some("github.example.com/owner/repo")
        );
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn ansi_escape_in_invalid_ref_is_stripped_from_error_raw() {
        // A crafted input containing ANSI escapes must not survive into the error
        // raw field; control characters are stripped before the value is stored,
        // mirroring the CommitSha::try_from pattern.
        let crafted = "\x1b[31mevil/repo#0\x1b[0m";
        let result = parse(crafted, None);
        assert!(result.is_err());
        if let Err(GgrError::InvalidPrRef { raw }) = result {
            assert!(
                !raw.chars().any(char::is_control),
                "control characters must be stripped from error raw: {raw:?}"
            );
        }
    }

    #[test]
    fn url_flag_trailing_slash_is_accepted() {
        let r = parse("owner/repo#42", Some("https://github.example.com/")).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn url_flag_bare_hostname_trailing_slash_is_accepted() {
        let r = parse("owner/repo#42", Some("github.example.com/")).unwrap();
        assert_eq!(r.number, 42);
        assert_eq!(r.hostname.as_deref(), Some("github.example.com"));
    }

    #[test]
    fn ansi_escape_in_url_repo_name_is_stripped_from_error_raw() {
        // ANSI escapes in the owner segment of a full pull URL must not survive
        // into the InvalidPrRef raw field; strip_controls is applied in the
        // RepoName validation failure path of parse_url.
        let crafted = "https://github.com/\x1b[31mevil\x1b[0m/repo/pull/1";
        let result = parse(crafted, None);
        assert!(result.is_err());
        if let Err(GgrError::InvalidPrRef { raw }) = result {
            assert!(
                !raw.chars().any(char::is_control),
                "control characters must be stripped from error raw: {raw:?}"
            );
        }
    }
}
