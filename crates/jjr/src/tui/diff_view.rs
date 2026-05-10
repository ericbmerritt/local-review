//! jjr diff-view layer.
//!
//! Re-exports the pure rendering types from `local_review_core::tui::diff_view`
//! and adds jjr-specific conversion functions that map `Comment` records (with
//! their `Anchor` variants) to `InlineComment` values ready for injection.

pub(crate) use local_review_core::tui::diff_view::{DiffView, InlineComment, RenderedLineKind};
#[cfg(test)]
pub(super) use local_review_core::tui::diff_view::{PairedRow, RenderedLine};

use crate::comment::{Anchor, Comment, Side, Status};

use super::composer::format_age;

/// Convert a saved `Comment` to an `InlineComment` for rendering, filtered by
/// file path. Returns `None` if the comment is not a line-scoped comment for
/// the given file, or if the comment is stale or orphaned and must not appear
/// inline.
pub(crate) fn comment_to_inline(
    comment: &Comment,
    comment_index: usize,
    file_path: Option<&std::path::Path>,
    now: time::OffsetDateTime,
) -> Option<InlineComment> {
    if matches!(comment.status, Some(Status::Stale | Status::Orphaned)) {
        return None;
    }
    let Anchor::Line { location, .. } = &comment.anchor else {
        return None;
    };
    match file_path {
        Some(fp) if fp == location.file.as_path() => {}
        _ => return None,
    }

    let age = format_age(now, comment.created_at);
    let body_lines = comment.body.lines().map(str::to_owned).collect();

    let (source_line, target_line) = match location.side {
        Side::Old => (location.old_line, None),
        Side::New => (None, location.new_line),
    };

    Some(InlineComment {
        source_line,
        target_line,
        severity: comment.severity,
        age,
        body_lines,
        comment_index,
    })
}

/// `source_line` is always `None`; the description view has no old side.
pub(crate) fn description_comment_to_inline(
    comment: &Comment,
    comment_index: usize,
    now: time::OffsetDateTime,
) -> Option<InlineComment> {
    if matches!(comment.status, Some(Status::Stale | Status::Orphaned)) {
        return None;
    }
    let Anchor::Description { location, .. } = &comment.anchor else {
        return None;
    };

    let age = format_age(now, comment.created_at);
    let body_lines = comment.body.lines().map(str::to_owned).collect();

    Some(InlineComment {
        source_line: None,
        target_line: location.display_line,
        severity: comment.severity,
        age,
        body_lines,
        comment_index,
    })
}

/// Convert an `Anchor::Change` comment to an `InlineComment` appended after
/// the description body. Change comments have no per-line anchor, so both
/// `source_line` and `target_line` are `None` — the caller routes them through
/// [`DiffView::with_change_comments_appended`] rather than
/// `with_inline_comments` (which keys on a matching line number).
pub(crate) fn change_comment_to_inline(
    comment: &Comment,
    comment_index: usize,
    target_change_id: &crate::change_id::ChangeId,
    now: time::OffsetDateTime,
) -> Option<InlineComment> {
    if matches!(comment.status, Some(Status::Stale | Status::Orphaned)) {
        return None;
    }
    let Anchor::Change { change_id } = &comment.anchor else {
        return None;
    };
    if change_id != target_change_id {
        return None;
    }

    let age = format_age(now, comment.created_at);
    let body_lines = comment.body.lines().map(str::to_owned).collect();

    Some(InlineComment {
        source_line: None,
        target_line: None,
        severity: comment.severity,
        age,
        body_lines,
        comment_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::{Anchor, Comment, LineAnchor, SchemaVersion, Severity, Side, Status};
    use crate::diff::{DiffFile, Hunk, Line, LineKind};
    use std::path::PathBuf;

    fn sample_modified() -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,3 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 2,
                target_start: 1,
                target_length: 3,
                lines: vec![
                    Line {
                        kind: LineKind::Context,
                        text: "ctx".to_owned(),
                        source_line: Some(1),
                        target_line: Some(1),
                    },
                    Line {
                        kind: LineKind::Added,
                        text: "added".to_owned(),
                        source_line: None,
                        target_line: Some(2),
                    },
                    Line {
                        kind: LineKind::Removed,
                        text: "removed".to_owned(),
                        source_line: Some(2),
                        target_line: None,
                    },
                ],
            }],
        }
    }

    fn make_line_comment(file: &str, severity: Severity) -> Comment {
        use crate::change_id::ChangeId;
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(2),
                    hunk_header: "@@ -1,2 +1,3 @@".to_owned(),
                    target_text: "added".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "hello".to_owned(),
            severity,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    #[test]
    fn comment_to_inline_returns_some_for_matching_file() {
        let comment = make_line_comment("foo.txt", Severity::Required);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 3, Some(std::path::Path::new("foo.txt")), now);
        let inline = inline.expect("matching file path should yield Some");
        assert_eq!(inline.severity, Severity::Required);
        assert_eq!(inline.target_line, Some(2));
        assert_eq!(inline.source_line, None);
        assert_eq!(inline.body_lines, vec!["hello".to_owned()]);
        assert_eq!(inline.comment_index, 3);
    }

    #[test]
    fn comment_to_inline_returns_none_for_file_mismatch() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 0, Some(std::path::Path::new("bar.txt")), now);
        assert!(inline.is_none());
    }

    #[test]
    fn comment_to_inline_returns_none_when_file_path_is_none() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 0, None, now);
        assert!(inline.is_none());
    }

    #[test]
    fn comment_to_inline_carries_comment_index() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 7, Some(std::path::Path::new("foo.txt")), now)
            .expect("matching file should produce Some");
        assert_eq!(inline.comment_index, 7);
    }

    #[test]
    fn inject_produces_meta_line_with_comment_index() {
        let view = DiffView::from_file(&sample_modified());
        let comment = InlineComment {
            source_line: None,
            target_line: Some(2),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["note".to_owned()],
            comment_index: 5,
        };
        let augmented = view.with_inline_comments(&[comment]);
        let meta = augmented
            .lines
            .iter()
            .find(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .expect("meta line should exist");
        match meta.kind {
            RenderedLineKind::InlineCommentMeta { comment_index } => {
                assert_eq!(comment_index, 5);
            }
            RenderedLineKind::HunkHeader
            | RenderedLineKind::HunkSeparator
            | RenderedLineKind::Context
            | RenderedLineKind::Added
            | RenderedLineKind::Removed
            | RenderedLineKind::Notice
            | RenderedLineKind::DescriptionLine
            | RenderedLineKind::InlineCommentBody => {
                panic!("expected InlineCommentMeta")
            }
        }
        let body = augmented
            .lines
            .iter()
            .find(|l| l.kind == RenderedLineKind::InlineCommentBody)
            .expect("body line should exist");
        assert_eq!(body.kind, RenderedLineKind::InlineCommentBody);
    }

    #[test]
    fn stale_comment_is_excluded_from_inline_rendering() {
        let mut comment = make_line_comment("foo.txt", Severity::Note);
        comment.status = Some(Status::Stale);
        comment.mismatch_reason = Some(crate::comment::MismatchReason::TargetTextChanged);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 0, Some(std::path::Path::new("foo.txt")), now);
        assert!(inline.is_none(), "stale comments must not render inline");
    }

    #[test]
    fn orphaned_comment_is_excluded_from_inline_rendering() {
        let mut comment = make_line_comment("foo.txt", Severity::Required);
        comment.status = Some(Status::Orphaned);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 0, Some(std::path::Path::new("foo.txt")), now);
        assert!(inline.is_none(), "orphaned comments must not render inline");
    }

    fn make_description_comment(display_line: Option<u32>, severity: Severity) -> Comment {
        use crate::comment::DescriptionAnchor;
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: crate::change_id::ChangeId::parse(&"a".repeat(32)).unwrap(),
                location: DescriptionAnchor {
                    display_line,
                    target_text: "summary line".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "description note".to_owned(),
            severity,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    #[test]
    fn description_comment_to_inline_uses_display_line_as_target() {
        let comment = make_description_comment(Some(2), Severity::Required);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = description_comment_to_inline(&comment, 7, now)
            .expect("description anchor should yield Some");
        assert_eq!(inline.target_line, Some(2));
        assert_eq!(inline.source_line, None);
        assert_eq!(inline.severity, Severity::Required);
        assert_eq!(inline.comment_index, 7);
        assert_eq!(inline.body_lines, vec!["description note".to_owned()]);
    }

    #[test]
    fn description_comment_to_inline_returns_none_for_line_anchor() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = description_comment_to_inline(&comment, 0, now);
        assert!(
            inline.is_none(),
            "line anchor must not match description path"
        );
    }

    #[test]
    fn comment_to_inline_returns_none_for_description_anchor() {
        let comment = make_description_comment(Some(1), Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, 0, Some(std::path::Path::new("foo.txt")), now);
        assert!(
            inline.is_none(),
            "description anchor must not be rendered as line comment"
        );
    }

    #[test]
    fn description_comment_injects_into_description_view_at_display_line() {
        let view = DiffView::from_description("first line\nsecond line\nthird line");
        let inline = description_comment_to_inline(
            &make_description_comment(Some(2), Severity::Suggestion),
            0,
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .expect("description anchor yields Some");
        let augmented = view.with_inline_comments(&[inline]);
        assert_eq!(augmented.lines.len(), 5);
        assert_eq!(augmented.lines[0].kind, RenderedLineKind::DescriptionLine);
        assert_eq!(augmented.lines[1].kind, RenderedLineKind::DescriptionLine);
        assert!(matches!(
            augmented.lines[2].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));
        assert_eq!(augmented.lines[3].kind, RenderedLineKind::InlineCommentBody);
        assert_eq!(augmented.lines[4].kind, RenderedLineKind::DescriptionLine);
    }

    #[test]
    fn description_comment_does_not_inject_into_diff_file_view() {
        let view = DiffView::from_file(&sample_modified());
        let original_len = view.lines.len();
        let comment = make_description_comment(Some(2), Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline_opt = comment_to_inline(&comment, 0, Some(std::path::Path::new("foo.txt")), now);
        let inlines: Vec<_> = inline_opt.into_iter().collect();
        let augmented = view.with_inline_comments(&inlines);
        assert_eq!(augmented.lines.len(), original_len);
    }

    #[test]
    fn description_comment_to_inline_returns_none_for_stale_or_orphaned() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let mut stale = make_description_comment(Some(1), Severity::Note);
        stale.status = Some(Status::Stale);
        assert!(
            description_comment_to_inline(&stale, 0, now).is_none(),
            "stale description comment must not render inline"
        );

        let mut orphaned = make_description_comment(Some(1), Severity::Required);
        orphaned.status = Some(Status::Orphaned);
        assert!(
            description_comment_to_inline(&orphaned, 0, now).is_none(),
            "orphaned description comment must not render inline"
        );
    }

    #[test]
    fn from_description_empty_input_renders_no_description_placeholder() {
        let view = DiffView::from_description("");
        assert_eq!(view.title, "<description>");
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].kind, RenderedLineKind::Notice);
        assert_eq!(view.lines[0].text, "(no description)");

        let view_ws = DiffView::from_description("   \n\t\n");
        assert_eq!(view_ws.lines.len(), 1);
        assert_eq!(view_ws.lines[0].kind, RenderedLineKind::Notice);
    }

    #[test]
    fn description_comment_with_display_line_zero_does_not_inject_anywhere() {
        let comment = make_description_comment(Some(0), Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = description_comment_to_inline(&comment, 0, now)
            .expect("conversion succeeds — display_line=0 is a valid Option<u32>");
        assert_eq!(inline.target_line, Some(0));

        let view = DiffView::from_description("hello\nworld");
        let original_len = view.lines.len();
        let augmented = view.with_inline_comments(&[inline]);
        assert_eq!(
            augmented.lines.len(),
            original_len,
            "display_line=0 must not inject anywhere — pinning silent-drop"
        );
    }

    #[test]
    fn description_comment_with_display_line_none_does_not_inject_anywhere() {
        let comment = make_description_comment(None, Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = description_comment_to_inline(&comment, 0, now)
            .expect("conversion succeeds for None display_line");
        assert_eq!(inline.target_line, None);

        let view = DiffView::from_description("hello\nworld");
        let original_len = view.lines.len();
        let augmented = view.with_inline_comments(&[inline]);
        assert_eq!(
            augmented.lines.len(),
            original_len,
            "display_line=None must not inject anywhere — pinning silent-drop"
        );
    }

    fn make_change_comment(
        target_change_id: &crate::change_id::ChangeId,
        severity: Severity,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: target_change_id.clone(),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "change note".to_owned(),
            severity,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    #[test]
    fn change_comment_to_inline_returns_some_for_matching_change() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let comment = make_change_comment(&cid, Severity::Required);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = change_comment_to_inline(&comment, 4, &cid, now)
            .expect("matching change_id should yield Some");
        assert_eq!(inline.severity, Severity::Required);
        assert_eq!(inline.source_line, None);
        assert_eq!(inline.target_line, None);
        assert_eq!(inline.comment_index, 4);
        assert_eq!(inline.body_lines, vec!["change note".to_owned()]);
    }

    #[test]
    fn change_comment_to_inline_returns_none_for_change_id_mismatch() {
        use crate::change_id::ChangeId;
        let cid_a = ChangeId::parse(&"a".repeat(32)).unwrap();
        let cid_b = ChangeId::parse(&"b".repeat(32)).unwrap();
        let comment = make_change_comment(&cid_a, Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = change_comment_to_inline(&comment, 0, &cid_b, now);
        assert!(inline.is_none());
    }

    #[test]
    fn change_comment_to_inline_returns_none_for_stale() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let mut comment = make_change_comment(&cid, Severity::Note);
        comment.status = Some(Status::Stale);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = change_comment_to_inline(&comment, 0, &cid, now);
        assert!(inline.is_none());
    }

    #[test]
    fn change_comment_to_inline_returns_none_for_orphaned() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let mut comment = make_change_comment(&cid, Severity::Note);
        comment.status = Some(Status::Orphaned);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = change_comment_to_inline(&comment, 0, &cid, now);
        assert!(inline.is_none());
    }

    #[test]
    fn change_comments_append_to_description_view_with_separator() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH;

        let desc_comment = {
            use crate::comment::{DescriptionAnchor, SchemaVersion};
            Comment {
                schema_version: SchemaVersion,
                anchor: Anchor::Description {
                    change_id: cid.clone(),
                    location: DescriptionAnchor {
                        display_line: Some(1),
                        target_text: "first description line".to_owned(),
                        context_before: vec![],
                        context_after: vec![],
                    },
                },
                repo_root: PathBuf::from("/repo"),
                revset: "@".to_owned(),
                commit_id: None,
                body: "desc note".to_owned(),
                severity: Severity::Note,
                created_at: now,
                updated_at: None,
                status: Some(Status::Pending),
                mismatch_reason: None,
            }
        };
        let change_comment = make_change_comment(&cid, Severity::Required);

        let desc_inline = description_comment_to_inline(&desc_comment, 0, now)
            .expect("description anchor yields Some");
        let change_inline = change_comment_to_inline(&change_comment, 1, &cid, now)
            .expect("matching change_id yields Some");

        let augmented = DiffView::from_description("first description line")
            .with_inline_comments(&[desc_inline])
            .with_change_comments_appended(&[change_inline]);

        let kinds: Vec<RenderedLineKind> = augmented.lines.iter().map(|l| l.kind).collect();
        assert!(matches!(
            kinds.as_slice(),
            [
                RenderedLineKind::DescriptionLine,
                RenderedLineKind::InlineCommentMeta { .. },
                RenderedLineKind::InlineCommentBody,
                RenderedLineKind::Notice,
                RenderedLineKind::InlineCommentMeta { .. },
                RenderedLineKind::InlineCommentBody,
            ]
        ));
    }

    #[test]
    fn change_comments_do_not_leak_into_diff_file_view() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH;

        let file_view = DiffView::from_file(&sample_modified());
        let original_len = file_view.lines.len();

        let change_comment = make_change_comment(&cid, Severity::Note);
        let change_inline = change_comment_to_inline(&change_comment, 0, &cid, now)
            .expect("matching change_id yields Some");

        let augmented = file_view.with_inline_comments(&[change_inline]);
        assert_eq!(
            augmented.lines.len(),
            original_len,
            "change-scoped comment must not inject into a file-level DiffView"
        );
    }

    #[test]
    fn change_comments_skip_separator_when_description_is_empty() {
        use crate::change_id::ChangeId;
        let cid = ChangeId::parse(&"a".repeat(32)).unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH;

        let change_comment = make_change_comment(&cid, Severity::Note);
        let change_inline = change_comment_to_inline(&change_comment, 0, &cid, now)
            .expect("matching change_id yields Some");

        let view = DiffView::from_description("").with_change_comments_appended(&[change_inline]);
        let notice_count = view
            .lines
            .iter()
            .filter(|l| l.kind == RenderedLineKind::Notice)
            .count();
        assert_eq!(
            notice_count, 1,
            "only the (no description) notice — no separator added for empty description"
        );
    }
}
