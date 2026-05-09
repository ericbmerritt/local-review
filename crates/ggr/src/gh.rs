//! Subprocess wrappers for `gh` and `git`.
//!
//! `fetch_pr_details` calls `gh pr view --json ...` to get the PR commit list.
//! `fetch_commit_diff` calls `git show <sha>` to get a per-commit diff and
//! parses it with `local_review_core::diff::parse`.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use local_review_core::diff::Diff;

use snafu::IntoError as _;

use crate::error::{
    GgrError, GhFailedSnafu, GhMissingSnafu, GitFailedSnafu, GitMissingSnafu, Result,
};
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
/// Runs `gh pr view <pr> --json number,title,headRefName,baseRefName,commits`.
/// `gh` auto-detects the repo from the git remote in the current directory.
pub(crate) fn fetch_pr_details(pr: u64) -> Result<PrDetails> {
    let pr_str = pr.to_string();
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_str,
            "--json",
            "number,title,headRefName,baseRefName,commits",
        ])
        .output()
        .map_err(|source| GhMissingSnafu.into_error(source))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code();
        // A PR-not-found error from gh typically contains "not found" in the message.
        if message.to_lowercase().contains("not found")
            || message.to_lowercase().contains("no pull requests found")
        {
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
        commits,
    })
}

/// Fetch the diff for a single commit via `git show`.
///
/// Uses `git show <sha> --format="" --no-color` so the commit message header
/// is suppressed, leaving only the unified diff output. The leading blank line
/// produced by `--format=""` is handled by the diff parser (which skips
/// non-diff lines).
pub(crate) fn fetch_commit_diff(repo_root: &Path, sha: &str) -> Result<Diff> {
    let output = Command::new("git")
        .args(["show", sha, "--format=", "--no-color"])
        .current_dir(repo_root)
        .output()
        .map_err(|source| GitMissingSnafu.into_error(source))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code();
        return GitFailedSnafu { message, exit_code }.fail();
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|source| GgrError::GitOutputEncoding { source })?;

    local_review_core::diff::parse(&raw).map_err(GgrError::from)
}
