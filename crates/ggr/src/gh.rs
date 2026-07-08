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

use local_review_core::comment::Side;
use local_review_core::diff::Diff;
use local_review_core::Severity;

use snafu::IntoError as _;

use local_review_core::util::strip_controls;

use crate::error::{GgrError, GhFailedSnafu, GhMissingSnafu, Result};
use crate::pr::{
    CommitEntry, CommitSha, PrComment, PrDetails, RepoName, ReviewThread, ThreadComment,
};

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
    /// Absent on older GHE releases; default to empty rather than failing
    /// the whole PR fetch.
    #[serde(rename = "messageBody", default)]
    message_body: String,
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
    // When GH_HOST is set the --repo flag must be "owner/repo", not
    // "host/owner/repo". Strip the host prefix if present so that GHE
    // repos work correctly (the host prefix was added for the old
    // --hostname flag approach and is now redundant).
    let repo_stripped;
    if let Some(r) = repo {
        repo_stripped = hostname
            .map(|h| r.strip_prefix(&format!("{h}/")).unwrap_or(r))
            .unwrap_or(r);
        args.push("--repo");
        args.push(repo_stripped);
    }
    // Inline subprocess rather than run_gh: only here can we inspect gh's exit
    // code and stderr to distinguish PrNotFound from RepoNotFound.  run_gh
    // collapses all non-zero exits to GhFailed, losing that discrimination.
    //
    // GH_HOST is used instead of --hostname because older gh releases do not
    // support --hostname on `gh pr view`.
    let mut cmd = Command::new("gh");
    cmd.args(&args);
    if let Some(h) = hostname {
        cmd.env("GH_HOST", h);
    }
    let output = cmd
        .output()
        .map_err(|source| GhMissingSnafu.into_error(source))?;

    if !output.status.success() {
        let message = strip_controls(&String::from_utf8_lossy(&output.stderr));
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
            let sha = CommitSha::try_from(c.oid.as_str())?;
            let short_sha = sha.as_str().chars().take(8).collect();
            Ok(CommitEntry {
                sha,
                short_sha,
                title: c.message_headline,
                body: c.message_body,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut comments: Vec<PrComment> = parsed
        .comments
        .into_iter()
        .map(|c| PrComment {
            author: c.author.login,
            body: c.body,
        })
        .collect();

    let repo_name = RepoName::try_from(parsed.head_repository.name_with_owner.as_str())?;

    // Also fetch review-level bodies (submitted review summaries from other
    // reviewers). These are separate from issue-thread comments and from
    // inline review threads — they're the top-level text of a PR review.
    let review_bodies = fetch_review_bodies(pr, &repo_name, hostname).unwrap_or_default();
    comments.extend(review_bodies);

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

// ── private subprocess helper ─────────────────────────────────────────────────

/// Run `gh <args>` and return stdout as UTF-8.
///
/// `hostname` is forwarded via `GH_HOST` so it works across all `gh` versions.
/// The `--hostname` flag is not used because older `gh` releases do not support
/// it on `gh pr view` (only on `gh api`).
fn run_gh(args: &[&str], hostname: Option<&str>) -> Result<String> {
    run_gh_with_stdin(args, &[], hostname)
}

/// Runs `gh` with the given args, feeding `body` to the process's stdin.
///
/// `hostname` is forwarded via `GH_HOST`. Used for POST endpoints that expect
/// a JSON body via `--input -`.
fn run_gh_with_stdin(args: &[&str], body: &[u8], hostname: Option<&str>) -> Result<String> {
    use std::io::Write as _;
    let mut cmd = Command::new("gh");
    cmd.args(args);
    if let Some(h) = hostname {
        cmd.env("GH_HOST", h);
    }
    cmd.stdin(if body.is_empty() {
        std::process::Stdio::null()
    } else {
        std::process::Stdio::piped()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|source| GhMissingSnafu.into_error(source))?;
    if !body.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(body)
                .map_err(|source| GgrError::Io { source })?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|source| GgrError::Io { source })?;
    if !output.status.success() {
        // gh api writes the status line to stderr and the response JSON body
        // to stdout. Include both so callers can see the actual API error.
        let stderr = strip_controls(&String::from_utf8_lossy(&output.stderr));
        let stdout = strip_controls(&String::from_utf8_lossy(&output.stdout));
        let message = if stdout.trim().is_empty() {
            stderr
        } else {
            format!("{stderr}\n{stdout}")
        };
        let exit_code = output.status.code();
        return GhFailedSnafu { message, exit_code }.fail();
    }
    String::from_utf8(output.stdout).map_err(|source| GgrError::GhOutputEncoding { source })
}

/// Post a pull request review to GitHub.
///
/// Returns `Ok(())` on HTTP 200; propagates `GhFailed` on any API error.
pub(crate) fn post_review(
    repo_name: &RepoName,
    pr_number: u64,
    hostname: Option<&str>,
    payload: &crate::submit::ReviewPayload<'_>,
) -> Result<()> {
    use serde_json::json;
    let endpoint = format!("repos/{}/pulls/{}/reviews", repo_name.as_str(), pr_number);
    let comment_values: Vec<serde_json::Value> = payload
        .comments
        .iter()
        .map(|c| {
            // commit_id is NOT a valid field on DraftPullRequestReviewThread;
            // it belongs at the top-level review, not per-comment.
            json!({
                "path": c.path,
                "line": c.line,
                "side": c.side,
                "body": c.body,
            })
        })
        .collect();
    let json_payload = json!({
        "event": payload.event,
        "body": payload.body,
        "comments": comment_values,
    });
    let json_bytes = serde_json::to_vec(&json_payload).map_err(|e| GgrError::Io {
        source: std::io::Error::other(e),
    })?;
    let args = vec!["api", &endpoint, "-X", "POST", "--input", "-"];
    run_gh_with_stdin(&args, &json_bytes, hostname)?;
    Ok(())
}

/// Post a reply to an existing review comment thread.
///
/// `in_reply_to_id` is the GitHub numeric comment ID of the root comment.
pub(crate) fn post_reply(
    repo_name: &RepoName,
    pr_number: u64,
    hostname: Option<&str>,
    body: &str,
    in_reply_to_id: &str,
) -> Result<()> {
    use serde_json::json;
    let endpoint = format!("repos/{}/pulls/{}/comments", repo_name.as_str(), pr_number);
    let in_reply_to: u64 = in_reply_to_id.parse().map_err(|_| GgrError::GhFailed {
        message: format!(
            "invalid parent_comment_id: {}",
            strip_controls(in_reply_to_id)
        ),
        exit_code: None,
    })?;
    let payload = json!({
        "body": body,
        "in_reply_to": in_reply_to,
    });
    let json_bytes = serde_json::to_vec(&payload).map_err(|e| GgrError::Io {
        source: std::io::Error::other(e),
    })?;
    let args = vec!["api", &endpoint, "-X", "POST", "--input", "-"];
    run_gh_with_stdin(&args, &json_bytes, hostname)?;
    Ok(())
}

/// Fetch the diff for a single commit via the GitHub API.
///
/// Uses `gh api repos/{repo_name}/commits/{sha}` with
/// `Accept: application/vnd.github.diff`, which returns a standard unified
/// diff. No local git clone is required; `repo_name` is a validated
/// [`RepoName`] and `sha` is a validated [`CommitSha`]. Pass `hostname` for
/// GitHub Enterprise Server endpoints.
pub(crate) fn fetch_commit_diff(
    repo_name: &RepoName,
    sha: &CommitSha,
    hostname: Option<&str>,
) -> Result<Diff> {
    let endpoint = format!("repos/{}/commits/{}", repo_name.as_str(), sha.as_str());
    let args = vec![
        "api",
        &endpoint,
        "--header",
        "Accept: application/vnd.github.diff",
    ];
    let raw = run_gh(&args, hostname)?;
    local_review_core::diff::parse(&raw).map_err(GgrError::from)
}

// ── blob content fetching ─────────────────────────────────────────────────────

/// Per-file result from `fetch_commit_file_contents`.
#[derive(Debug)]
pub(crate) struct FilePair {
    pub path: String,
    pub before: String,
    pub after: String,
}

/// Fetch before/after content for each changed file in `file_paths` using a
/// single GraphQL batch query.
///
/// `commit_sha` is the head commit; the before content uses `<sha>^` (the
/// git parent notation), which GitHub's GraphQL API accepts. Returns one
/// `FilePair` per path; missing blobs (added/deleted files) produce empty
/// strings for the absent side.
///
/// On GraphQL error (e.g., response-size limit), all files are returned with
/// empty content — the caller renders them as fallback rows.
pub(crate) fn fetch_commit_file_contents(
    repo_name: &RepoName,
    commit_sha: &CommitSha,
    file_paths: &[String],
    hostname: Option<&str>,
) -> Vec<FilePair> {
    if file_paths.is_empty() {
        return Vec::new();
    }

    let sha = commit_sha.as_str();
    let Some((owner, repo)) = repo_name.as_str().split_once('/') else {
        return fallback_pairs(file_paths);
    };

    let query = build_blob_query(sha, file_paths);
    let body = serde_json::json!({
        "query": query,
        "variables": { "owner": owner, "repo": repo }
    });
    let Ok(body_bytes) = serde_json::to_vec(&body) else {
        return fallback_pairs(file_paths);
    };

    let Ok(raw) = run_gh_with_stdin(&["api", "graphql", "--input", "-"], &body_bytes, hostname)
    else {
        return fallback_pairs(file_paths);
    };

    parse_blob_response(&raw, file_paths).unwrap_or_else(|_| fallback_pairs(file_paths))
}

fn fallback_pairs(file_paths: &[String]) -> Vec<FilePair> {
    file_paths
        .iter()
        .map(|p| FilePair {
            path: p.clone(),
            before: String::new(),
            after: String::new(),
        })
        .collect()
}

/// Build a batch GraphQL query that fetches each file at `<sha>:path` (after)
/// and `<sha>^:path` (before) using field aliases.
fn build_blob_query(sha: &str, file_paths: &[String]) -> String {
    let mut q =
        String::from("query($owner:String!,$repo:String!){repository(owner:$owner,name:$repo){");
    for (i, path) in file_paths.iter().enumerate() {
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        let after_alias =
            format!("h{i}:object(expression:\"{sha}:{escaped}\"){{...on Blob{{text}}}}");
        let before_alias =
            format!("b{i}:object(expression:\"{sha}^:{escaped}\"){{...on Blob{{text}}}}");
        q.push_str(&after_alias);
        q.push_str(&before_alias);
    }
    q.push_str("}}");
    q
}

fn parse_blob_response(raw: &str, file_paths: &[String]) -> Result<Vec<FilePair>> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|source| GgrError::GhJsonParse { source })?;
    let repo = &v["data"]["repository"];
    let mut pairs = Vec::with_capacity(file_paths.len());
    for (i, path) in file_paths.iter().enumerate() {
        let after = repo[format!("h{i}")]
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let before = repo[format!("b{i}")]
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        pairs.push(FilePair {
            path: path.clone(),
            before,
            after,
        });
    }
    Ok(pairs)
}

// ── review comments JSON shapes ───────────────────────────────────────────────

/// GitHub API value for the `side` field on a review comment.
///
/// `#[serde(other)]` on `Unknown` captures any unrecognized string (e.g.
/// `"BOTH"`) as a named variant rather than silently discarding it via `_ =>
/// None`, keeping the exhaustive match in `group_review_comments` compiler-
/// enforced.
#[derive(Deserialize, Debug)]
enum RawSide {
    #[serde(rename = "LEFT")]
    Left,
    #[serde(rename = "RIGHT")]
    Right,
    #[serde(other)]
    Unknown,
}

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
    /// `null` for hunk-context and outdated threads.
    #[serde(default)]
    line: Option<u32>,
    /// `null` for right-side, hunk-context, and outdated threads.
    #[serde(default)]
    original_line: Option<u32>,
    /// `null` for hunk-context and outdated threads.
    #[serde(default)]
    side: Option<RawSide>,
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
fn group_review_comments(raw: Vec<RawReviewComment>) -> Result<Vec<ReviewThread>> {
    let mut threads: Vec<ReviewThread> = Vec::new();
    let mut root_index: HashMap<u64, usize> = HashMap::new();

    for comment in raw {
        match comment.in_reply_to_id {
            None => {
                let original_commit_id = CommitSha::try_from(comment.original_commit_id.as_str())?;
                let root = ThreadComment {
                    id: comment.id,
                    author: comment.user.login,
                    created_at: comment.created_at,
                    body: comment.body,
                };
                let diff_side = match comment.side {
                    Some(RawSide::Right) => Some(Side::New),
                    Some(RawSide::Left) => Some(Side::Old),
                    Some(RawSide::Unknown) | None => None,
                };
                let thread = ReviewThread {
                    path: strip_controls(&comment.path),
                    position: comment.position,
                    original_commit_id,
                    root,
                    replies: vec![],
                    line: comment.line,
                    original_line: comment.original_line,
                    diff_side,
                    severity: Severity::Note,
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

    Ok(threads)
}

/// Fetch and group inline review threads for a pull request via `gh`.
///
/// Fetch the top-level body text of submitted PR reviews.
///
/// Each GitHub PR review can carry a summary body (the text a reviewer types
/// in the review box before clicking Approve/Request Changes/Comment). These
/// bodies are separate from issue-thread comments and from inline review
/// threads; they live at `GET /repos/{repo}/pulls/{pr}/reviews`.
///
/// Returns `Vec<PrComment>` so review bodies can be shown alongside
/// issue-thread comments on the description page. Soft-fails to empty on any
/// error — missing review bodies are cosmetic, not load-bearing.
fn fetch_review_bodies(
    pr: u64,
    repo_name: &RepoName,
    hostname: Option<&str>,
) -> Option<Vec<PrComment>> {
    #[derive(Deserialize)]
    struct ReviewJson {
        user: ReviewUserJson,
        body: String,
        state: String,
    }
    #[derive(Deserialize)]
    struct ReviewUserJson {
        login: String,
    }

    let endpoint = format!("repos/{}/pulls/{pr}/reviews", repo_name.as_str());
    let raw = run_gh(&["api", &endpoint, "--paginate"], hostname).ok()?;

    let mut all: Vec<ReviewJson> = Vec::new();
    let mut stream = serde_json::Deserializer::from_str(&raw).into_iter::<Vec<ReviewJson>>();
    for page in &mut stream {
        all.extend(page.ok()?);
    }

    let bodies: Vec<PrComment> = all
        .into_iter()
        .filter(|r| !r.body.trim().is_empty())
        .map(|r| {
            let label = match r.state.as_str() {
                "APPROVED" => "approved",
                "CHANGES_REQUESTED" => "requested changes",
                _ => "commented",
            };
            PrComment {
                author: format!("{} ({})", strip_controls(&r.user.login), label),
                body: strip_controls(&r.body),
            }
        })
        .collect();

    Some(bodies)
}

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
    let args = vec!["api", &endpoint, "--paginate"];
    let raw = run_gh(&args, hostname)?;

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

    group_review_comments(comments)
}

/// Fetch the single-line commit message headline for `sha`.
///
/// Used by the re-anchoring pass to find a subject-based successor when a
/// commit SHA is no longer in the PR. Returns `None` if the commit is not
/// accessible (garbage-collected, network error, etc.) so callers can fall
/// back to marking the draft stale.
pub(crate) fn fetch_commit_subject(
    repo_name: &RepoName,
    sha: &str,
    hostname: Option<&str>,
) -> Option<String> {
    let endpoint = format!(
        "repos/{}/commits/{}",
        repo_name.as_str(),
        strip_controls(sha)
    );
    let args = vec!["api", &endpoint, "--jq", ".commit.message"];
    let raw = run_gh(&args, hostname).ok()?;
    // The jq expression returns the full commit message; subject is the first line.
    let subject = strip_controls(raw.lines().next()?.trim());
    if subject.is_empty() {
        None
    } else {
        Some(subject)
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{group_review_comments, CommentAuthorJson, RawReviewComment, RawSide, Side};

    fn make_root(id: u64, path: &str, position: Option<u32>) -> RawReviewComment {
        RawReviewComment {
            id,
            path: path.to_owned(),
            position,
            original_commit_id: format!("{id:040x}"),
            in_reply_to_id: None,
            user: CommentAuthorJson {
                login: format!("user{id}"),
            },
            created_at: format!("2024-01-{id:02}T00:00:00Z"),
            body: format!("root comment {id}"),
            line: None,
            original_line: None,
            side: None,
        }
    }

    fn make_reply(id: u64, parent_id: u64) -> RawReviewComment {
        RawReviewComment {
            id,
            path: String::new(),
            position: None,
            // Replies carry original_commit_id in the API response but it is not
            // structurally meaningful: the field anchors root comments to a diff
            // position; group_review_comments skips SHA validation for replies, so
            // any string (including empty) is valid here.
            original_commit_id: String::new(),
            in_reply_to_id: Some(parent_id),
            user: CommentAuthorJson {
                login: format!("user{id}"),
            },
            created_at: format!("2024-01-{id:02}T01:00:00Z"),
            body: format!("reply {id} to {parent_id}"),
            line: None,
            original_line: None,
            side: None,
        }
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let threads = group_review_comments(vec![]).unwrap();
        assert!(threads.is_empty());
    }

    #[test]
    fn single_root_no_replies_produces_one_thread() {
        let threads = group_review_comments(vec![make_root(1, "src/lib.rs", Some(5))]).unwrap();
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
        let threads = group_review_comments(raw).unwrap();
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
        let threads = group_review_comments(raw).unwrap();
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
        let threads = group_review_comments(vec![make_root(1, "src/lib.rs", Some(5))]).unwrap();
        assert_eq!(threads[0].path, "src/lib.rs");
        assert_eq!(
            threads[0].original_commit_id.as_str(),
            "0000000000000000000000000000000000000001"
        );
    }

    #[test]
    fn null_position_thread_is_outdated() {
        let threads = group_review_comments(vec![make_root(1, "src/main.rs", None)]).unwrap();
        assert_eq!(threads.len(), 1);
        assert!(threads[0].position.is_none());
        assert!(threads[0].is_outdated());
    }

    #[test]
    fn some_position_thread_is_not_outdated() {
        let threads = group_review_comments(vec![make_root(1, "src/main.rs", Some(7))]).unwrap();
        assert_eq!(threads.len(), 1);
        assert!(threads[0].position.is_some());
        assert!(!threads[0].is_outdated());
    }

    #[test]
    fn orphan_reply_is_skipped_gracefully() {
        let raw = vec![make_root(1, "src/lib.rs", Some(2)), make_reply(2, 999)];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads.len(), 1);
        assert!(
            threads[0].replies.is_empty(),
            "orphan reply must be skipped"
        );
    }

    #[test]
    fn root_with_invalid_sha_returns_err() {
        let raw = vec![RawReviewComment {
            id: 1,
            path: "src/lib.rs".to_owned(),
            position: Some(1),
            original_commit_id: "bad_sha".to_owned(),
            in_reply_to_id: None,
            user: CommentAuthorJson {
                login: "user1".to_owned(),
            },
            created_at: "2024-01-01T00:00:00Z".to_owned(),
            body: "comment".to_owned(),
            line: None,
            original_line: None,
            side: None,
        }];
        assert!(group_review_comments(raw).is_err());
    }

    #[test]
    fn group_review_comments_returns_err_for_invalid_sha_on_second_root() {
        let raw = vec![
            make_root(1, "src/lib.rs", Some(1)),
            RawReviewComment {
                id: 2,
                path: "src/main.rs".to_owned(),
                position: Some(2),
                original_commit_id: "bad_sha".to_owned(),
                in_reply_to_id: None,
                user: CommentAuthorJson {
                    login: "user2".to_owned(),
                },
                created_at: "2024-01-02T00:00:00Z".to_owned(),
                body: "second root".to_owned(),
                line: None,
                original_line: None,
                side: None,
            },
        ];
        assert!(group_review_comments(raw).is_err());
    }

    #[test]
    fn reply_before_root_is_treated_as_orphan() {
        let raw = vec![make_reply(2, 1), make_root(1, "src/lib.rs", Some(3))];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads.len(), 1, "root creates one thread");
        assert!(
            threads[0].replies.is_empty(),
            "reply-before-root is dropped"
        );
        assert_eq!(threads[0].root.body, "root comment 1");
        assert_eq!(threads[0].path, "src/lib.rs");
    }

    #[test]
    fn control_char_created_at_survives_into_thread_comment() {
        let raw = vec![RawReviewComment {
            created_at: "2024-01-01T\x1b[1m10:30:00Z".to_owned(),
            ..make_root(1, "src/lib.rs", Some(1))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert!(
            threads[0].root.created_at.contains('\x1b'),
            "control character must survive unmodified; stripping is the renderer's responsibility"
        );
    }

    #[test]
    fn control_char_created_at_in_reply_survives_into_thread_comment() {
        let raw = vec![
            make_root(1, "src/lib.rs", Some(1)),
            RawReviewComment {
                created_at: "2024-01-02T\x1b[1m10:30:00Z".to_owned(),
                ..make_reply(2, 1)
            },
        ];
        let threads = group_review_comments(raw).unwrap();
        assert!(
            threads[0].replies[0].created_at.contains('\x1b'),
            "control character must survive in reply created_at; stripping is the renderer's responsibility"
        );
    }

    #[test]
    fn reply_with_invalid_sha_is_not_validated() {
        let raw = vec![
            make_root(1, "src/lib.rs", Some(1)),
            RawReviewComment {
                original_commit_id: "bad_sha_not_validated".to_owned(),
                ..make_reply(2, 1)
            },
        ];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].replies.len(), 1);
    }

    #[test]
    fn root_with_line_and_right_side_propagated_to_thread() {
        let raw = vec![RawReviewComment {
            line: Some(42),
            side: Some(RawSide::Right),
            ..make_root(1, "src/lib.rs", Some(5))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].line, Some(42));
        assert_eq!(threads[0].diff_side, Some(Side::New));
        assert_eq!(threads[0].original_line, None);
    }

    #[test]
    fn root_with_original_line_and_left_side_propagated_to_thread() {
        let raw = vec![RawReviewComment {
            original_line: Some(7),
            side: Some(RawSide::Left),
            line: None,
            ..make_root(1, "src/lib.rs", Some(5))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].original_line, Some(7));
        assert_eq!(threads[0].diff_side, Some(Side::Old));
        assert_eq!(threads[0].line, None);
    }

    #[test]
    fn root_with_none_line_produces_thread_with_none_line() {
        let raw = vec![make_root(1, "src/main.rs", Some(3))];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].line, None);
        assert_eq!(threads[0].diff_side, None);
        assert_eq!(threads[0].original_line, None);
    }

    #[test]
    fn root_with_unrecognized_side_string_produces_none_diff_side() {
        let raw = vec![RawReviewComment {
            side: Some(RawSide::Unknown),
            line: Some(10),
            ..make_root(1, "src/lib.rs", Some(5))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].diff_side, None);
    }

    #[test]
    fn right_side_thread_with_both_line_and_original_line_stores_both() {
        let raw = vec![RawReviewComment {
            side: Some(RawSide::Right),
            line: Some(42),
            original_line: Some(5),
            ..make_root(1, "src/lib.rs", Some(5))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert_eq!(threads[0].line, Some(42));
        assert_eq!(threads[0].original_line, Some(5));
        assert_eq!(threads[0].diff_side, Some(Side::New));
    }

    #[test]
    fn group_review_comments_strips_control_chars_from_path() {
        let crafted_path = "\x1b[31mevil\x1b[0m/src/lib.rs";
        let raw = vec![RawReviewComment {
            path: crafted_path.to_owned(),
            ..make_root(1, "ignored", Some(1))
        }];
        let threads = group_review_comments(raw).unwrap();
        assert!(
            !threads[0].path.chars().any(char::is_control),
            "thread.path must have control chars stripped; got: {:?}",
            threads[0].path
        );
    }
}
