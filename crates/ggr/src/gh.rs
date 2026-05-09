//! Subprocess wrappers for the `gh` CLI.
//!
//! `fetch_pr_details` calls `gh pr view --json ...` for PR metadata (including
//! `headRepository.nameWithOwner` to learn the repo slug without requiring a
//! local git clone). `fetch_commit_diff` calls `gh api` with an
//! `Accept: application/vnd.github.diff` header to get per-commit diffs from
//! the GitHub API — no local `git clone` required.

use std::process::Command;

use serde::Deserialize;

use local_review_core::diff::Diff;

use snafu::IntoError as _;

use crate::error::{GgrError, GhFailedSnafu, GhMissingSnafu, Result};
use crate::pr::{CommitEntry, PrDetails};

// ── gh JSON shapes ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PrJson {
    number: u64,
    title: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    commits: Vec<CommitJson>,
    #[serde(rename = "headRepository")]
    head_repository: HeadRepositoryJson,
}

#[derive(Deserialize)]
struct HeadRepositoryJson {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct CommitJson {
    oid: String,
    #[serde(rename = "messageHeadline")]
    message_headline: String,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Fetch PR metadata and the ordered commit list from the GitHub API via `gh`.
///
/// Runs `gh pr view <pr> --json ...`. When `repo` is `Some("owner/repo")` the
/// `--repo` flag is passed, allowing use outside a git working tree. When
/// `repo` is `None`, `gh` auto-detects the repo from the current directory's
/// git remote or from `gh repo set-default`.
///
/// The response includes `headRepository.nameWithOwner`, which is stored in
/// `PrDetails.repo_name` and used by `fetch_commit_diff` — so the caller does
/// not need to track the repo slug separately.
pub(crate) fn fetch_pr_details(pr: u64, repo: Option<&str>) -> Result<PrDetails> {
    let pr_str = pr.to_string();
    let mut args = vec!["pr", "view", &pr_str, "--json"];
    let fields = "number,title,headRefName,baseRefName,commits,headRepository";
    args.push(fields);
    if let Some(r) = repo {
        args.push("--repo");
        args.push(r);
    }

    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| GhMissingSnafu.into_error(source))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code();
        let msg_lower = message.to_lowercase();
        if msg_lower.contains("could not resolve to a repository") {
            let repo = repo.unwrap_or("unknown").to_owned();
            return Err(GgrError::RepoNotFound { repo });
        }
        if msg_lower.contains("not found") || msg_lower.contains("no pull requests found") {
            return Err(GgrError::PrNotFound { pr });
        }
        return GhFailedSnafu { message, exit_code }.fail();
    }

    let raw =
        String::from_utf8(output.stdout).map_err(|source| GgrError::GhOutputEncoding { source })?;

    let parsed: PrJson =
        serde_json::from_str(&raw).map_err(|source| GgrError::GhJsonParse { source })?;

    let commits = parsed
        .commits
        .into_iter()
        .map(|c| {
            let short_sha = c.oid.chars().take(8).collect();
            CommitEntry {
                sha: c.oid,
                short_sha,
                title: c.message_headline,
            }
        })
        .collect();

    Ok(PrDetails {
        number: parsed.number,
        title: parsed.title,
        head_ref: parsed.head_ref_name,
        base_ref: parsed.base_ref_name,
        repo_name: parsed.head_repository.name_with_owner,
        hostname: None,
        commits,
    })
}

/// Fetch the diff for a single commit via the GitHub API.
///
/// Uses `gh api repos/{repo_name}/commits/{sha}` with
/// `Accept: application/vnd.github.diff`, which returns a standard unified
/// diff. No local git clone is required; `repo_name` is `owner/repo`.
/// Pass `hostname` for GitHub Enterprise Server endpoints.
pub(crate) fn fetch_commit_diff(
    repo_name: &str,
    sha: &str,
    hostname: Option<&str>,
) -> Result<Diff> {
    let endpoint = format!("repos/{repo_name}/commits/{sha}");
    let mut args = vec![
        "api",
        &endpoint,
        "--header",
        "Accept: application/vnd.github.diff",
    ];
    if let Some(host) = hostname {
        args.push("--hostname");
        args.push(host);
    }
    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| GhMissingSnafu.into_error(source))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code();
        return GhFailedSnafu { message, exit_code }.fail();
    }

    let raw =
        String::from_utf8(output.stdout).map_err(|source| GgrError::GhOutputEncoding { source })?;

    local_review_core::diff::parse(&raw).map_err(GgrError::from)
}
