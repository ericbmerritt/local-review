//! Submit logic: assemble a GitHub review payload from local drafts and post it.
//!
//! Entry point is [`submit`]. The caller is responsible for verdict selection;
//! this module handles payload assembly, API calls, and draft clearing.

use std::path::Path;

use local_review_core::comment::Side;
use local_review_core::util::strip_controls;
use local_review_core::Severity;

use crate::draft::{
    delete_reply, drafts_dir_from_base, list_drafts, list_replies, replies_file_from_base,
    GgrAnchor, GgrDraft, GgrReply,
};
use crate::error::{GgrError, Result};
use crate::pr::{CommitEntry, PrDetails};

// ── types ─────────────────────────────────────────────────────────────────────

/// The reviewer's verdict for the GitHub review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    pub(crate) fn as_api_str(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
            Self::Comment => "COMMENT",
        }
    }
}

/// One line-anchored inline comment in the review payload.
pub(crate) struct ReviewLineComment {
    pub(crate) path: String,
    /// 1-based line number on the appropriate diff side.
    pub(crate) line: u32,
    /// `"LEFT"` (old side) or `"RIGHT"` (new side).
    pub(crate) side: &'static str,
    pub(crate) commit_id: String,
    pub(crate) body: String,
}

/// Outcome of a submit attempt.
#[derive(Debug)]
pub(crate) struct SubmitOutcome {
    pub(crate) message: String,
}

/// Payload for [`crate::gh::post_review`].
pub(crate) struct ReviewPayload<'a> {
    pub(crate) event: &'a str,
    pub(crate) body: &'a str,
    pub(crate) comments: &'a [ReviewLineComment],
}

// ── body assembly ─────────────────────────────────────────────────────────────

fn severity_marker(severity: Severity) -> &'static str {
    match severity {
        Severity::Required => "[REQUIRED]",
        Severity::Suggestion => "[SUGGESTION]",
        Severity::Note => "[NOTE]",
    }
}

fn format_comment_body(body: &str, severity: Severity) -> String {
    format!("{}\n\n{}", severity_marker(severity), strip_controls(body))
}

/// Build the review `body` field from PR-scoped and commit-scoped drafts.
///
/// PR-scoped drafts render verbatim (with severity marker). Commit-scoped
/// drafts render as quoted attribution blocks so the commit context is visible
/// on the GitHub review page.
pub(crate) fn build_review_body(
    pr_drafts: &[&GgrDraft],
    commit_drafts: &[&GgrDraft],
    commits: &[CommitEntry],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    for draft in pr_drafts {
        parts.push(format_comment_body(&draft.body, draft.severity));
    }

    for draft in commit_drafts {
        let GgrAnchor::Commit { commit_sha } = &draft.anchor else {
            continue;
        };
        let short_sha = &commit_sha.as_str()[..8];
        let title = commits
            .iter()
            .find(|c| c.sha.as_str() == commit_sha.as_str())
            .map(|c| c.title.as_str())
            .unwrap_or("unknown commit");
        let attribution = format!("> Commit {short_sha} — \"{title}\"");
        let body_quoted = strip_controls(&draft.body)
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        let severity_line = format!("> {}", severity_marker(draft.severity));
        parts.push(format!(
            "{attribution}\n>\n{severity_line}\n>\n{body_quoted}"
        ));
    }

    parts.join("\n\n---\n\n")
}

/// Convert a line-scoped draft to a `ReviewLineComment`, or `None` if the
/// anchor is missing a line number (should not happen for valid drafts).
pub(crate) fn draft_to_line_comment(draft: &GgrDraft) -> Option<ReviewLineComment> {
    let GgrAnchor::Line {
        commit_sha,
        file,
        side,
        old_line,
        new_line,
        ..
    } = &draft.anchor
    else {
        return None;
    };
    let (line, api_side) = match side {
        Side::New => ((*new_line)?, "RIGHT"),
        Side::Old => ((*old_line)?, "LEFT"),
    };
    Some(ReviewLineComment {
        path: file.clone(),
        line,
        side: api_side,
        commit_id: commit_sha.as_str().to_owned(),
        body: format_comment_body(&draft.body, draft.severity),
    })
}

// ── PR storage coordinates ────────────────────────────────────────────────────

/// Groups the five parameters needed to locate a PR's storage directory.
pub(crate) struct PrCoords<'a> {
    base: &'a Path,
    host: &'a str,
    owner: &'a str,
    repo: &'a str,
    pr_number: u64,
}

// ── draft collection ──────────────────────────────────────────────────────────

/// Collect all drafts and replies for a PR across all commits.
///
/// Reads `<sha>.jsonl` for every commit in `commits`, plus `_pr.jsonl` and
/// `_replies.jsonl`. Skips files that don't exist.
pub(crate) fn collect_all_pr_drafts(
    coords: &PrCoords<'_>,
    commits: &[CommitEntry],
) -> Result<(Vec<GgrDraft>, Vec<GgrReply>)> {
    let drafts_dir = drafts_dir_from_base(
        coords.base,
        coords.host,
        coords.owner,
        coords.repo,
        coords.pr_number,
    );
    let mut drafts: Vec<GgrDraft> = Vec::new();

    for commit in commits {
        let path = drafts_dir.join(format!("{}.jsonl", commit.sha.as_str()));
        drafts.extend(list_drafts(&path)?);
    }
    let pr_file = drafts_dir.join("_pr.jsonl");
    drafts.extend(list_drafts(&pr_file)?);

    let replies_file = replies_file_from_base(
        coords.base,
        coords.host,
        coords.owner,
        coords.repo,
        coords.pr_number,
    );
    let replies = list_replies(&replies_file)?;

    Ok((drafts, replies))
}

/// Clear all draft files (commit, PR-scope) for the PR. Does not touch replies.
fn clear_review_drafts(coords: &PrCoords<'_>, commits: &[CommitEntry]) -> Result<()> {
    let drafts_dir = drafts_dir_from_base(
        coords.base,
        coords.host,
        coords.owner,
        coords.repo,
        coords.pr_number,
    );
    for commit in commits {
        let path = drafts_dir.join(format!("{}.jsonl", commit.sha.as_str()));
        if path.exists() {
            crate::draft::clear_drafts(&path)?;
        }
    }
    let pr_file = drafts_dir.join("_pr.jsonl");
    if pr_file.exists() {
        crate::draft::clear_drafts(&pr_file)?;
    }
    Ok(())
}

// ── submit helpers ────────────────────────────────────────────────────────────

/// Build the review payload from drafts and POST it to GitHub.
fn post_review_from_drafts(
    pr: &PrDetails,
    verdict: Verdict,
    all_drafts: &[GgrDraft],
) -> Result<()> {
    let pr_scope: Vec<&GgrDraft> = all_drafts
        .iter()
        .filter(|d| matches!(d.anchor, GgrAnchor::Pr))
        .collect();
    let commit_scope: Vec<&GgrDraft> = all_drafts
        .iter()
        .filter(|d| matches!(d.anchor, GgrAnchor::Commit { .. }))
        .collect();
    let line_scope: Vec<&GgrDraft> = all_drafts
        .iter()
        .filter(|d| matches!(d.anchor, GgrAnchor::Line { .. }))
        .collect();
    let review_body = build_review_body(&pr_scope, &commit_scope, &pr.commits);
    let comments: Vec<ReviewLineComment> = line_scope
        .iter()
        .filter_map(|d| draft_to_line_comment(d))
        .collect();
    crate::gh::post_review(
        &pr.repo_name,
        pr.number,
        pr.hostname.as_deref(),
        &ReviewPayload {
            event: verdict.as_api_str(),
            body: &review_body,
            comments: &comments,
        },
    )
}

/// POST reply drafts serially. Returns `(posted, failed, first_error_message)`.
fn fan_out_replies(pr: &PrDetails, replies: &[GgrReply]) -> (usize, usize, Option<String>) {
    let mut posted_ats: Vec<String> = Vec::new();
    let mut first_error: Option<String> = None;
    for reply in replies {
        let body = format_comment_body(&reply.body, reply.severity);
        match crate::gh::post_reply(
            &pr.repo_name,
            pr.number,
            pr.hostname.as_deref(),
            &body,
            &reply.parent_comment_id,
        ) {
            Ok(()) => posted_ats.push(reply.created_at.clone()),
            Err(e) => {
                first_error = Some(format!(
                    "reply to {} failed: {}",
                    reply.parent_comment_id,
                    strip_controls(&e.to_string())
                ));
                break;
            }
        }
    }
    let posted = posted_ats.len();
    let failed = replies.len().saturating_sub(posted);
    (posted, failed, first_error)
}

/// Clear drafts and replies after a submit attempt.
///
/// Review drafts are always cleared when this is called (review was posted).
/// Replies are cleared based on how many were successfully posted.
fn clear_after_submit(
    coords: &PrCoords<'_>,
    commits: &[CommitEntry],
    replies_posted: usize,
    replies_failed: usize,
    all_replies: &[GgrReply],
) -> Result<()> {
    clear_review_drafts(coords, commits)?;
    let replies_file = replies_file_from_base(
        coords.base,
        coords.host,
        coords.owner,
        coords.repo,
        coords.pr_number,
    );
    if replies_failed == 0 {
        if replies_file.exists() {
            crate::draft::clear_replies(&replies_file)?;
        }
    } else if replies_posted > 0 {
        let posted_ats: std::collections::HashSet<&str> = all_replies[..replies_posted]
            .iter()
            .map(|r| r.created_at.as_str())
            .collect();
        delete_reply(&replies_file, |r| {
            posted_ats.contains(r.created_at.as_str())
        })?;
    }
    Ok(())
}

// ── submit orchestration ──────────────────────────────────────────────────────

/// Submit all local drafts as a single GitHub review plus reply fan-out.
///
/// Ordering per spec:
/// 1. Validate (COMMENT with zero drafts is rejected).
/// 2. POST the review. If it fails, return the error; all drafts stay on disk.
/// 3. Fan out replies serially. Stop at first failure.
/// 4. On full success: clear all drafts and replies.
///    On partial failure: clear review drafts; clear only the posted replies.
pub(crate) fn submit(pr: &PrDetails, verdict: Verdict, base: &Path) -> Result<SubmitOutcome> {
    let host = pr.hostname.as_deref().unwrap_or("github.com");
    let slug = pr.repo_name.as_str();
    let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));
    let coords = PrCoords {
        base,
        host,
        owner,
        repo,
        pr_number: pr.number,
    };

    let (all_drafts, all_replies) = collect_all_pr_drafts(&coords, &pr.commits)?;

    if matches!(verdict, Verdict::Comment) && all_drafts.is_empty() && all_replies.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason:
                "nothing to submit; use approve or request-changes to weigh in without comments"
                    .to_owned(),
        });
    }

    post_review_from_drafts(pr, verdict, &all_drafts)?;

    let (replies_posted, replies_failed, first_reply_error) = fan_out_replies(pr, &all_replies);

    clear_after_submit(
        &coords,
        &pr.commits,
        replies_posted,
        replies_failed,
        &all_replies,
    )?;

    let message = build_outcome_message(
        verdict,
        replies_posted,
        replies_failed,
        first_reply_error.as_deref(),
    );
    Ok(SubmitOutcome { message })
}

fn build_outcome_message(
    verdict: Verdict,
    replies_posted: usize,
    replies_failed: usize,
    first_error: Option<&str>,
) -> String {
    let verdict_str = match verdict {
        Verdict::Approve => "approved",
        Verdict::RequestChanges => "changes requested",
        Verdict::Comment => "review posted",
    };
    if replies_failed == 0 {
        if replies_posted == 0 {
            verdict_str.to_owned()
        } else {
            format!(
                "{verdict_str} + {replies_posted} repl{}",
                if replies_posted == 1 { "y" } else { "ies" }
            )
        }
    } else {
        let msg = first_error.unwrap_or("unknown error");
        format!(
            "{verdict_str}; {replies_posted} repl{} posted, {replies_failed} failed: {msg}",
            if replies_posted == 1 { "y" } else { "ies" }
        )
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{CommonParams, GgrDraft, LineAnchorParams};
    use crate::pr::CommitSha;

    fn make_commit(sha_char: char, title: &str) -> CommitEntry {
        CommitEntry {
            sha: CommitSha::try_from(sha_char.to_string().repeat(40).as_str()).unwrap(),
            short_sha: sha_char.to_string().repeat(8),
            title: title.to_owned(),
        }
    }

    fn common(body: &str, severity: Severity) -> CommonParams {
        CommonParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            body: body.to_owned(),
            severity,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn line_anchor(sha_char: char) -> LineAnchorParams {
        LineAnchorParams {
            commit_sha: CommitSha::try_from(sha_char.to_string().repeat(40).as_str()).unwrap(),
            file: "src/lib.rs".to_owned(),
            side: Side::New,
            old_line: None,
            new_line: Some(10),
            hunk_header: "@@ -1,3 +1,4 @@".to_owned(),
            target_text: "fn foo() {}".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
    }

    #[test]
    fn severity_marker_values() {
        assert_eq!(severity_marker(Severity::Required), "[REQUIRED]");
        assert_eq!(severity_marker(Severity::Suggestion), "[SUGGESTION]");
        assert_eq!(severity_marker(Severity::Note), "[NOTE]");
    }

    #[test]
    fn format_comment_body_prepends_marker() {
        let body = format_comment_body("fix this", Severity::Required);
        assert!(body.starts_with("[REQUIRED]\n\n"));
        assert!(body.contains("fix this"));
    }

    #[test]
    fn format_comment_body_strips_ansi_escapes() {
        let body = format_comment_body("\x1b[31mevil\x1b[0m", Severity::Note);
        // body has newlines (intended), but must not have ANSI escape sequences
        assert!(
            !body.contains('\x1b'),
            "ANSI escape must be stripped: {body:?}"
        );
        assert!(body.contains("evil"), "visible text must survive stripping");
    }

    #[test]
    fn build_review_body_empty_returns_empty() {
        assert_eq!(build_review_body(&[], &[], &[]), "");
    }

    #[test]
    fn build_review_body_pr_scope_only() {
        let draft = GgrDraft::new_pr(&common("overall note", Severity::Note)).unwrap();
        let body = build_review_body(&[&draft], &[], &[]);
        assert!(body.contains("[NOTE]"));
        assert!(body.contains("overall note"));
    }

    #[test]
    fn build_review_body_commit_scope_attribution() {
        let sha_char = 'a';
        let commit = make_commit(sha_char, "implement retry policy");
        let sha_str = sha_char.to_string().repeat(40);
        let draft =
            GgrDraft::new_commit(&common("split this", Severity::Suggestion), &sha_str).unwrap();
        let body = build_review_body(&[], &[&draft], &[commit]);
        assert!(body.contains("> Commit aaaaaaaa — \"implement retry policy\""));
        assert!(body.contains("> [SUGGESTION]"));
        assert!(body.contains("> split this"));
    }

    #[test]
    fn build_review_body_separates_multiple_with_hr() {
        let d1 = GgrDraft::new_pr(&common("first", Severity::Note)).unwrap();
        let d2 = GgrDraft::new_pr(&common("second", Severity::Note)).unwrap();
        let body = build_review_body(&[&d1, &d2], &[], &[]);
        assert!(body.contains("\n\n---\n\n"));
    }

    #[test]
    fn draft_to_line_comment_new_side() {
        let draft =
            GgrDraft::new_line(&common("fix", Severity::Required), &line_anchor('a')).unwrap();
        let comment = draft_to_line_comment(&draft).unwrap();
        assert_eq!(comment.side, "RIGHT");
        assert_eq!(comment.line, 10);
        assert!(comment.body.starts_with("[REQUIRED]"));
    }

    #[test]
    fn draft_to_line_comment_old_side() {
        let mut anchor = line_anchor('b');
        anchor.side = Side::Old;
        anchor.old_line = Some(5);
        anchor.new_line = None;
        let draft = GgrDraft::new_line(&common("fix", Severity::Note), &anchor).unwrap();
        let comment = draft_to_line_comment(&draft).unwrap();
        assert_eq!(comment.side, "LEFT");
        assert_eq!(comment.line, 5);
    }

    #[test]
    fn draft_to_line_comment_pr_scope_returns_none() {
        let draft = GgrDraft::new_pr(&common("body", Severity::Note)).unwrap();
        assert!(draft_to_line_comment(&draft).is_none());
    }

    #[test]
    fn verdict_api_strings() {
        assert_eq!(Verdict::Approve.as_api_str(), "APPROVE");
        assert_eq!(Verdict::RequestChanges.as_api_str(), "REQUEST_CHANGES");
        assert_eq!(Verdict::Comment.as_api_str(), "COMMENT");
    }

    #[test]
    fn build_outcome_message_no_replies() {
        assert_eq!(
            build_outcome_message(Verdict::Approve, 0, 0, None),
            "approved"
        );
        assert_eq!(
            build_outcome_message(Verdict::RequestChanges, 0, 0, None),
            "changes requested"
        );
        assert_eq!(
            build_outcome_message(Verdict::Comment, 0, 0, None),
            "review posted"
        );
    }

    #[test]
    fn build_outcome_message_with_posted_replies() {
        let msg = build_outcome_message(Verdict::Approve, 1, 0, None);
        assert!(msg.contains("approved"));
        assert!(msg.contains("1 reply"));

        let msg = build_outcome_message(Verdict::Comment, 3, 0, None);
        assert!(msg.contains("3 replies"));
    }

    #[test]
    fn build_outcome_message_partial_failure() {
        let msg =
            build_outcome_message(Verdict::Comment, 2, 1, Some("reply to 99 failed: gh error"));
        assert!(msg.contains("2 repl"));
        assert!(msg.contains("1 failed"));
        assert!(msg.contains("gh error"));
    }

    #[test]
    fn collect_all_pr_drafts_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let coords = PrCoords {
            base: dir.path(),
            host: "github.com",
            owner: "acme",
            repo: "widget",
            pr_number: 42,
        };
        let commits = vec![make_commit('a', "first")];
        let (drafts, replies) = collect_all_pr_drafts(&coords, &commits).unwrap();
        assert!(drafts.is_empty());
        assert!(replies.is_empty());
    }

    #[test]
    fn collect_all_pr_drafts_reads_commit_and_pr_files() {
        use crate::draft::{append_draft, drafts_dir_from_base};

        let dir = tempfile::tempdir().unwrap();
        let commit = make_commit('a', "first commit");
        let coords = PrCoords {
            base: dir.path(),
            host: "github.com",
            owner: "acme",
            repo: "widget",
            pr_number: 42,
        };

        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 42);

        // Write a commit-scoped draft
        let sha_str = 'a'.to_string().repeat(40);
        let commit_draft =
            GgrDraft::new_commit(&common("commit note", Severity::Note), &sha_str).unwrap();
        let commit_path = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_path, &commit_draft).unwrap();

        // Write a PR-scoped draft
        let pr_draft = GgrDraft::new_pr(&common("overall", Severity::Note)).unwrap();
        let pr_path = drafts_dir.join("_pr.jsonl");
        append_draft(&pr_path, &pr_draft).unwrap();

        let (drafts, _) = collect_all_pr_drafts(&coords, &[commit]).unwrap();
        assert_eq!(drafts.len(), 2);
    }

    #[test]
    fn clear_review_drafts_empties_commit_and_pr_files() {
        use crate::draft::{append_draft, drafts_dir_from_base, list_drafts};

        let dir = tempfile::tempdir().unwrap();
        let commit = make_commit('b', "commit");
        let coords = PrCoords {
            base: dir.path(),
            host: "github.com",
            owner: "acme",
            repo: "widget",
            pr_number: 7,
        };
        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 7);
        let sha_str = 'b'.to_string().repeat(40);
        let d = GgrDraft::new_commit(&common("note", Severity::Note), &sha_str).unwrap();
        let commit_path = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_path, &d).unwrap();

        clear_review_drafts(&coords, &[commit]).unwrap();

        assert_eq!(list_drafts(&commit_path).unwrap().len(), 0);
    }

    #[test]
    fn clear_after_submit_full_success_clears_replies() {
        use crate::draft::{append_reply, list_replies, replies_file_from_base, ReplyParams};

        let dir = tempfile::tempdir().unwrap();
        let coords = PrCoords {
            base: dir.path(),
            host: "github.com",
            owner: "acme",
            repo: "widget",
            pr_number: 42,
        };
        let replies_file = replies_file_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let r = GgrReply::new(&ReplyParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            parent_comment_id: "1".to_owned(),
            body: "ok".to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
        append_reply(&replies_file, &r).unwrap();
        let all_replies = list_replies(&replies_file).unwrap();
        clear_after_submit(&coords, &[], 1, 0, &all_replies).unwrap();
        assert_eq!(list_replies(&replies_file).unwrap().len(), 0);
    }

    #[test]
    fn clear_after_submit_partial_keeps_unposted_replies() {
        use crate::draft::{append_reply, list_replies, replies_file_from_base, ReplyParams};

        let dir = tempfile::tempdir().unwrap();
        let coords = PrCoords {
            base: dir.path(),
            host: "github.com",
            owner: "acme",
            repo: "widget",
            pr_number: 43,
        };
        let replies_file = replies_file_from_base(dir.path(), "github.com", "acme", "widget", 43);
        for (id, created) in [("1", "2026-01-01T00:00:00Z"), ("2", "2026-01-02T00:00:00Z")] {
            let r = GgrReply::new(&ReplyParams {
                host: "github.com".to_owned(),
                owner: "acme".to_owned(),
                repo: "widget".to_owned(),
                pr_number: 43,
                parent_comment_id: id.to_owned(),
                body: "body".to_owned(),
                severity: Severity::Note,
                created_at: created.to_owned(),
            })
            .unwrap();
            append_reply(&replies_file, &r).unwrap();
        }
        let all_replies = list_replies(&replies_file).unwrap();
        // 1 posted, 1 failed — only the second reply should remain
        clear_after_submit(&coords, &[], 1, 1, &all_replies).unwrap();
        let remaining = list_replies(&replies_file).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].parent_comment_id, "2");
    }

    #[test]
    fn submit_comment_with_no_drafts_returns_error() {
        use crate::pr::{PrDetails, RepoName};

        let dir = tempfile::tempdir().unwrap();
        let pr = PrDetails {
            number: 1,
            title: "t".to_owned(),
            body: String::new(),
            comments: vec![],
            repo_name: RepoName::try_from("acme/widget").unwrap(),
            hostname: None,
            commits: vec![],
            review_threads: vec![],
        };
        let err = submit(&pr, Verdict::Comment, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("nothing to submit"),
            "wrong error: {err}"
        );
    }
}
