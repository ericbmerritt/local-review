use crate::comment::{Anchor, Comment, Severity, Side};
use crate::diff::{DiffFile, Hunk, Line, LineKind};

use super::composer::format_age;

/// Best-case rows added per inline comment when sizing the rebuilt `Vec`:
/// 1 meta line + 1 body line. Multi-line bodies grow past this; the `Vec`
/// reallocates as needed. Bound is total comment count, not user input rate.
const ROWS_PER_INLINE_COMMENT_HINT: usize = 2;

#[derive(Debug, Clone)]
pub(crate) struct DiffView {
    pub(crate) title: String,
    pub(crate) lines: Vec<RenderedLine>,
}

#[derive(Debug, Clone)]
pub(crate) struct RenderedLine {
    pub(crate) kind: RenderedLineKind,
    pub(crate) text: String,
    /// 1-based source-side (old) line number. `None` for added lines,
    /// hunk headers, separators, and notices.
    pub(crate) source_line: Option<u32>,
    /// 1-based target-side (new) line number. `None` for removed lines,
    /// hunk headers, separators, and notices.
    pub(crate) target_line: Option<u32>,
    /// The verbatim hunk header that this line belongs to. Used when building
    /// a `LineAnchor` from the cursor position.
    pub(crate) hunk_header: Option<String>,
    /// Severity carried by injected comment lines so the renderer can color
    /// them per spec principle 6 (severity is color, not text). `None` on
    /// every non-comment line.
    pub(crate) comment_severity: Option<Severity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderedLineKind {
    HunkHeader,
    HunkSeparator,
    Context,
    Added,
    Removed,
    Notice,
    /// Synthetic line injected below a diff line to display a saved comment.
    InlineCommentMeta,
    /// Continuation line for a multi-line inline comment body.
    InlineCommentBody,
}

/// A resolved inline comment ready to be injected below its target diff line.
#[derive(Debug, Clone)]
pub(crate) struct InlineComment {
    pub(crate) source_line: Option<u32>,
    pub(crate) target_line: Option<u32>,
    pub(crate) severity: Severity,
    /// Pre-formatted age string ("just now", "2 min ago", etc.).
    pub(crate) age: String,
    /// Comment body lines (already split on `\n`).
    pub(crate) body_lines: Vec<String>,
}

/// Convert a saved `Comment` to an `InlineComment` for rendering, filtered by
/// file path. Returns `None` if the comment is not a line-scoped comment for
/// the given file.
pub(crate) fn comment_to_inline(
    comment: &Comment,
    file_path: Option<&std::path::Path>,
    now: time::OffsetDateTime,
) -> Option<InlineComment> {
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
    })
}

impl DiffView {
    pub(crate) fn from_file(file: &DiffFile) -> Self {
        let title = render_title(file);
        let lines = render_lines(file);
        Self { title, lines }
    }

    /// Return a new `DiffView` with `InlineComment` annotation lines injected
    /// immediately below each matched diff line.
    ///
    /// Matching: for each comment, find the first `RenderedLine` in this view
    /// whose `source_line` equals `comment.source_line` (for `Side::Old`) or
    /// `target_line` equals `comment.target_line` (for `Side::New`). If no match
    /// is found, the comment is skipped (logged by the caller if needed).
    pub(crate) fn with_inline_comments(self, comments: &[InlineComment]) -> Self {
        if comments.is_empty() {
            return self;
        }

        let mut output: Vec<RenderedLine> =
            Vec::with_capacity(self.lines.len() + comments.len() * ROWS_PER_INLINE_COMMENT_HINT);

        for line in &self.lines {
            output.push(line.clone());
            for comment in comments {
                if line_matches_comment(line, comment) {
                    inject_comment_lines(&mut output, comment);
                }
            }
        }

        Self {
            title: self.title,
            lines: output,
        }
    }
}

fn render_title(file: &DiffFile) -> String {
    match file {
        DiffFile::Modified { path, .. } => path.display().to_string(),
        DiffFile::Added { path, .. } => format!("{} (added)", path.display()),
        DiffFile::Removed { path, .. } => format!("{} (removed)", path.display()),
        DiffFile::Renamed { from, to, .. } => {
            format!("{} -> {}", from.display(), to.display())
        }
        DiffFile::Binary { path } => format!("{} (binary)", path.display()),
    }
}

fn render_lines(file: &DiffFile) -> Vec<RenderedLine> {
    if let DiffFile::Binary { .. } = file {
        return vec![RenderedLine {
            kind: RenderedLineKind::Notice,
            text: "Binary file not shown".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        }];
    }

    let mut output = Vec::new();
    let hunks = file.hunks();
    if hunks.is_empty() {
        output.push(RenderedLine {
            kind: RenderedLineKind::Notice,
            text: "No textual changes in this file.".to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        });
        return output;
    }

    for (index, hunk) in hunks.iter().enumerate() {
        if index > 0 {
            output.push(RenderedLine {
                kind: RenderedLineKind::HunkSeparator,
                text: String::new(),
                source_line: None,
                target_line: None,
                hunk_header: None,
                comment_severity: None,
            });
        }
        push_hunk(&mut output, hunk);
    }

    output
}

fn push_hunk(output: &mut Vec<RenderedLine>, hunk: &Hunk) {
    output.push(RenderedLine {
        kind: RenderedLineKind::HunkHeader,
        text: hunk.header.clone(),
        source_line: None,
        target_line: None,
        hunk_header: Some(hunk.header.clone()),
        comment_severity: None,
    });
    for line in &hunk.lines {
        output.push(render_line(line, &hunk.header));
    }
}

fn render_line(line: &Line, hunk_header: &str) -> RenderedLine {
    let kind = match line.kind {
        LineKind::Context => RenderedLineKind::Context,
        LineKind::Added => RenderedLineKind::Added,
        LineKind::Removed => RenderedLineKind::Removed,
    };
    RenderedLine {
        kind,
        text: line.text.clone(),
        source_line: line.source_line,
        target_line: line.target_line,
        hunk_header: Some(hunk_header.to_owned()),
        comment_severity: None,
    }
}

fn line_matches_comment(line: &RenderedLine, comment: &InlineComment) -> bool {
    match (comment.source_line, comment.target_line) {
        (Some(sl), _) if comment.target_line.is_none() => line.source_line == Some(sl),
        (_, Some(tl)) => line.target_line == Some(tl),
        _ => false,
    }
}

fn inject_comment_lines(output: &mut Vec<RenderedLine>, comment: &InlineComment) {
    let label = severity_label(comment.severity);
    // `●` sigil pairs with the severity color so NO_COLOR terminals still
    // distinguish severity by reading the label.
    let meta = format!("┃ ● {label} · {}", comment.age);
    output.push(RenderedLine {
        kind: RenderedLineKind::InlineCommentMeta,
        text: meta,
        source_line: None,
        target_line: None,
        hunk_header: None,
        comment_severity: Some(comment.severity),
    });
    for body_line in &comment.body_lines {
        output.push(RenderedLine {
            kind: RenderedLineKind::InlineCommentBody,
            text: format!("┃ {body_line}"),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: Some(comment.severity),
        });
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Note => "note",
        Severity::Suggestion => "suggestion",
        Severity::Required => "required",
    }
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

    #[test]
    fn renders_modified_file_lines() {
        let view = DiffView::from_file(&sample_modified());
        assert_eq!(view.title, "foo.txt");
        assert_eq!(view.lines.len(), 4);
        assert_eq!(view.lines[0].kind, RenderedLineKind::HunkHeader);
        assert_eq!(view.lines[1].kind, RenderedLineKind::Context);
        assert_eq!(view.lines[2].kind, RenderedLineKind::Added);
        assert_eq!(view.lines[3].kind, RenderedLineKind::Removed);
    }

    #[test]
    fn renders_binary_notice() {
        let file = DiffFile::Binary {
            path: PathBuf::from("foo.bin"),
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.title, "foo.bin (binary)");
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].kind, RenderedLineKind::Notice);
    }

    #[test]
    fn renders_renamed_title_with_ascii_arrow() {
        let file = DiffFile::Renamed {
            from: PathBuf::from("old.rs"),
            to: PathBuf::from("new.rs"),
            hunks: vec![],
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.title, "old.rs -> new.rs");
    }

    #[test]
    fn renders_added_title() {
        let file = DiffFile::Added {
            path: PathBuf::from("new.rs"),
            hunks: vec![],
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.title, "new.rs (added)");
    }

    #[test]
    fn renders_removed_title() {
        let file = DiffFile::Removed {
            path: PathBuf::from("old.rs"),
            hunks: vec![],
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.title, "old.rs (removed)");
    }

    #[test]
    fn empty_hunks_produce_notice() {
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.txt"),
            hunks: vec![],
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].kind, RenderedLineKind::Notice);
    }

    #[test]
    fn separator_between_hunks() {
        let mut file = sample_modified();
        if let DiffFile::Modified { hunks, .. } = &mut file {
            let extra = hunks[0].clone();
            hunks.push(extra);
        }
        let view = DiffView::from_file(&file);
        let separator_count = view
            .lines
            .iter()
            .filter(|l| l.kind == RenderedLineKind::HunkSeparator)
            .count();
        assert_eq!(separator_count, 1);
    }

    #[test]
    fn rendered_line_carries_source_and_target_line_numbers() {
        let view = DiffView::from_file(&sample_modified());
        // [0] HunkHeader, [1] Context(src=1,tgt=1), [2] Added(src=None,tgt=2),
        // [3] Removed(src=2,tgt=None)
        assert_eq!(view.lines[1].source_line, Some(1));
        assert_eq!(view.lines[1].target_line, Some(1));
        assert_eq!(view.lines[2].source_line, None);
        assert_eq!(view.lines[2].target_line, Some(2));
        assert_eq!(view.lines[3].source_line, Some(2));
        assert_eq!(view.lines[3].target_line, None);
    }

    #[test]
    fn rendered_line_carries_hunk_header() {
        let view = DiffView::from_file(&sample_modified());
        assert_eq!(
            view.lines[1].hunk_header.as_deref(),
            Some("@@ -1,2 +1,3 @@")
        );
        assert_eq!(
            view.lines[2].hunk_header.as_deref(),
            Some("@@ -1,2 +1,3 @@")
        );
    }

    #[test]
    fn with_inline_comments_injects_after_matched_line() {
        let view = DiffView::from_file(&sample_modified());
        let comment = InlineComment {
            source_line: None,
            target_line: Some(2),
            severity: Severity::Required,
            age: "just now".to_owned(),
            body_lines: vec!["Fix this.".to_owned()],
        };
        let augmented = view.with_inline_comments(&[comment]);
        // Original: [HunkHeader, Context, Added, Removed] = 4 lines
        // After Added (target_line=2) we inject meta + 1 body = 2 extra lines
        assert_eq!(augmented.lines.len(), 6);
        assert_eq!(augmented.lines[2].kind, RenderedLineKind::Added);
        assert_eq!(augmented.lines[3].kind, RenderedLineKind::InlineCommentMeta);
        assert!(augmented.lines[3].text.contains("required"));
        assert!(augmented.lines[3].text.contains("just now"));
        assert_eq!(
            augmented.lines[3].comment_severity,
            Some(Severity::Required)
        );
        assert_eq!(augmented.lines[4].kind, RenderedLineKind::InlineCommentBody);
        assert!(augmented.lines[4].text.contains("Fix this."));
        assert_eq!(
            augmented.lines[4].comment_severity,
            Some(Severity::Required)
        );
    }

    #[test]
    fn with_inline_comments_no_match_leaves_lines_unchanged() {
        let view = DiffView::from_file(&sample_modified());
        let original_len = view.lines.len();
        let comment = InlineComment {
            source_line: None,
            target_line: Some(999),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["No match.".to_owned()],
        };
        let augmented = view.with_inline_comments(&[comment]);
        assert_eq!(augmented.lines.len(), original_len);
    }

    #[test]
    fn with_inline_comments_empty_slice_returns_same_view() {
        let view = DiffView::from_file(&sample_modified());
        let original_len = view.lines.len();
        let augmented = view.with_inline_comments(&[]);
        assert_eq!(augmented.lines.len(), original_len);
    }

    #[test]
    fn with_inline_comments_source_line_match() {
        let view = DiffView::from_file(&sample_modified());
        let comment = InlineComment {
            source_line: Some(2),
            target_line: None,
            severity: Severity::Suggestion,
            age: "1 min ago".to_owned(),
            body_lines: vec!["Old line comment.".to_owned()],
        };
        let augmented = view.with_inline_comments(&[comment]);
        // Removed line is at index 3 (src=2), after it we inject 2 lines
        assert_eq!(augmented.lines.len(), 6);
        assert_eq!(augmented.lines[3].kind, RenderedLineKind::Removed);
        assert_eq!(augmented.lines[4].kind, RenderedLineKind::InlineCommentMeta);
    }

    fn make_line_comment(file: &str, severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: crate::change_id::ChangeId::parse(&"a".repeat(32)).unwrap(),
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
        let inline = comment_to_inline(&comment, Some(std::path::Path::new("foo.txt")), now);
        let inline = inline.expect("matching file path should yield Some");
        assert_eq!(inline.severity, Severity::Required);
        assert_eq!(inline.target_line, Some(2));
        assert_eq!(inline.source_line, None);
        assert_eq!(inline.body_lines, vec!["hello".to_owned()]);
    }

    #[test]
    fn comment_to_inline_returns_none_for_file_mismatch() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, Some(std::path::Path::new("bar.txt")), now);
        assert!(inline.is_none());
    }

    #[test]
    fn comment_to_inline_returns_none_when_file_path_is_none() {
        let comment = make_line_comment("foo.txt", Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = comment_to_inline(&comment, None, now);
        assert!(inline.is_none());
    }
}
