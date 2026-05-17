//! Re-anchoring pass for `ggr`.
//!
//! Runs on every open and on the `R` mid-session refresh. Reads all local
//! draft and reply files for a PR, compares their anchors against the current
//! PR state from GitHub, and rewrites each file with updated `status` /
//! `mismatch_reason` fields.
//!
//! Outcomes per draft kind:
//!
//! - **Line-scoped**: fetch the commit diff, run `match_anchor`. If the commit
//!   SHA is gone, try a subject-based successor first.
//! - **Commit-scoped**: SHA in PR → pending; gone → subject successor or stale.
//! - **PR-scoped**: never stale (the `pr_number` never changes).
//! - **Reply**: `parent_comment_id` still present in threads → pending; gone →
//!   stale with `"parent comment deleted"`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use local_review_core::anchoring::{match_anchor, AnchorOutcome};
use local_review_core::comment::{LineAnchor, MismatchReason};

use crate::draft::{
    self, drafts_dir_from_base, list_drafts, list_replies, replies_file_from_base, DraftStatus,
    GgrAnchor, GgrDraft, GgrReply,
};
use crate::pr::{CommitEntry, CommitSha, PrDetails, RepoName};

// ── public entry point ────────────────────────────────────────────────────────

/// Re-anchor all local drafts and replies for `pr` against the current GitHub
/// state. Rewrites each JSONL file atomically. Returns the number of stale
/// drafts found.
///
/// Errors from individual commits (e.g. a transient diff fetch failure) are
/// silently skipped — the draft retains its existing status so a temporary
/// network blip does not falsely mark a draft stale.
pub(crate) fn reanchor_all(pr: &PrDetails, base: &Path) -> usize {
    let host = pr.hostname.as_deref().unwrap_or("github.com");
    let slug = pr.repo_name.as_str();
    let (owner, repo) = slug.split_once('/').unwrap_or(("_", "_"));

    let current_shas: HashSet<&str> = pr.commits.iter().map(|c| c.sha.as_str()).collect();
    let subject_map = build_subject_map(&pr.commits);
    let thread_ids: HashSet<String> = pr
        .review_threads
        .iter()
        .map(|t| t.root.id.to_string())
        .collect();

    let drafts_dir = drafts_dir_from_base(base, host, owner, repo, pr.number);
    let mut stale_count = 0usize;

    let Ok(entries) = std::fs::read_dir(&drafts_dir) else {
        return 0; // drafts dir doesn't exist yet; nothing to do
    };

    let mut commit_paths: Vec<(String, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str())?;
            if ext != "jsonl" {
                return None;
            }
            let stem = p.file_stem()?.to_str()?.to_owned();
            if stem.starts_with('_') {
                return None;
            }
            Some((stem, p))
        })
        .collect();
    commit_paths.sort_by(|a, b| a.0.cmp(&b.0));

    for (sha_str, path) in &commit_paths {
        let Ok(drafts) = list_drafts(path) else {
            continue;
        };
        if drafts.is_empty() {
            continue;
        }

        let updated = if current_shas.contains(sha_str.as_str()) {
            let Some(commit) = pr.commits.iter().find(|c| c.sha.as_str() == sha_str) else {
                continue; // should not happen: sha_str is in current_shas
            };
            reanchor_commit_drafts(drafts, commit, &pr.repo_name, pr.hostname.as_deref())
        } else {
            let old_subject =
                crate::gh::fetch_commit_subject(&pr.repo_name, sha_str, pr.hostname.as_deref());
            reanchor_orphaned_drafts(
                drafts,
                old_subject.as_deref(),
                &OrphanedCtx {
                    subject_map: &subject_map,
                    repo_name: &pr.repo_name,
                    hostname: pr.hostname.as_deref(),
                },
            )
        };

        stale_count += updated
            .iter()
            .filter(|d| d.status == Some(DraftStatus::Stale))
            .count();
        let _ = draft::write_drafts_to_path(path, &updated);
    }

    let replies_file = replies_file_from_base(base, host, owner, repo, pr.number);
    if replies_file.exists() {
        if let Ok(replies) = list_replies(&replies_file) {
            let updated: Vec<GgrReply> = replies
                .into_iter()
                .map(|r| reanchor_reply(r, &thread_ids))
                .collect();
            stale_count += updated
                .iter()
                .filter(|r| r.status == Some(DraftStatus::Stale))
                .count();
            let _ = draft::write_replies_to_path(&replies_file, &updated);
        }
    }

    stale_count
}

// ── per-SHA helpers ───────────────────────────────────────────────────────────

fn reanchor_commit_drafts(
    drafts: Vec<GgrDraft>,
    commit: &CommitEntry,
    repo_name: &RepoName,
    hostname: Option<&str>,
) -> Vec<GgrDraft> {
    let diff = crate::gh::fetch_commit_diff(repo_name, &commit.sha, hostname).ok();
    drafts
        .into_iter()
        .map(|mut d| {
            match &d.anchor {
                GgrAnchor::Pr | GgrAnchor::Commit { .. } => {
                    d.status = Some(DraftStatus::Pending);
                    d.mismatch_reason = None;
                }
                GgrAnchor::Line {
                    file,
                    side,
                    old_line,
                    new_line,
                    hunk_header,
                    target_text,
                    context_before,
                    context_after,
                    ..
                } => {
                    if let Some(diff) = &diff {
                        let anchor = LineAnchor {
                            file: std::path::PathBuf::from(file),
                            side: *side,
                            old_line: *old_line,
                            new_line: *new_line,
                            hunk_header: hunk_header.clone(),
                            target_text: target_text.clone(),
                            context_before: context_before.clone(),
                            context_after: context_after.clone(),
                        };
                        apply_anchor_outcome(&mut d, match_anchor(&anchor, diff));
                    }
                    // Diff fetch failed — leave status unchanged.
                }
            }
            d
        })
        .collect()
}

/// Context shared across all orphaned drafts for the same old commit SHA.
struct OrphanedCtx<'a> {
    subject_map: &'a HashMap<&'a str, &'a CommitEntry>,
    repo_name: &'a RepoName,
    hostname: Option<&'a str>,
}

fn reanchor_orphaned_drafts(
    drafts: Vec<GgrDraft>,
    old_subject: Option<&str>,
    ctx: &OrphanedCtx<'_>,
) -> Vec<GgrDraft> {
    let successor = old_subject.and_then(|s| ctx.subject_map.get(s).copied());
    let successor_diff = successor
        .and_then(|c| crate::gh::fetch_commit_diff(ctx.repo_name, &c.sha, ctx.hostname).ok());

    drafts
        .into_iter()
        .map(|mut d| {
            match &d.anchor {
                GgrAnchor::Pr => {
                    d.status = Some(DraftStatus::Pending);
                    d.mismatch_reason = None;
                }
                GgrAnchor::Commit { .. } => {
                    if let Some(c) = successor {
                        if let Ok(new_sha) = CommitSha::try_from(c.sha.as_str()) {
                            d.anchor = GgrAnchor::Commit {
                                commit_sha: new_sha,
                            };
                            d.status = Some(DraftStatus::Pending);
                            d.mismatch_reason = None;
                        } else {
                            mark_stale_commit_not_in_pr(&mut d);
                        }
                    } else {
                        mark_stale_commit_not_in_pr(&mut d);
                    }
                }
                GgrAnchor::Line { .. } => {
                    reanchor_orphaned_line(&mut d, successor, successor_diff.as_ref());
                }
            }
            d
        })
        .collect()
}

fn reanchor_orphaned_line(
    d: &mut GgrDraft,
    successor: Option<&CommitEntry>,
    successor_diff: Option<&local_review_core::diff::Diff>,
) {
    let GgrAnchor::Line {
        file,
        side,
        old_line,
        new_line,
        hunk_header,
        target_text,
        context_before,
        context_after,
        ..
    } = &d.anchor
    else {
        return;
    };
    if let (Some(new_commit), Some(diff)) = (successor, successor_diff) {
        let anchor = LineAnchor {
            file: std::path::PathBuf::from(file),
            side: *side,
            old_line: *old_line,
            new_line: *new_line,
            hunk_header: hunk_header.clone(),
            target_text: target_text.clone(),
            context_before: context_before.clone(),
            context_after: context_after.clone(),
        };
        match match_anchor(&anchor, diff) {
            AnchorOutcome::ReAnchored(new_anchor) => {
                if let Ok(new_sha) = CommitSha::try_from(new_commit.sha.as_str()) {
                    d.anchor = GgrAnchor::Line {
                        commit_sha: new_sha,
                        file: new_anchor.file.to_string_lossy().into_owned(),
                        side: new_anchor.side,
                        old_line: new_anchor.old_line,
                        new_line: new_anchor.new_line,
                        hunk_header: new_anchor.hunk_header,
                        target_text: new_anchor.target_text,
                        context_before: new_anchor.context_before,
                        context_after: new_anchor.context_after,
                    };
                    d.status = Some(DraftStatus::Pending);
                    d.mismatch_reason = None;
                } else {
                    mark_stale_commit_not_in_pr(d);
                }
            }
            AnchorOutcome::Stale(reason) => {
                d.status = Some(DraftStatus::Stale);
                d.mismatch_reason = Some(mismatch_reason_str(reason));
            }
        }
    } else {
        mark_stale_commit_not_in_pr(d);
    }
}

fn reanchor_reply(mut reply: GgrReply, thread_ids: &HashSet<String>) -> GgrReply {
    if thread_ids.contains(&reply.parent_comment_id) {
        reply.status = Some(DraftStatus::Pending);
        reply.mismatch_reason = None;
    } else {
        reply.status = Some(DraftStatus::Stale);
        reply.mismatch_reason = Some("parent comment deleted".to_owned());
    }
    reply
}

// ── small helpers ─────────────────────────────────────────────────────────────

fn build_subject_map(commits: &[CommitEntry]) -> HashMap<&str, &CommitEntry> {
    let mut map: HashMap<&str, &CommitEntry> = HashMap::new();
    let mut dupes: HashSet<&str> = HashSet::new();
    for c in commits {
        let title = c.title.as_str();
        if map.contains_key(title) {
            dupes.insert(title);
        } else {
            map.insert(title, c);
        }
    }
    for dupe in dupes {
        map.remove(dupe);
    }
    map
}

fn mark_stale_commit_not_in_pr(d: &mut GgrDraft) {
    d.status = Some(DraftStatus::Stale);
    d.mismatch_reason = Some("commit not in PR".to_owned());
}

fn mismatch_reason_str(reason: MismatchReason) -> String {
    use local_review_core::util::strip_controls;
    strip_controls(
        &serde_json::to_string(&reason).unwrap_or_else(|_| "\"anchor not found\"".to_owned()),
    )
    .trim_matches('"')
    .to_owned()
}

fn apply_anchor_outcome(d: &mut GgrDraft, outcome: AnchorOutcome<LineAnchor>) {
    match outcome {
        AnchorOutcome::ReAnchored(new_anchor) => {
            if let GgrAnchor::Line {
                old_line, new_line, ..
            } = &mut d.anchor
            {
                *old_line = new_anchor.old_line;
                *new_line = new_anchor.new_line;
            }
            d.status = Some(DraftStatus::Pending);
            d.mismatch_reason = None;
        }
        AnchorOutcome::Stale(reason) => {
            d.status = Some(DraftStatus::Stale);
            d.mismatch_reason = Some(mismatch_reason_str(reason));
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{append_draft, append_reply, CommonParams, GgrDraft, ReplyParams};
    use crate::pr::{CommitSha, PrDetails, RepoName, ReviewThread, ThreadComment};
    use local_review_core::Severity;

    fn make_pr(commits: Vec<CommitEntry>, thread_ids: Vec<u64>) -> PrDetails {
        let review_threads = thread_ids
            .into_iter()
            .map(|id| ReviewThread {
                path: "src/foo.rs".to_owned(),
                position: Some(1),
                original_commit_id: CommitSha::try_from("a".repeat(40).as_str()).unwrap(),
                root: ThreadComment {
                    id,
                    author: "reviewer".to_owned(),
                    created_at: "2026-01-01T00:00:00Z".to_owned(),
                    body: "body".to_owned(),
                },
                replies: vec![],
                line: Some(1),
                original_line: None,
                diff_side: None,
                severity: Severity::Note,
            })
            .collect();
        PrDetails {
            number: 42,
            title: "PR".to_owned(),
            body: String::new(),
            comments: vec![],
            repo_name: RepoName::try_from("acme/widget").unwrap(),
            hostname: None,
            commits,
            review_threads,
        }
    }

    fn make_commit(sha_char: char, title: &str) -> CommitEntry {
        CommitEntry {
            sha: CommitSha::try_from(sha_char.to_string().repeat(40).as_str()).unwrap(),
            short_sha: sha_char.to_string().repeat(8),
            title: title.to_owned(),
        }
    }

    fn common() -> CommonParams {
        CommonParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            body: "body".to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn build_subject_map_deduplicates() {
        let commits = vec![
            make_commit('a', "unique A"),
            make_commit('b', "duplicate"),
            make_commit('c', "duplicate"),
        ];
        let map = build_subject_map(&commits);
        assert!(map.contains_key("unique A"));
        assert!(
            !map.contains_key("duplicate"),
            "duplicates must be excluded"
        );
    }

    #[test]
    fn build_subject_map_single_entry() {
        let commits = vec![make_commit('a', "only one")];
        let map = build_subject_map(&commits);
        assert_eq!(map.len(), 1);
        assert_eq!(map["only one"].short_sha, "aaaaaaaa");
    }

    #[test]
    fn mismatch_reason_str_known_variants() {
        use local_review_core::comment::MismatchReason;
        assert_eq!(
            mismatch_reason_str(MismatchReason::AnchorNotFound),
            "anchor not found"
        );
        assert_eq!(
            mismatch_reason_str(MismatchReason::FileNotInDiff),
            "file not in diff"
        );
    }

    #[test]
    fn reanchor_all_empty_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let pr = make_pr(vec![make_commit('a', "first")], vec![]);
        assert_eq!(reanchor_all(&pr, dir.path()), 0);
    }

    #[test]
    fn reanchor_all_reply_with_matching_thread_is_pending() {
        use crate::draft::replies_file_from_base;

        let dir = tempfile::tempdir().unwrap();
        let pr = make_pr(vec![], vec![99]);

        let replies_file = replies_file_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let r = GgrReply::new(&ReplyParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            parent_comment_id: "99".to_owned(),
            body: "looks good".to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
        append_reply(&replies_file, &r).unwrap();

        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 0);

        let loaded = list_replies(&replies_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Pending));
    }

    #[test]
    fn reanchor_all_reply_with_deleted_parent_is_stale() {
        use crate::draft::replies_file_from_base;

        let dir = tempfile::tempdir().unwrap();
        let pr = make_pr(vec![], vec![/* no threads */]);

        let replies_file = replies_file_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let r = GgrReply::new(&ReplyParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            parent_comment_id: "999".to_owned(),
            body: "reply body".to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        })
        .unwrap();
        append_reply(&replies_file, &r).unwrap();

        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 1);

        let loaded = list_replies(&replies_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Stale));
        assert_eq!(
            loaded[0].mismatch_reason.as_deref(),
            Some("parent comment deleted")
        );
    }

    #[test]
    fn reanchor_all_commit_scope_draft_with_current_sha_is_pending() {
        use crate::draft::drafts_dir_from_base;

        let dir = tempfile::tempdir().unwrap();
        let sha_char = 'a';
        let sha_str = sha_char.to_string().repeat(40);
        let commit = make_commit(sha_char, "implement feature");
        let pr = make_pr(vec![commit], vec![]);

        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let commit_draft = GgrDraft::new_commit(&common(), &sha_str).unwrap();
        let commit_file = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_file, &commit_draft).unwrap();

        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 0);

        let loaded = list_drafts(&commit_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Pending));
    }

    #[test]
    fn reanchor_all_commit_scope_orphaned_no_successor_is_stale() {
        use crate::draft::drafts_dir_from_base;

        let dir = tempfile::tempdir().unwrap();
        let sha_char = 'b';
        let sha_str = sha_char.to_string().repeat(40);
        // PR has different commits — sha_char SHA is not present.
        let pr = make_pr(vec![make_commit('c', "different commit")], vec![]);

        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let commit_draft = GgrDraft::new_commit(&common(), &sha_str).unwrap();
        let commit_file = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_file, &commit_draft).unwrap();

        // fetch_commit_subject will fail (no gh binary in tests) → no subject match.
        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 1);

        let loaded = list_drafts(&commit_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Stale));
    }

    #[test]
    fn reanchor_all_line_scope_orphaned_no_successor_is_stale() {
        use crate::draft::{drafts_dir_from_base, LineAnchorParams};
        use local_review_core::comment::Side;

        let dir = tempfile::tempdir().unwrap();
        let sha_char = 'c';
        let sha_str = sha_char.to_string().repeat(40);
        // PR has no matching commit and no commit with the same subject.
        let pr = make_pr(vec![make_commit('d', "other commit")], vec![]);

        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 42);
        let commit_sha = crate::pr::CommitSha::try_from(sha_str.as_str()).unwrap();
        let line_draft = GgrDraft::new_line(
            &common(),
            &LineAnchorParams {
                commit_sha,
                file: "src/lib.rs".to_owned(),
                side: Side::New,
                old_line: None,
                new_line: Some(1),
                hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
                target_text: "fn foo() {}".to_owned(),
                context_before: vec![],
                context_after: vec![],
            },
        )
        .unwrap();
        let commit_file = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_file, &line_draft).unwrap();

        // No gh binary → fetch_commit_subject fails → no successor → stale.
        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 1);

        let loaded = list_drafts(&commit_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Stale));
        assert_eq!(
            loaded[0].mismatch_reason.as_deref(),
            Some("commit not in PR")
        );
    }

    #[test]
    fn reanchor_all_pr_scope_draft_in_commit_file_is_pending() {
        use crate::draft::drafts_dir_from_base;

        let dir = tempfile::tempdir().unwrap();
        let sha_char = 'e';
        let sha_str = sha_char.to_string().repeat(40);
        let commit = make_commit(sha_char, "commit e");
        let pr = make_pr(vec![commit], vec![]);

        let drafts_dir = drafts_dir_from_base(dir.path(), "github.com", "acme", "widget", 42);
        // Place a PR-scope draft inside a commit SHA file (degenerate but handled).
        let pr_draft_in_commit_file = GgrDraft::new_pr(&common()).unwrap();
        let commit_file = drafts_dir.join(format!("{sha_str}.jsonl"));
        append_draft(&commit_file, &pr_draft_in_commit_file).unwrap();

        let stale = reanchor_all(&pr, dir.path());
        assert_eq!(stale, 0);

        let loaded = list_drafts(&commit_file).unwrap();
        assert_eq!(loaded[0].status, Some(DraftStatus::Pending));
    }
}
