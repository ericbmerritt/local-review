//! Diff view data structures and rendering logic.
//!
//! A `DiffView` is a per-file snapshot of rendered diff lines with optional
//! inline comment annotations injected. All rendering decisions are pure
//! (no IO, no terminal state); the caller owns the frame/buffer.

use crate::diff::{DiffFile, Hunk, Line, LineKind};
use crate::severity::Severity;
use crate::util::strip_injection_controls;

/// Best-case rows added per inline comment when sizing the rebuilt `Vec`:
/// 1 meta line + 1 body line. Multi-line bodies grow past this; the `Vec`
/// reallocates as needed. Bound is total comment count, not user input rate.
const ROWS_PER_INLINE_COMMENT_HINT: usize = 2;

/// Context window used by [`collect_context`].
pub const CONTEXT_LINES: usize = 3;

/// Discriminates between a locally-editable draft comment and a read-only
/// GitHub review thread when stored in [`InlineComment::comment_index`] and
/// [`RenderedLineKind::InlineCommentMeta`].
///
/// - `Local(idx)` — index into the surface's loaded-drafts list; editable.
/// - `LocalReply(idx)` — index into the surface's loaded-replies list; editable.
/// - `GitHubThread(idx)` — enumerate index of a GitHub review thread fetched
///   from the API; read-only in the local tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentIndex {
    Local(usize),
    LocalReply(usize),
    GitHubThread(usize),
}

/// A rendered snapshot of one diff file (or the synthetic description view)
/// ready for the TUI renderer. Rebuilt whenever the underlying diff or
/// comments change.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub title: String,
    pub lines: Vec<RenderedLine>,
    /// Side-by-side projection of `lines`. Each entry pairs a Removed line
    /// (left/source) with an Added line (right/target) by index within their
    /// respective `-`/`+` runs in the same hunk. Context lines emit a
    /// `Pair { Some(idx), Some(idx) }` so the same source text renders on
    /// both sides, truncated independently to `side_width`. Genuine metadata
    /// rows (hunk headers, separators, notices, inline comments) occupy a
    /// `PairedRow::Spanning` row painted across the full body width.
    /// Computed eagerly on view construction and recomputed whenever
    /// `lines` is rebuilt (e.g. after `with_inline_comments`).
    pub paired_rows: Vec<PairedRow>,
}

/// A single rendered line within a `DiffView`.
#[derive(Debug, Clone)]
pub struct RenderedLine {
    pub kind: RenderedLineKind,
    pub text: String,
    /// 1-based source-side (old) line number. `None` for added lines,
    /// hunk headers, separators, and notices.
    pub source_line: Option<u32>,
    /// 1-based target-side (new) line number. `None` for removed lines,
    /// hunk headers, separators, and notices.
    pub target_line: Option<u32>,
    /// The verbatim hunk header that this line belongs to. Used when building
    /// a `LineAnchor` from the cursor position.
    pub hunk_header: Option<String>,
    /// Severity carried by injected comment lines so the renderer can color
    /// them per spec principle 6 (severity is color, not text). `None` on
    /// every non-comment line.
    pub comment_severity: Option<Severity>,
}

/// Discriminator for [`RenderedLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedLineKind {
    HunkHeader,
    HunkSeparator,
    Context,
    Added,
    Removed,
    Notice,
    /// Synthetic line injected below a diff line to display a saved comment.
    /// Carries the comment identity so `e`/`d` can resolve the focused comment
    /// in O(1) without a separate lookup.
    InlineCommentMeta {
        comment_index: CommentIndex,
    },
    /// Continuation line for a multi-line inline comment body.
    InlineCommentBody,
    /// A line from the change description, rendered in the synthetic description
    /// view prepended to the diff files list. `target_line` holds the 1-based
    /// line number within the description; `source_line` is always `None`.
    DescriptionLine,
}

/// A resolved inline comment ready to be injected below its target diff line.
#[derive(Debug, Clone)]
pub struct InlineComment {
    pub source_line: Option<u32>,
    pub target_line: Option<u32>,
    pub severity: Severity,
    /// Pre-formatted age string ("just now", "2 min ago", etc.).
    pub age: String,
    /// Comment body lines (already split on `\n`).
    pub body_lines: Vec<String>,
    /// Identity of this comment for edit/delete lookup.
    pub comment_index: CommentIndex,
}

/// One row of the side-by-side diff view.
///
/// `Spanning(idx)` rows reference a single `RenderedLine` that the renderer
/// paints across the full body width: hunk headers, separators, notices,
/// description lines, and inline comment rows.
///
/// `Pair { left, right }` rows hold up to two `RenderedLine` indices, one for
/// each column. [`PairedRow::new_pair`] enforces the invariant that at least
/// one of `left` / `right` is `Some` by returning `None` when both are `None`;
/// `#[non_exhaustive]` prevents external crates from constructing the variant
/// directly and bypassing that check. Use [`PairedRow::new_pair`] to construct
/// a `Pair` row; use [`PairedRow::pair_parts`] to read the fields when pattern
/// matching is inconvenient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairedRow {
    Spanning(usize),
    #[non_exhaustive]
    Pair {
        left: Option<usize>,
        right: Option<usize>,
    },
}

impl PairedRow {
    /// Construct a `Pair` row, or return `None` when both `left` and `right`
    /// are `None` (which would violate the at-least-one-Some invariant).
    pub fn new_pair(left: Option<usize>, right: Option<usize>) -> Option<Self> {
        if left.is_none() && right.is_none() {
            return None;
        }
        Some(Self::Pair { left, right })
    }

    /// Return `(left, right)` when this row is a `Pair`, or `None` when it is
    /// `Spanning`. Avoids the need for field-access pattern matching from
    /// outside the crate where the `#[non_exhaustive]` variant requires `..`.
    pub fn pair_parts(self) -> Option<(Option<usize>, Option<usize>)> {
        match self {
            Self::Pair { left, right } => Some((left, right)),
            Self::Spanning(_) => None,
        }
    }
}

impl DiffView {
    /// Build a `DiffView` from a parsed diff file.
    pub fn from_file(file: &DiffFile) -> Self {
        let title = render_title(file);
        let lines = render_lines(file);
        let paired_rows = pair_rows(&lines);
        Self {
            title,
            lines,
            paired_rows,
        }
    }

    /// Build the synthetic description view that always sits at view index 0,
    /// alongside the diff files. Empty / whitespace-only descriptions yield a
    /// single `Notice` placeholder row so the pane is never blank.
    pub fn from_description(description: &str) -> Self {
        if description.trim().is_empty() {
            let lines = vec![RenderedLine {
                kind: RenderedLineKind::Notice,
                text: "(no description)".to_owned(),
                source_line: None,
                target_line: None,
                hunk_header: None,
                comment_severity: None,
            }];
            let paired_rows = pair_rows(&lines);
            return Self {
                title: "<description>".to_owned(),
                lines,
                paired_rows,
            };
        }
        let lines: Vec<RenderedLine> = description
            .lines()
            .enumerate()
            .map(|(idx, text)| RenderedLine {
                kind: RenderedLineKind::DescriptionLine,
                text: text.to_owned(),
                source_line: None,
                target_line: Some(u32::try_from(idx + 1).unwrap_or(u32::MAX)),
                hunk_header: None,
                comment_severity: None,
            })
            .collect();
        let paired_rows = pair_rows(&lines);
        Self {
            title: "<description>".to_owned(),
            lines,
            paired_rows,
        }
    }

    /// Return a new `DiffView` with `InlineComment` annotation lines injected
    /// immediately below each matched diff line.
    ///
    /// Matching: for each comment, find the first `RenderedLine` in this view
    /// whose `source_line` equals `comment.source_line` (for `Side::Old`) or
    /// `target_line` equals `comment.target_line` (for `Side::New`). If no match
    /// is found, the comment is skipped.
    pub fn with_inline_comments(self, comments: &[InlineComment]) -> Self {
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

        let paired_rows = pair_rows(&output);
        Self {
            title: self.title,
            lines: output,
            paired_rows,
        }
    }

    /// Return a new `DiffView` with change-scoped comment rows appended after
    /// the description body. Used to surface change-anchored comments inline —
    /// they have no per-line anchor, so they sit as a fixed block after all
    /// description content.
    ///
    /// When the description has its own content (i.e. `DescriptionLine` rows)
    /// the appended block is preceded by a `Notice` separator row so the
    /// reader can tell at a glance that the following `┃ ● ...` rows are
    /// anchored to the change as a whole and not to the last description line.
    pub fn with_change_comments_appended(self, comments: &[InlineComment]) -> Self {
        if comments.is_empty() {
            return self;
        }

        let needs_separator = self
            .lines
            .iter()
            .any(|l| matches!(l.kind, RenderedLineKind::DescriptionLine));

        let mut output: Vec<RenderedLine> = Vec::with_capacity(
            self.lines.len()
                + usize::from(needs_separator)
                + comments.len() * ROWS_PER_INLINE_COMMENT_HINT,
        );
        output.extend(self.lines.iter().cloned());
        if needs_separator {
            output.push(RenderedLine {
                kind: RenderedLineKind::Notice,
                text: "─ on this change ─".to_owned(),
                source_line: None,
                target_line: None,
                hunk_header: None,
                comment_severity: None,
            });
        }
        for comment in comments {
            inject_comment_lines(&mut output, comment);
        }

        let paired_rows = pair_rows(&output);
        Self {
            title: self.title,
            lines: output,
            paired_rows,
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
        text: strip_injection_controls(&hunk.header),
        source_line: None,
        target_line: None,
        hunk_header: Some(hunk.header.clone()),
        comment_severity: None,
    });
    for line in &hunk.lines {
        output.push(render_line(line, &hunk.header));
    }
}

/// Build the side-by-side row projection from a unified `lines` sequence.
///
/// Pairing rule: walk the sequence top-to-bottom. Whenever a contiguous run of
/// `Removed` lines is immediately followed by a contiguous run of `Added`
/// lines, pair the Nth removed with the Nth added (index pairing). When one
/// run is longer than the other, the unpaired tail produces one-side-only
/// rows (right column blank for extra removed; left column blank for extra
/// added).
///
/// `Context` lines are unchanged source code: they belong on BOTH sides
/// (matching `delta` / `vim diffsplit` / Gerrit / Crucible convention), so
/// they emit a `Pair { left: Some(idx), right: Some(idx) }` row referencing
/// the same line on each side. The renderer truncates each side independently
/// to `side_width`, preventing long context text from bleeding past the gutter
/// into the opposite column.
///
/// **Invariant:** every `PairedRow::Pair` emitted by this function has at
/// least one of `left` / `right` as `Some`. A line index from either the
/// removed run or the added run is always present; `None` only appears on
/// the side without a counterpart.
pub fn pair_rows(lines: &[RenderedLine]) -> Vec<PairedRow> {
    let mut rows = Vec::with_capacity(lines.len());
    let mut idx = 0;
    while idx < lines.len() {
        let kind = lines[idx].kind;
        if matches!(kind, RenderedLineKind::Removed) {
            let removed_start = idx;
            while idx < lines.len() && matches!(lines[idx].kind, RenderedLineKind::Removed) {
                idx += 1;
            }
            let added_start = idx;
            while idx < lines.len() && matches!(lines[idx].kind, RenderedLineKind::Added) {
                idx += 1;
            }
            push_paired_run(&mut rows, removed_start..added_start, added_start..idx);
        } else if matches!(kind, RenderedLineKind::Added) {
            let added_start = idx;
            while idx < lines.len() && matches!(lines[idx].kind, RenderedLineKind::Added) {
                idx += 1;
            }
            push_paired_run(&mut rows, added_start..added_start, added_start..idx);
        } else if matches!(kind, RenderedLineKind::Context) {
            if let Some(row) = PairedRow::new_pair(Some(idx), Some(idx)) {
                rows.push(row);
            }
            idx += 1;
        } else {
            rows.push(PairedRow::Spanning(idx));
            idx += 1;
        }
    }
    rows
}

/// Push paired rows for a contiguous `-` run followed by a `+` run.
fn push_paired_run(
    rows: &mut Vec<PairedRow>,
    removed: std::ops::Range<usize>,
    added: std::ops::Range<usize>,
) {
    let max = removed.len().max(added.len());
    for n in 0..max {
        let left = removed.start.checked_add(n).filter(|i| *i < removed.end);
        let right = added.start.checked_add(n).filter(|i| *i < added.end);
        if let Some(row) = PairedRow::new_pair(left, right) {
            rows.push(row);
        }
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
        text: strip_injection_controls(&line.text),
        source_line: line.source_line,
        target_line: line.target_line,
        hunk_header: Some(hunk_header.to_owned()),
        comment_severity: None,
    }
}

fn line_matches_comment(line: &RenderedLine, comment: &InlineComment) -> bool {
    match (comment.source_line, comment.target_line) {
        (_, Some(tl)) => line.target_line == Some(tl),
        (Some(sl), None) => line.source_line == Some(sl),
        (None, None) => false,
    }
}

/// Severity → display label for the inline comment `┃ ● {label}` glyph.
pub fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Required => "required",
        Severity::Suggestion => "suggestion",
        Severity::Note => "note",
    }
}

/// Collect up to [`CONTEXT_LINES`] commentable lines before and after `idx`
/// in `lines`, skipping non-diff kinds (hunk headers, separators, inline
/// comment rows, notices, description lines).
///
/// Returns `(before, after)` where `before` is in source order (earliest
/// first) and `after` contains lines immediately following the cursor.
pub fn collect_context(lines: &[RenderedLine], idx: usize) -> (Vec<String>, Vec<String>) {
    let is_content = |kind: &RenderedLineKind| {
        matches!(
            kind,
            RenderedLineKind::Added | RenderedLineKind::Removed | RenderedLineKind::Context
        )
    };

    let before: Vec<String> = lines[..idx.min(lines.len())]
        .iter()
        .rev()
        .filter(|l| is_content(&l.kind))
        .take(CONTEXT_LINES)
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let after: Vec<String> = lines
        .get(idx + 1..)
        .unwrap_or(&[])
        .iter()
        .filter(|l| is_content(&l.kind))
        .take(CONTEXT_LINES)
        .map(|l| l.text.clone())
        .collect();

    (before, after)
}

fn inject_comment_lines(output: &mut Vec<RenderedLine>, comment: &InlineComment) {
    let label = severity_label(comment.severity);
    let meta = format!("┃ ● {label} · {}", comment.age);
    let source_line = comment.source_line;
    let target_line = comment.target_line;
    output.push(RenderedLine {
        kind: RenderedLineKind::InlineCommentMeta {
            comment_index: comment.comment_index,
        },
        text: meta,
        source_line,
        target_line,
        hunk_header: None,
        comment_severity: Some(comment.severity),
    });
    for body_line in &comment.body_lines {
        output.push(RenderedLine {
            kind: RenderedLineKind::InlineCommentBody,
            text: format!("┃ {body_line}"),
            source_line,
            target_line,
            hunk_header: None,
            comment_severity: Some(comment.severity),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(view.lines[1].source_line, Some(1));
        assert_eq!(view.lines[1].target_line, Some(1));
        assert_eq!(view.lines[2].source_line, None);
        assert_eq!(view.lines[2].target_line, Some(2));
        assert_eq!(view.lines[3].source_line, Some(2));
        assert_eq!(view.lines[3].target_line, None);
    }

    #[test]
    fn description_view_from_empty_string_is_notice() {
        let view = DiffView::from_description("");
        assert_eq!(view.title, "<description>");
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].kind, RenderedLineKind::Notice);
    }

    #[test]
    fn description_view_from_text_produces_description_lines() {
        let view = DiffView::from_description("hello\nworld");
        assert_eq!(view.lines.len(), 2);
        assert!(view
            .lines
            .iter()
            .all(|l| l.kind == RenderedLineKind::DescriptionLine));
        assert_eq!(view.lines[0].target_line, Some(1));
        assert_eq!(view.lines[1].target_line, Some(2));
    }

    #[test]
    fn inline_comment_injected_below_matching_line() {
        let view = DiffView::from_file(&sample_modified());
        let comments = vec![InlineComment {
            source_line: None,
            target_line: Some(2),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["body".to_owned()],
            comment_index: CommentIndex::Local(0),
        }];
        let annotated = view.with_inline_comments(&comments);
        let meta_count = annotated
            .lines
            .iter()
            .filter(|l| matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. }))
            .count();
        assert_eq!(meta_count, 1);
    }

    #[test]
    fn pair_rows_context_maps_to_both_sides() {
        let view = DiffView::from_file(&sample_modified());
        let has_pair = view.paired_rows.iter().any(|r| {
            if let PairedRow::Pair {
                left: Some(l),
                right: Some(r),
            } = r
            {
                l == r && matches!(view.lines[*l].kind, RenderedLineKind::Context)
            } else {
                false
            }
        });
        assert!(has_pair, "context line must produce a both-sides Pair row");
    }

    fn line(kind: RenderedLineKind, text: &str) -> RenderedLine {
        RenderedLine {
            kind,
            text: text.to_owned(),
            source_line: None,
            target_line: None,
            hunk_header: None,
            comment_severity: None,
        }
    }

    #[test]
    fn pair_rows_pairs_single_removed_with_single_added() {
        let lines = vec![
            line(RenderedLineKind::HunkHeader, "@@"),
            line(RenderedLineKind::Removed, "old"),
            line(RenderedLineKind::Added, "new"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], PairedRow::Spanning(0));
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(2),
            }
        );
    }

    #[test]
    fn pair_rows_pairs_multi_line_runs_by_index() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Removed, "old2"),
            line(RenderedLineKind::Added, "new1"),
            line(RenderedLineKind::Added, "new2"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: Some(2),
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(3),
            }
        );
    }

    #[test]
    fn pair_rows_unpaired_added_tail_is_right_only() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Added, "new1"),
            line(RenderedLineKind::Added, "new2"),
            line(RenderedLineKind::Added, "new3"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: Some(1),
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: None,
                right: Some(2),
            }
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(3),
            }
        );
    }

    #[test]
    fn pair_rows_unpaired_removed_tail_is_left_only() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Removed, "old2"),
            line(RenderedLineKind::Removed, "old3"),
            line(RenderedLineKind::Added, "new1"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: Some(3),
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: None,
            }
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: Some(2),
                right: None,
            }
        );
    }

    #[test]
    fn pair_rows_added_only_run_is_right_only() {
        let lines = vec![
            line(RenderedLineKind::Context, "ctx"),
            line(RenderedLineKind::Added, "new1"),
            line(RenderedLineKind::Added, "new2"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: Some(0),
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: None,
                right: Some(1),
            }
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(2),
            }
        );
    }

    #[test]
    fn pair_rows_removed_only_run_is_left_only() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Removed, "old2"),
            line(RenderedLineKind::Context, "ctx"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: None,
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: None,
            }
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: Some(2),
                right: Some(2),
            }
        );
    }

    #[test]
    fn pair_rows_empty_input() {
        assert!(pair_rows(&[]).is_empty());
    }

    #[test]
    fn pair_rows_hunk_header_spans_and_context_lines_self_pair() {
        let lines = vec![
            line(RenderedLineKind::HunkHeader, "@@"),
            line(RenderedLineKind::Context, "a"),
            line(RenderedLineKind::Context, "b"),
            line(RenderedLineKind::Context, "c"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], PairedRow::Spanning(0));
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(1),
            }
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: Some(2),
                right: Some(2),
            }
        );
        assert_eq!(
            rows[3],
            PairedRow::Pair {
                left: Some(3),
                right: Some(3),
            }
        );
    }

    #[test]
    fn pair_rows_all_removed_file() {
        let lines = vec![
            line(RenderedLineKind::Removed, "a"),
            line(RenderedLineKind::Removed, "b"),
            line(RenderedLineKind::Removed, "c"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(
                matches!(
                    r,
                    PairedRow::Pair {
                        left: Some(_),
                        right: None,
                        ..
                    }
                ),
                "expected Pair{{ left: Some, right: None }}, got {r:?}"
            );
        }
    }

    #[test]
    fn pair_rows_all_added_file() {
        let lines = vec![
            line(RenderedLineKind::Added, "a"),
            line(RenderedLineKind::Added, "b"),
            line(RenderedLineKind::Added, "c"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(
                matches!(
                    r,
                    PairedRow::Pair {
                        left: None,
                        right: Some(_),
                        ..
                    }
                ),
                "expected Pair{{ left: None, right: Some }}, got {r:?}"
            );
        }
    }

    #[test]
    fn pair_rows_comment_breaks_run_pairing() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old"),
            line(
                RenderedLineKind::InlineCommentMeta {
                    comment_index: CommentIndex::Local(0),
                },
                "┃ ● note",
            ),
            line(RenderedLineKind::Added, "new"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: None,
            }
        );
        assert_eq!(rows[1], PairedRow::Spanning(1));
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(2),
            }
        );
    }

    #[test]
    fn from_file_populates_paired_rows() {
        let view = DiffView::from_file(&sample_modified());
        assert_eq!(view.paired_rows.len(), 4);
        assert_eq!(view.paired_rows[0], PairedRow::Spanning(0));
        assert_eq!(
            view.paired_rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(1),
            }
        );
        assert_eq!(
            view.paired_rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(2),
            }
        );
        assert_eq!(
            view.paired_rows[3],
            PairedRow::Pair {
                left: Some(3),
                right: None,
            }
        );
    }

    #[test]
    fn pair_rows_treats_context_as_pair_not_span() {
        let lines = vec![
            line(RenderedLineKind::HunkHeader, "@@ -1,3 +1,4 @@"),
            line(RenderedLineKind::Context, "ctx_before"),
            line(RenderedLineKind::Removed, "old"),
            line(RenderedLineKind::Added, "new"),
            line(RenderedLineKind::Context, "ctx_after"),
            line(RenderedLineKind::Notice, "(note)"),
            line(RenderedLineKind::DescriptionLine, "desc"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0], PairedRow::Spanning(0), "HunkHeader must Span");
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(1),
            },
            "Context line must self-pair so each side truncates independently"
        );
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: Some(2),
                right: Some(3),
            },
            "Removed/Added pair as before"
        );
        assert_eq!(
            rows[3],
            PairedRow::Pair {
                left: Some(4),
                right: Some(4),
            },
            "trailing Context must also self-pair"
        );
        assert_eq!(rows[4], PairedRow::Spanning(5), "Notice must Span");
        assert_eq!(rows[5], PairedRow::Spanning(6), "DescriptionLine must Span");
    }

    #[test]
    fn pair_rows_does_not_pair_across_hunk_boundary() {
        let lines = vec![
            line(RenderedLineKind::HunkHeader, "@@ -1,1 +1,1 @@"),
            line(RenderedLineKind::Removed, "old_A"),
            line(RenderedLineKind::HunkSeparator, ""),
            line(RenderedLineKind::HunkHeader, "@@ -10,1 +10,1 @@"),
            line(RenderedLineKind::Added, "new_B"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], PairedRow::Spanning(0));
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: None,
            },
            "hunk A's `-` must NOT pair with hunk B's `+`"
        );
        assert_eq!(rows[2], PairedRow::Spanning(2));
        assert_eq!(rows[3], PairedRow::Spanning(3));
        assert_eq!(
            rows[4],
            PairedRow::Pair {
                left: None,
                right: Some(4),
            },
            "hunk B's `+` must NOT pair with hunk A's `-`"
        );
    }

    #[test]
    fn pair_rows_comment_in_middle_of_runs_breaks_each_side() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Removed, "old2"),
            line(
                RenderedLineKind::InlineCommentMeta {
                    comment_index: CommentIndex::Local(0),
                },
                "┃ ● note",
            ),
            line(RenderedLineKind::Added, "new1"),
            line(RenderedLineKind::Added, "new2"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: None,
            },
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: None,
            },
        );
        assert_eq!(rows[2], PairedRow::Spanning(2));
        assert_eq!(
            rows[3],
            PairedRow::Pair {
                left: None,
                right: Some(3),
            },
        );
        assert_eq!(
            rows[4],
            PairedRow::Pair {
                left: None,
                right: Some(4),
            },
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
            comment_index: CommentIndex::Local(0),
        };
        let augmented = view.with_inline_comments(&[comment]);
        assert_eq!(augmented.lines.len(), 6);
        assert_eq!(augmented.lines[2].kind, RenderedLineKind::Added);
        assert!(matches!(
            augmented.lines[3].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));
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
    fn with_inline_comments_rebuilds_paired_rows() {
        let view = DiffView::from_file(&sample_modified());
        let before = view.paired_rows.len();
        let comment = InlineComment {
            source_line: None,
            target_line: Some(2),
            severity: Severity::Required,
            body_lines: vec!["fix".to_owned()],
            age: "just now".to_owned(),
            comment_index: CommentIndex::Local(0),
        };
        let augmented = view.with_inline_comments(&[comment]);
        assert!(
            augmented.paired_rows.len() > before,
            "comment injection should add at least one paired row"
        );
    }

    fn make_change_inline(severity: Severity, body: &str) -> InlineComment {
        InlineComment {
            source_line: None,
            target_line: None,
            severity,
            age: "just now".to_owned(),
            body_lines: vec![body.to_owned()],
            comment_index: CommentIndex::Local(0),
        }
    }

    #[test]
    fn change_comments_append_to_description_view_with_separator() {
        let desc_inline = InlineComment {
            source_line: None,
            target_line: Some(1),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["desc note".to_owned()],
            comment_index: CommentIndex::Local(0),
        };
        let change_inline = make_change_inline(Severity::Required, "split");
        let augmented_desc = DiffView::from_description("first description line")
            .with_inline_comments(&[desc_inline])
            .with_change_comments_appended(&[change_inline]);
        let kinds: Vec<RenderedLineKind> = augmented_desc.lines.iter().map(|l| l.kind).collect();
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
        let file_view = DiffView::from_file(&sample_modified());
        let original_len = file_view.lines.len();
        let change_comment = InlineComment {
            source_line: None,
            target_line: None,
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["change-scoped body".to_owned()],
            comment_index: CommentIndex::Local(0),
        };
        let augmented_file = file_view.with_inline_comments(&[change_comment]);
        let change_metas = augmented_file
            .lines
            .iter()
            .filter(|l| {
                matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. })
                    && l.source_line.is_none()
                    && l.target_line.is_none()
            })
            .count();
        assert_eq!(
            change_metas, 0,
            "change-scoped comments must not appear in file-level views"
        );
        assert_eq!(
            augmented_file.lines.len(),
            original_len,
            "no lines added for change-scoped comment"
        );
    }

    // The "(no description)" placeholder is itself signal that what follows is
    // not anchored to any description line; an additional separator would be
    // visual noise.
    #[test]
    fn change_comments_skip_separator_when_description_is_empty() {
        let inline = make_change_inline(Severity::Note, "body");
        let view = DiffView::from_description("").with_change_comments_appended(&[inline]);
        let notice_count = view
            .lines
            .iter()
            .filter(|l| l.kind == RenderedLineKind::Notice)
            .count();
        assert_eq!(
            notice_count, 1,
            "only the (no description) notice — no separator added"
        );
    }

    #[test]
    fn new_pair_returns_none_when_both_sides_are_none() {
        assert_eq!(PairedRow::new_pair(None, None), None);
    }

    // -----------------------------------------------------------------------
    // push_hunk — strip invariant: display text stripped, anchor verbatim
    // -----------------------------------------------------------------------

    #[test]
    fn push_hunk_header_display_stripped_anchor_verbatim() {
        // Hunk header containing ANSI escape sequences: ESC is a control
        // character and must be stripped from the rendered display text, but
        // the verbatim header must be preserved in hunk_header for anchor
        // identity matching.
        let header = "\x1b[31m@@ -1,1 +1,1 @@\x1b[0m".to_owned();
        let hunk = Hunk {
            header: header.clone(),
            function_context: None,
            source_start: 1,
            source_length: 1,
            target_start: 1,
            target_length: 1,
            lines: vec![],
        };
        let mut output = Vec::new();
        push_hunk(&mut output, &hunk);
        let header_line = &output[0];
        assert_eq!(header_line.kind, RenderedLineKind::HunkHeader);
        assert!(
            !header_line.text.chars().any(char::is_control),
            "rendered hunk header text must have no control chars; got: {:?}",
            header_line.text
        );
        assert_eq!(
            header_line.hunk_header.as_deref(),
            Some(header.as_str()),
            "hunk_header anchor key must be the verbatim (unstripped) header"
        );
    }

    // -----------------------------------------------------------------------

    #[test]
    fn render_line_strips_control_chars_from_line_text() {
        // A diff line whose text contains ANSI escape sequences must have
        // those control characters stripped in the rendered RenderedLine::text.
        let file = DiffFile::Modified {
            path: PathBuf::from("test.rs"),
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 1,
                target_start: 1,
                target_length: 1,
                lines: vec![Line {
                    kind: LineKind::Added,
                    text: "\x1b[32mcolored text\x1b[0m".to_owned(),
                    source_line: None,
                    target_line: Some(1),
                }],
            }],
        };
        let view = DiffView::from_file(&file);
        let added_line = view
            .lines
            .iter()
            .find(|l| l.kind == RenderedLineKind::Added)
            .expect("must have an Added line");
        assert!(
            !added_line.text.chars().any(char::is_control),
            "rendered line text must have control chars stripped; got: {:?}",
            added_line.text
        );
        assert!(
            added_line.text.contains("colored text"),
            "non-control characters must be preserved; got: {:?}",
            added_line.text
        );
    }

    #[test]
    fn push_hunk_preserves_tab_in_display_text() {
        // Hunk headers from some diff tools include tab characters.
        // Tabs must be preserved in the display text (they are not injection
        // vectors); ESC sequences are still stripped.
        let header = "@@ -1,1 +1,1 @@ fn\tfoo()".to_owned();
        let hunk = Hunk {
            header: header.clone(),
            function_context: None,
            source_start: 1,
            source_length: 1,
            target_start: 1,
            target_length: 1,
            lines: vec![],
        };
        let mut output = Vec::new();
        push_hunk(&mut output, &hunk);
        let header_line = &output[0];
        assert!(
            header_line.text.contains('\t'),
            "tab must be preserved in hunk header display text; got: {:?}",
            header_line.text
        );
        assert_eq!(
            header_line.hunk_header.as_deref(),
            Some(header.as_str()),
            "hunk_header anchor key must be the verbatim (unstripped) header"
        );
    }

    #[test]
    fn render_line_preserves_tab_in_line_text() {
        // Go, Makefile, Python, and Rust source files use tab indentation.
        // Tabs must survive into RenderedLine::text so the TUI displays correct
        // indentation; ESC sequences are still stripped.
        let file = DiffFile::Modified {
            path: PathBuf::from("Makefile"),
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".to_owned(),
                function_context: None,
                source_start: 1,
                source_length: 1,
                target_start: 1,
                target_length: 1,
                lines: vec![Line {
                    kind: LineKind::Added,
                    text: "\tindented line".to_owned(),
                    source_line: None,
                    target_line: Some(1),
                }],
            }],
        };
        let view = DiffView::from_file(&file);
        let added_line = view
            .lines
            .iter()
            .find(|l| l.kind == RenderedLineKind::Added)
            .expect("must have an Added line");
        assert!(
            added_line.text.contains('\t'),
            "tab must be preserved in rendered line text; got: {:?}",
            added_line.text
        );
        assert!(
            added_line.text.contains("indented line"),
            "non-tab content must be preserved; got: {:?}",
            added_line.text
        );
    }
}
