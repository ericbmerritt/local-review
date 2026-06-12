pub use local_review_core::anchoring::{match_anchor, match_description_anchor, AnchorOutcome};

use crate::comment::{Anchor, Comment, DescriptionAnchor, LineAnchor, MismatchReason, Status};
use crate::diff::Diff;

/// Reconcile a saved comment's anchor against the current diff and description.
///
/// Returns `None` when the comment is already correct (no write needed).
/// Returns `Some(updated)` when the anchor location, status, or mismatch
/// reason needs to change.
///
/// Callers that have no description text should pass `""`, which causes
/// description anchors to return `Stale(AnchorNotFound)`.
pub fn reanchor_comment(comment: &Comment, diff: &Diff, description: &str) -> Option<Comment> {
    match &comment.anchor {
        Anchor::Line {
            change_id,
            location,
        } => reanchor_line(comment, change_id, location, diff),
        Anchor::Description {
            change_id,
            location,
        } => reanchor_description(comment, change_id, location, description),
        Anchor::Change { .. } | Anchor::Stack { .. } => None,
    }
}

fn reanchor_line(
    comment: &Comment,
    change_id: &crate::change_id::ChangeId,
    location: &LineAnchor,
    diff: &Diff,
) -> Option<Comment> {
    match match_anchor(location, diff) {
        AnchorOutcome::ReAnchored(new_anchor) => {
            if !needs_reanchor_update(comment, &new_anchor != location) {
                return None;
            }
            Some(Comment {
                anchor: Anchor::Line {
                    change_id: change_id.clone(),
                    location: new_anchor,
                },
                status: Some(Status::Pending),
                mismatch_reason: None,
                ..comment.clone()
            })
        }
        AnchorOutcome::Stale(reason) => stale_update(comment, reason),
    }
}

fn reanchor_description(
    comment: &Comment,
    change_id: &crate::change_id::ChangeId,
    location: &DescriptionAnchor,
    description: &str,
) -> Option<Comment> {
    match match_description_anchor(location, description) {
        AnchorOutcome::ReAnchored(new_anchor) => {
            if !needs_reanchor_update(comment, &new_anchor != location) {
                return None;
            }
            Some(Comment {
                anchor: Anchor::Description {
                    change_id: change_id.clone(),
                    location: new_anchor,
                },
                status: Some(Status::Pending),
                mismatch_reason: None,
                ..comment.clone()
            })
        }
        AnchorOutcome::Stale(reason) => stale_update(comment, reason),
    }
}

/// Whether a re-anchored comment needs its on-disk record rewritten. Skips the
/// write when the anchor didn't move and the comment was already pending; we
/// only need to touch the record when something actually changed (location
/// shifted, transitioning out of stale, or status was unset on a v1 record).
fn needs_reanchor_update(comment: &Comment, anchor_moved: bool) -> bool {
    let was_stale = comment.status == Some(Status::Stale);
    let status_unset = comment.status.is_none();
    anchor_moved || was_stale || status_unset
}

fn stale_update(comment: &Comment, reason: MismatchReason) -> Option<Comment> {
    let already_stale_with_same_reason =
        comment.status == Some(Status::Stale) && comment.mismatch_reason == Some(reason);
    if already_stale_with_same_reason {
        return None;
    }
    Some(Comment {
        status: Some(Status::Stale),
        mismatch_reason: Some(reason),
        ..comment.clone()
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::ChangeId;
    use crate::comment::{
        Anchor, Comment, DescriptionAnchor, LineAnchor, MismatchReason, SchemaVersion, Severity,
        Side, Status,
    };
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};

    fn make_line(kind: LineKind, text: &str, src: Option<u32>, tgt: Option<u32>) -> Line {
        Line {
            kind,
            text: text.to_owned(),
            source_line: src,
            target_line: tgt,
        }
    }

    fn added_line(text: &str, tgt: u32) -> Line {
        make_line(LineKind::Added, text, None, Some(tgt))
    }

    fn make_hunk(header: &str, fn_ctx: Option<&str>, lines: Vec<Line>) -> Hunk {
        let source_length =
            u32::try_from(lines.iter().filter(|l| l.source_line.is_some()).count()).unwrap();
        let target_length =
            u32::try_from(lines.iter().filter(|l| l.target_line.is_some()).count()).unwrap();
        Hunk {
            header: header.to_owned(),
            function_context: fn_ctx.map(str::to_owned),
            source_start: 1,
            source_length,
            target_start: 1,
            target_length,
            lines,
        }
    }

    fn make_diff(files: Vec<DiffFile>) -> Diff {
        Diff { files }
    }

    fn modified_file(path: &str, hunks: Vec<Hunk>) -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from(path),
            hunks,
        }
    }

    fn make_comment(
        anchor: Anchor,
        status: Option<Status>,
        reason: Option<MismatchReason>,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor,
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "test comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:00:00 UTC),
            updated_at: None,
            status,
            mismatch_reason: reason,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn line_anchor(file: &str, target: &str, new_line: u32) -> LineAnchor {
        LineAnchor {
            file: PathBuf::from(file),
            side: Side::New,
            old_line: None,
            new_line: Some(new_line),
            hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
            target_text: target.to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
    }

    fn change_id() -> ChangeId {
        ChangeId::parse("abc12345").unwrap()
    }

    #[test]
    fn reanchor_pending_exact_match_unchanged_location_returns_none() {
        let anchor = LineAnchor {
            file: PathBuf::from("foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@ -0,0 +1,1 @@".to_owned(),
            target_text: "target".to_owned(),
            context_before: vec![],
            context_after: vec![],
        };
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor,
            },
            Some(Status::Pending),
            None,
        );
        let hunk = make_hunk("@@ -0,0 +1,1 @@", None, vec![added_line("target", 1)]);
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        assert!(reanchor_comment(&comment, &diff, "").is_none());
    }

    #[test]
    fn reanchor_pending_exact_match_moved_location_returns_updated_anchor() {
        let anchor = LineAnchor {
            file: PathBuf::from("foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
            target_text: "target".to_owned(),
            context_before: vec!["before".to_owned()],
            context_after: vec!["after".to_owned()],
        };
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor,
            },
            Some(Status::Pending),
            None,
        );
        let hunk = make_hunk(
            "@@ -5,3 +5,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "before", Some(5), Some(5)),
                make_line(LineKind::Context, "target", Some(6), Some(6)),
                make_line(LineKind::Context, "after", Some(7), Some(7)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let updated =
            reanchor_comment(&comment, &diff, "").expect("should return Some for moved anchor");
        let Anchor::Line { location, .. } = &updated.anchor else {
            panic!("expected Line anchor");
        };
        assert_eq!(location.new_line, Some(6));
        assert_eq!(updated.status, Some(Status::Pending));
        assert!(updated.mismatch_reason.is_none());
    }

    #[test]
    fn reanchor_pending_fuzzy_match_returns_stale_with_reason() {
        let anchor = LineAnchor {
            file: PathBuf::from("foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
            target_text: "old body".to_owned(),
            context_before: vec!["before".to_owned()],
            context_after: vec!["after".to_owned()],
        };
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor,
            },
            Some(Status::Pending),
            None,
        );
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "before", Some(1), Some(1)),
                make_line(LineKind::Context, "new body", Some(2), Some(2)),
                make_line(LineKind::Context, "after", Some(3), Some(3)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let updated =
            reanchor_comment(&comment, &diff, "").expect("should return Some when going stale");
        assert_eq!(updated.status, Some(Status::Stale));
        assert_eq!(
            updated.mismatch_reason,
            Some(MismatchReason::TargetTextChanged)
        );
    }

    #[test]
    fn reanchor_stale_now_exact_match_returns_pending() {
        let anchor = line_anchor("foo.rs", "target", 2);
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor.clone(),
            },
            Some(Status::Stale),
            Some(MismatchReason::TargetTextChanged),
        );
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "before", Some(1), Some(1)),
                make_line(LineKind::Context, "target", Some(2), Some(2)),
                make_line(LineKind::Context, "after", Some(3), Some(3)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let updated =
            reanchor_comment(&comment, &diff, "").expect("stale->pending should return Some");
        assert_eq!(updated.status, Some(Status::Pending));
        assert!(updated.mismatch_reason.is_none());
    }

    #[test]
    fn reanchor_stale_same_reason_returns_none() {
        let anchor = LineAnchor {
            file: PathBuf::from("foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
            target_text: "old body".to_owned(),
            context_before: vec!["before".to_owned()],
            context_after: vec!["after".to_owned()],
        };
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor,
            },
            Some(Status::Stale),
            Some(MismatchReason::TargetTextChanged),
        );
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "before", Some(1), Some(1)),
                make_line(LineKind::Context, "new body", Some(2), Some(2)),
                make_line(LineKind::Context, "after", Some(3), Some(3)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        assert!(reanchor_comment(&comment, &diff, "").is_none());
    }

    #[test]
    fn reanchor_stale_reason_changes_returns_updated_reason() {
        let anchor = LineAnchor {
            file: PathBuf::from("foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@".to_owned(),
            target_text: "target".to_owned(),
            context_before: vec!["current_before".to_owned()],
            context_after: vec!["original_after".to_owned()],
        };
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor,
            },
            Some(Status::Stale),
            Some(MismatchReason::ContextBeforeChanged),
        );
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "current_before", Some(1), Some(1)),
                make_line(LineKind::Context, "target", Some(2), Some(2)),
                make_line(LineKind::Context, "new_after", Some(3), Some(3)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let updated =
            reanchor_comment(&comment, &diff, "").expect("reason change should return Some");
        assert_eq!(updated.status, Some(Status::Stale));
        assert_eq!(
            updated.mismatch_reason,
            Some(MismatchReason::ContextAfterChanged)
        );
    }

    #[test]
    fn reanchor_change_scoped_comment_returns_none() {
        let comment = make_comment(
            Anchor::Change {
                change_id: change_id(),
            },
            Some(Status::Pending),
            None,
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![])]);
        assert!(reanchor_comment(&comment, &diff, "").is_none());
    }

    #[test]
    fn reanchor_stack_scoped_comment_returns_none() {
        use crate::stack::RevsetHash;
        let comment = make_comment(
            Anchor::Stack {
                revset_hash: RevsetHash::from_revset("@"),
            },
            None,
            None,
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![])]);
        assert!(reanchor_comment(&comment, &diff, "").is_none());
    }

    #[test]
    fn reanchor_none_status_exact_match_returns_pending() {
        let anchor = line_anchor("foo.rs", "target", 2);
        let comment = make_comment(
            Anchor::Line {
                change_id: change_id(),
                location: anchor.clone(),
            },
            None,
            None,
        );
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                make_line(LineKind::Context, "x", Some(1), Some(1)),
                make_line(LineKind::Context, "target", Some(2), Some(2)),
                make_line(LineKind::Context, "y", Some(3), Some(3)),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let updated = reanchor_comment(&comment, &diff, "")
            .expect("legacy no-status comment with exact match should return pending");
        assert_eq!(updated.status, Some(Status::Pending));
        assert!(updated.mismatch_reason.is_none());
    }

    fn make_desc_anchor(
        target: &str,
        display_line: Option<u32>,
        before: Vec<&str>,
        after: Vec<&str>,
    ) -> DescriptionAnchor {
        DescriptionAnchor {
            display_line,
            target_text: target.to_owned(),
            context_before: before.into_iter().map(str::to_owned).collect(),
            context_after: after.into_iter().map(str::to_owned).collect(),
        }
    }

    fn desc_comment(
        anchor: DescriptionAnchor,
        status: Option<Status>,
        reason: Option<MismatchReason>,
    ) -> Comment {
        make_comment(
            Anchor::Description {
                change_id: change_id(),
                location: anchor,
            },
            status,
            reason,
        )
    }

    fn empty_diff() -> Diff {
        make_diff(vec![])
    }

    #[test]
    fn reanchor_pending_desc_exact_match_unchanged_returns_none() {
        let anchor = make_desc_anchor("target", Some(1), vec![], vec![]);
        let comment = desc_comment(anchor, Some(Status::Pending), None);
        let description = "target";
        let diff = empty_diff();
        assert!(reanchor_comment(&comment, &diff, description).is_none());
    }

    #[test]
    fn reanchor_pending_desc_exact_match_new_line_returns_updated() {
        let anchor = make_desc_anchor("target", Some(1), vec![], vec!["second"]);
        let comment = desc_comment(anchor, Some(Status::Pending), None);
        let description = "preamble\ntarget\nsecond";
        let diff = empty_diff();
        let updated =
            reanchor_comment(&comment, &diff, description).expect("moved line should return Some");
        let Anchor::Description { location, .. } = &updated.anchor else {
            panic!("expected Description anchor");
        };
        assert_eq!(location.display_line, Some(2));
        assert_eq!(updated.status, Some(Status::Pending));
        assert!(updated.mismatch_reason.is_none());
    }

    #[test]
    fn reanchor_pending_desc_fuzzy_returns_stale() {
        let anchor = make_desc_anchor("old body", Some(2), vec!["before"], vec!["after"]);
        let comment = desc_comment(anchor, Some(Status::Pending), None);
        let description = "before\nnew body\nafter";
        let diff = empty_diff();
        let updated = reanchor_comment(&comment, &diff, description)
            .expect("fuzzy mismatch should return Some");
        assert_eq!(updated.status, Some(Status::Stale));
        assert_eq!(
            updated.mismatch_reason,
            Some(MismatchReason::TargetTextChanged)
        );
    }

    #[test]
    fn reanchor_stale_desc_now_exact_match_returns_pending() {
        let anchor = make_desc_anchor("target", Some(1), vec![], vec![]);
        let comment = desc_comment(
            anchor,
            Some(Status::Stale),
            Some(MismatchReason::TargetTextChanged),
        );
        let description = "target";
        let diff = empty_diff();
        let updated = reanchor_comment(&comment, &diff, description)
            .expect("stale->pending should return Some");
        assert_eq!(updated.status, Some(Status::Pending));
        assert!(updated.mismatch_reason.is_none());
    }

    #[test]
    fn reanchor_stale_desc_same_reason_returns_none() {
        let anchor = make_desc_anchor("old body", Some(2), vec!["before"], vec!["after"]);
        let comment = desc_comment(
            anchor,
            Some(Status::Stale),
            Some(MismatchReason::TargetTextChanged),
        );
        let description = "before\nnew body\nafter";
        let diff = empty_diff();
        assert!(reanchor_comment(&comment, &diff, description).is_none());
    }
}
