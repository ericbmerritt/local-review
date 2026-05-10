//! Subprocess wrappers for the `gh` CLI.
//!
//! `fetch_pr_details` calls `gh pr view --json ...` for PR metadata (including
//! `headRepository.nameWithOwner` to learn the repo slug without requiring a
//! local git clone). `fetch_commit_diff` calls `gh api` with an
//! `Accept: application/vnd.github.diff` header to get per-commit diffs from
//! the GitHub API — no local `git clone` required.

use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use local_review_core::diff::Diff;

use snafu::IntoError as _;

use crate::error::{GgrError, GhFailedSnafu, GhMissingSnafu, Result};
use crate::pr::{CommitEntry, PrComment, PrDetails, RepoName, ReviewThread, ThreadComment};

// ── gh JSON shapes ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PrJson {
    number: u64,
    title: String,
    body: String,
    comments: Vec<CommentJson>,
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

#[derive(Deserialize)]
struct CommentJson {
    author: CommentAuthorJson,
    body: String,
}

#[derive(Deserialize)]
struct CommentAuthorJson {
    login: String,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Fetch PR metadata, the ordered commit list, and inline review threads from the GitHub API via `gh`.
///
/// Runs `gh pr view <pr> --json ...`. When `repo` is `Some("owner/repo")` the
/// `--repo` flag is passed, allowing use outside a git working tree. When
/// `repo` is `None`, `gh` auto-detects the repo from the current directory's
/// git remote or from `gh repo set-default`.
///
/// The response includes `headRepository.nameWithOwner`, which is stored in
/// `PrDetails.repo_name` and used by `fetch_commit_diff` — so the caller does
/// not need to track the repo slug separately.  Review threads are fetched and
/// populated before returning.
pub(crate) fn fetch_pr_details(
    pr: u64,
    repo: Option<&str>,
    hostname: Option<&str>,
) -> Result<PrDetails> {
    let pr_str = pr.to_string();
    let mut args = vec!["pr", "view", &pr_str, "--json"];
    let fields = "number,title,body,comments,commits,headRepository";
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

    let comments = parsed
        .comments
        .into_iter()
        .map(|c| PrComment {
            author: c.author.login,
            body: c.body,
        })
        .collect();

    let repo_name = RepoName::try_from(parsed.head_repository.name_with_owner.as_str())?;

    let review_threads = fetch_review_threads(pr, &repo_name, hostname)?;

    Ok(PrDetails {
        number: parsed.number,
        title: parsed.title,
        body: parsed.body,
        comments,
        repo_name,
        hostname: hostname.map(str::to_owned),
        commits,
        review_threads,
    })
}

/// Fetch the diff for a single commit via the GitHub API.
///
/// Uses `gh api repos/{repo_name}/commits/{sha}` with
/// `Accept: application/vnd.github.diff`, which returns a standard unified
/// diff. No local git clone is required; `repo_name` is a validated
/// [`RepoName`]. Pass `hostname` for GitHub Enterprise Server endpoints.
pub(crate) fn fetch_commit_diff(
    repo_name: &RepoName,
    sha: &str,
    hostname: Option<&str>,
) -> Result<Diff> {
    let endpoint = format!("repos/{}/commits/{sha}", repo_name.as_str());
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

// ── review comments JSON shapes ───────────────────────────────────────────────

/// Raw shape for one element of `GET /pulls/{pr}/comments`.
///
/// GitHub returns a flat JSON array; replies are distinguished by the presence
/// of `in_reply_to_id`.
#[derive(Deserialize)]
struct RawReviewComment {
    id: u64,
    path: String,
    /// `null` when the thread is outdated (the anchored diff line is gone).
    position: Option<u32>,
    original_commit_id: String,
    /// Present and non-null for reply comments; absent/null for root comments.
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    user: CommentAuthorJson,
    created_at: String,
    body: String,
}

// ── grouping logic (pure) ─────────────────────────────────────────────────────

/// Group a flat list of raw review comments into per-root-comment threads.
///
/// GitHub returns all review comments for a PR as a flat array in
/// chronological order.  Root comments have no `in_reply_to_id`; reply
/// comments carry the `id` of their root.
///
/// **Orphan replies** — replies whose `in_reply_to_id` does not match any root
/// comment in the array — are silently skipped.  This is a defensive choice:
/// the GitHub API should never produce orphan replies in practice, but if it
/// does, attaching them to an arbitrary thread would corrupt the display.
/// Skipping keeps all other threads correct.
///
/// The returned `Vec` preserves the order of root comments as they appeared in
/// the input array.  Replies within each thread are appended in input order
/// (GitHub already returns them chronologically).
fn group_review_comments(raw: Vec<RawReviewComment>) -> Vec<ReviewThread> {
    let mut threads: Vec<ReviewThread> = Vec::new();
    let mut root_index: HashMap<u64, usize> = HashMap::new();

    for comment in raw {
        match comment.in_reply_to_id {
            None => {
                let root = ThreadComment {
                    id: comment.id,
                    author: comment.user.login,
                    created_at: comment.created_at,
                    body: comment.body,
                };
                let thread = ReviewThread {
                    path: comment.path,
                    position: comment.position,
                    original_commit_id: comment.original_commit_id,
                    root,
                    replies: vec![],
                };
                root_index.insert(comment.id, threads.len());
                threads.push(thread);
            }
            Some(parent_id) => {
                if let Some(&idx) = root_index.get(&parent_id) {
                    let reply = ThreadComment {
                        id: comment.id,
                        author: comment.user.login,
                        created_at: comment.created_at,
                        body: comment.body,
                    };
                    threads[idx].replies.push(reply);
                }
            }
        }
    }

    threads
}

// ── public API (continued) ────────────────────────────────────────────────────

/// Fetch and group inline review threads for a pull request via `gh`.
///
/// Calls `gh api repos/{repo_name}/pulls/{pr}/comments`, which returns a flat
/// JSON array of all inline review comments.  The flat list is grouped into
/// [`ReviewThread`] values by [`group_review_comments`].
///
/// Pass `hostname` for GitHub Enterprise Server endpoints; `None` uses
/// github.com.
///
/// Failures from the `gh` subprocess are returned as hard errors — a missing
/// thread list is not silently swallowed, because the caller needs accurate
/// thread state to display existing review context.
pub(crate) fn fetch_review_threads(
    pr: u64,
    repo_name: &RepoName,
    hostname: Option<&str>,
) -> Result<Vec<ReviewThread>> {
    let endpoint = format!("repos/{}/pulls/{pr}/comments", repo_name.as_str());
    let mut args = vec!["api", &endpoint, "--paginate"];
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

    let comments: Vec<RawReviewComment> = {
        let mut all: Vec<RawReviewComment> = Vec::new();
        let mut stream =
            serde_json::Deserializer::from_str(&raw).into_iter::<Vec<RawReviewComment>>();
        for page in &mut stream {
            let page = page.map_err(|source| GgrError::ReviewCommentParse { source })?;
            all.extend(page);
        }
        all
    };

    Ok(group_review_comments(comments))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{group_review_comments, CommentAuthorJson, RawReviewComment};

    fn make_root(id: u64, path: &str, position: Option<u32>) -> RawReviewComment {
        RawReviewComment {
            id,
            path: path.to_owned(),
            position,
            original_commit_id: format!("deadbeef{id:08x}"),
            in_reply_to_id: None,
            user: CommentAuthorJson {
                login: format!("user{id}"),
            },
            created_at: format!("2024-01-{id:02}T00:00:00Z"),
            body: format!("root comment {id}"),
        }
    }

    fn make_reply(id: u64, parent_id: u64) -> RawReviewComment {
        RawReviewComment {
            id,
            path: String::new(),
            position: None,
            original_commit_id: String::new(),
            in_reply_to_id: Some(parent_id),
            user: CommentAuthorJson {
                login: format!("user{id}"),
            },
            created_at: format!("2024-01-{id:02}T01:00:00Z"),
            body: format!("reply {id} to {parent_id}"),
        }
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let threads = group_review_comments(vec![]);
        assert!(threads.is_empty());
    }

    #[test]
    fn single_root_no_replies_produces_one_thread() {
        let threads = group_review_comments(vec![make_root(1, "src/lib.rs", Some(5))]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.body, "root comment 1");
        assert!(threads[0].replies.is_empty());
    }

    #[test]
    fn single_root_with_two_replies_root_first_replies_in_order() {
        let raw = vec![
            make_root(1, "src/lib.rs", Some(3)),
            make_reply(2, 1),
            make_reply(3, 1),
        ];
        let threads = group_review_comments(raw);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.body, "root comment 1");
        assert_eq!(threads[0].replies.len(), 2);
        assert_eq!(threads[0].replies[0].body, "reply 2 to 1");
        assert_eq!(threads[0].replies[1].body, "reply 3 to 1");
    }

    #[test]
    fn interleaved_roots_and_replies_routed_correctly() {
        let raw = vec![
            make_root(10, "a.rs", Some(1)),
            make_root(20, "b.rs", Some(2)),
            make_reply(11, 10),
            make_reply(21, 20),
        ];
        let threads = group_review_comments(raw);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].root.body, "root comment 10");
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].body, "reply 11 to 10");
        assert_eq!(threads[1].root.body, "root comment 20");
        assert_eq!(threads[1].replies.len(), 1);
        assert_eq!(threads[1].replies[0].body, "reply 21 to 20");
    }

    #[test]
    fn thread_path_and_commit_id_preserved() {
        let threads = group_review_comments(vec![make_root(1, "src/lib.rs", Some(5))]);
        assert_eq!(threads[0].path, "src/lib.rs");
        assert_eq!(threads[0].original_commit_id, "deadbeef00000001");
    }

    #[test]
    fn null_position_thread_is_outdated() {
        let threads = group_review_comments(vec![make_root(1, "src/main.rs", None)]);
        assert_eq!(threads.len(), 1);
        assert!(threads[0].position.is_none());
        assert!(threads[0].is_outdated());
    }

    #[test]
    fn some_position_thread_is_not_outdated() {
        let threads = group_review_comments(vec![make_root(1, "src/main.rs", Some(7))]);
        assert_eq!(threads.len(), 1);
        assert!(threads[0].position.is_some());
        assert!(!threads[0].is_outdated());
    }

    #[test]
    fn orphan_reply_is_skipped_gracefully() {
        let raw = vec![make_root(1, "src/lib.rs", Some(2)), make_reply(2, 999)];
        let threads = group_review_comments(raw);
        assert_eq!(threads.len(), 1);
        assert!(
            threads[0].replies.is_empty(),
            "orphan reply must be skipped"
        );
    }

    #[test]
    fn reply_before_root_is_treated_as_orphan() {
        let raw = vec![make_reply(2, 1), make_root(1, "src/lib.rs", Some(3))];
        let threads = group_review_comments(raw);
        assert_eq!(threads.len(), 1, "root creates one thread");
        assert!(
            threads[0].replies.is_empty(),
            "reply-before-root is dropped"
        );
        assert_eq!(threads[0].root.body, "root comment 1");
        assert_eq!(threads[0].path, "src/lib.rs");
    }
}
