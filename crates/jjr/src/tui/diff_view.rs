use crate::comment::{Anchor, Comment, Severity, Side, Status};
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
    /// Side-by-side projection of `lines`. Each entry pairs a Removed line
    /// (left/source) with an Added line (right/target) by index within their
    /// respective `-`/`+` runs in the same hunk. Context lines emit a
    /// `Pair { Some(idx), Some(idx) }` so the same source text renders on
    /// both sides, truncated independently to `side_width`. Genuine metadata
    /// rows (hunk headers, separators, notices, inline comments) occupy a
    /// `PairedRow::Spanning` row painted across the full body width.
    /// Computed eagerly on view construction and recomputed whenever
    /// `lines` is rebuilt (e.g. after `with_inline_comments`).
    pub(crate) paired_rows: Vec<PairedRow>,
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
    /// Carries the index into `App::loaded_comments` so `e`/`d` can resolve
    /// the focused comment in O(1) without a separate lookup.
    InlineCommentMeta {
        comment_index: usize,
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
pub(crate) struct InlineComment {
    pub(crate) source_line: Option<u32>,
    pub(crate) target_line: Option<u32>,
    pub(crate) severity: Severity,
    /// Pre-formatted age string ("just now", "2 min ago", etc.).
    pub(crate) age: String,
    /// Comment body lines (already split on `\n`).
    pub(crate) body_lines: Vec<String>,
    /// Index into `App::loaded_comments` for edit/delete lookup.
    pub(crate) comment_index: usize,
}

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

/// One row of the side-by-side diff view.
///
/// `Spanning(idx)` rows reference a single `RenderedLine` that the renderer
/// paints across the full body width: hunk headers, separators, notices,
/// description lines, and inline comment rows. The renderer decides whether
/// to expand to full width or restrict to the right column (for inline
/// comments) based on the `RenderedLine`'s `kind` — that policy is
/// rendering, not data.
///
/// `Pair { left, right }` rows hold up to two `RenderedLine` indices, one for
/// each column. By construction at least one of `left` or `right` is `Some`;
/// both `None` is unrepresentable. `pair_rows` emits Pair rows for `Removed`
/// / `Added` runs (left = Removed, right = Added; unequal-length runs leave
/// a `None` cell on the shorter side) and for `Context` lines (`left ==
/// right == Some(idx)` referencing the same line so each side truncates
/// independently). No other kinds yield Pair rows. The kind invariants are
/// upheld by `pair_rows`, not the type system — see deferred follow-up to
/// NewType-encode the side-specific kinds.
///
/// Pairing rule: index pairing within each hunk's `-`/`+` run. The Nth
/// removed line pairs with the Nth added line. Unequal-length runs leave a
/// `None` cell on the shorter side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairedRow {
    Spanning(usize),
    Pair {
        left: Option<usize>,
        right: Option<usize>,
    },
}

impl DiffView {
    pub(crate) fn from_file(file: &DiffFile) -> Self {
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
    pub(crate) fn from_description(description: &str) -> Self {
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
    /// The empty-description case already shows a `(no description)` Notice
    /// placeholder, which is signal enough on its own — no extra separator.
    ///
    /// The renderer uses the same `InlineCommentMeta` / `InlineCommentBody`
    /// kinds and severity styling as line- and description-anchored comments,
    /// so navigation (`e`, `d`, `c`) and visual treatment fall out unchanged.
    pub(crate) fn with_change_comments_appended(self, comments: &[InlineComment]) -> Self {
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
/// Genuinely-metadata rows — hunk headers, separators, notices, description
/// lines, and inline comment meta/body rows — produce a `Spanning(idx)` row
/// the renderer paints across the entire body width (or, for inline comment
/// rows, in the right column only per the side-by-side spec — that decision
/// lives in the renderer, not the data model).
///
/// Inline comment rows that follow a Removed/Added line interrupt the
/// `-`/`+` run for pairing purposes: a `-` run + comment + `+` run pairs the
/// comment as a `Spanning` row and the `+` run starts fresh (no pairing
/// against the earlier `-` run). This keeps comment placement faithful to its
/// anchor and avoids visually-misleading cross-comment pairings.
pub(crate) fn pair_rows(lines: &[RenderedLine]) -> Vec<PairedRow> {
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
            // A `+` run that is not preceded by a `-` run: every line
            // is right-side-only.
            let added_start = idx;
            while idx < lines.len() && matches!(lines[idx].kind, RenderedLineKind::Added) {
                idx += 1;
            }
            push_paired_run(&mut rows, added_start..added_start, added_start..idx);
        } else if matches!(kind, RenderedLineKind::Context) {
            // Context is source content; render on both sides so each column
            // truncates independently to side_width.
            rows.push(PairedRow::Pair {
                left: Some(idx),
                right: Some(idx),
            });
            idx += 1;
        } else {
            rows.push(PairedRow::Spanning(idx));
            idx += 1;
        }
    }
    rows
}

/// Push paired rows for a contiguous `-` run followed by a `+` run.
/// Index-pairs the two runs and emits `Pair { left, right }` rows; one side
/// becomes `None` whenever a run is shorter than the other. Empty input
/// (both ranges zero-length) emits no rows.
fn push_paired_run(
    rows: &mut Vec<PairedRow>,
    removed: std::ops::Range<usize>,
    added: std::ops::Range<usize>,
) {
    let max = removed.len().max(added.len());
    for n in 0..max {
        let left = removed.start.checked_add(n).filter(|i| *i < removed.end);
        let right = added.start.checked_add(n).filter(|i| *i < added.end);
        rows.push(PairedRow::Pair { left, right });
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
        (_, Some(tl)) => line.target_line == Some(tl),
        (Some(sl), None) => line.source_line == Some(sl),
        (None, None) => false,
    }
}

fn inject_comment_lines(output: &mut Vec<RenderedLine>, comment: &InlineComment) {
    let label = super::severity_label(comment.severity);
    // `●` sigil pairs with the severity color so NO_COLOR terminals still
    // distinguish severity by reading the label.
    let meta = format!("┃ ● {label} · {}", comment.age);
    // Propagate the comment's anchor-side line numbers onto the synthetic
    // rows so the side-by-side renderer can read which side the anchor was
    // on (Side::Old → source_line.is_some(), Side::New → target_line.is_some()).
    // This does not change unified-mode behavior — the unified renderer
    // ignores these fields on inline-comment kinds.
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
            comment_index: 0,
        };
        let augmented = view.with_inline_comments(&[comment]);
        // Original: [HunkHeader, Context, Added, Removed] = 4 lines
        // After Added (target_line=2) we inject meta + 1 body = 2 extra lines
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
    fn with_inline_comments_no_match_leaves_lines_unchanged() {
        let view = DiffView::from_file(&sample_modified());
        let original_len = view.lines.len();
        let comment = InlineComment {
            source_line: None,
            target_line: Some(999),
            severity: Severity::Note,
            age: "just now".to_owned(),
            body_lines: vec!["No match.".to_owned()],
            comment_index: 0,
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
            comment_index: 0,
        };
        let augmented = view.with_inline_comments(&[comment]);
        // Removed line is at index 3 (src=2), after it we inject 2 lines
        assert_eq!(augmented.lines.len(), 6);
        assert_eq!(augmented.lines[3].kind, RenderedLineKind::Removed);
        assert!(matches!(
            augmented.lines[4].kind,
            RenderedLineKind::InlineCommentMeta { .. }
        ));
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
        // Body lines are not the carrier of the index; they have a different kind.
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

    // -- B1: a description-anchored comment becomes an `InlineComment`
    //   targeting the saved `display_line`. Pins the parallel-path conversion
    //   that the description view 0 needs.
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

    // -- B1: a line-anchored comment must not slip through the description
    //   conversion path. Defends the symmetric bug where the wrong path
    //   accepts the wrong anchor kind.
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

    // -- B1: a description-anchored comment must not slip through the
    //   line-comment conversion path; otherwise it would be injected into
    //   the wrong (diff-file) view.
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

    // -- B1: a description comment injects an inline at the matching
    //   description-view line (built via `from_description`) and does NOT
    //   appear when injecting against a diff-file view.
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
        // 3 description lines + meta + 1 body line under line 2.
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
        // Build the diff-file inline list using `comment_to_inline` (which
        // refuses description anchors); confirm the augmented view is unchanged.
        let comment = make_description_comment(Some(2), Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline_opt = comment_to_inline(&comment, 0, Some(std::path::Path::new("foo.txt")), now);
        let inlines: Vec<_> = inline_opt.into_iter().collect();
        let augmented = view.with_inline_comments(&inlines);
        assert_eq!(augmented.lines.len(), original_len);
    }

    // -- T1: stale and orphaned description-anchored comments must not render
    //   inline. Mirrors the line-anchored guard.
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

    // -- U4: empty / whitespace-only descriptions render a Notice placeholder
    //   so the description pane is never silently blank.
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

    // -- T-G5: display_line=0 boundary. `from_description` emits 1-based
    //   target_line numbers, so an inline with `target_line: Some(0)` will
    //   not match any rendered DescriptionLine — the comment is silently
    //   dropped from the rendered view. Pin the current behavior so a
    //   future change is intentional.
    #[test]
    fn description_comment_with_display_line_zero_does_not_inject_anywhere() {
        let comment = make_description_comment(Some(0), Severity::Note);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = description_comment_to_inline(&comment, 0, now)
            .expect("conversion succeeds — display_line=0 is a valid Option<u32>");
        assert_eq!(inline.target_line, Some(0));

        let view = DiffView::from_description("hello\nworld");
        // Description lines are 1-based (1, 2). target_line=0 matches none.
        let original_len = view.lines.len();
        let augmented = view.with_inline_comments(&[inline]);
        assert_eq!(
            augmented.lines.len(),
            original_len,
            "display_line=0 must not inject anywhere — pinning silent-drop"
        );
    }

    // -- T-G5-none: display_line=None (re-anchored stale that lost its line).
    //   `with_inline_comments` matches only `Some(line_num)`, so a
    //   `target_line: None` inline is silently dropped from the rendered view.
    //   Pin the boundary so a future change is intentional.
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

    // pair_rows: a single Removed followed by a single Added pairs by index.
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

    // pair_rows: a Removed run pairs index-by-index with the following Added run.
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

    // pair_rows: extra Added lines beyond the Removed run produce
    // right-only rows for the unpaired tail.
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

    // pair_rows: extra Removed lines beyond the Added run produce
    // left-only rows for the unpaired tail.
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

    // pair_rows: an Added run with no preceding Removed run is fully right-only.
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

    // pair_rows: a Removed run with no following Added run is fully left-only.
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

    // pair_rows: empty input produces no rows.
    #[test]
    fn pair_rows_empty_input() {
        assert!(pair_rows(&[]).is_empty());
    }

    // pair_rows: a hunk header Spans; the context lines that follow self-pair.
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

    // pair_rows: an all-removed file produces N left-only Pair rows.
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
                    }
                ),
                "expected Pair{{ left: Some, right: None }}, got {r:?}"
            );
        }
    }

    // pair_rows: an all-added file produces N right-only Pair rows.
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
                    }
                ),
                "expected Pair{{ left: None, right: Some }}, got {r:?}"
            );
        }
    }

    // pair_rows: an inline comment row that lands between a `-` run and a
    // `+` run prevents pairing across the comment. The `-` run becomes
    // left-only, the comment is Spanning, and the `+` run becomes right-only.
    #[test]
    fn pair_rows_comment_breaks_run_pairing() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old"),
            line(
                RenderedLineKind::InlineCommentMeta { comment_index: 0 },
                "┃ ● note",
            ),
            line(RenderedLineKind::Added, "new"),
        ];
        let rows = pair_rows(&lines);
        // Removed (no following Added in same run) -> left-only.
        // InlineCommentMeta -> Spanning.
        // Added (no preceding Removed in same run) -> right-only.
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

    // DiffView: from_file populates paired_rows alongside lines so the
    // renderer never has to recompute. The sample has lines in order
    // [HunkHeader, Context, Added, Removed], so the walker emits
    // HunkHeader(Spanning), Context(self-Pair), Added-only Pair, then
    // Removed-only Pair — pairing only happens when a Removed run is
    // immediately followed by an Added run.
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

    // Bug fix: Context lines must classify as `Pair { Some(idx), Some(idx) }`,
    // not `Spanning`. A Spanning Context row paints across the full body width
    // (gutter + both columns), causing long context text to bleed past the
    // gutter into the right column when the right column is otherwise empty.
    // Self-pairing forces the renderer to truncate each side independently to
    // `side_width`.
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

    // T2 — Multi-hunk pairing isolation: the last `-` of hunk A must NOT pair
    // with the first `+` of hunk B. The HunkHeader / HunkSeparator rows that
    // sit between hunks are non-Removed/non-Added kinds, so the run walker
    // naturally breaks across them. Pin the regression.
    #[test]
    fn pair_rows_does_not_pair_across_hunk_boundary() {
        let lines = vec![
            // Hunk A
            line(RenderedLineKind::HunkHeader, "@@ -1,1 +1,1 @@"),
            line(RenderedLineKind::Removed, "old_A"),
            line(RenderedLineKind::HunkSeparator, ""),
            // Hunk B
            line(RenderedLineKind::HunkHeader, "@@ -10,1 +10,1 @@"),
            line(RenderedLineKind::Added, "new_B"),
        ];
        let rows = pair_rows(&lines);
        // 5 rows: HunkHeader (Spanning), Removed-only Pair, HunkSeparator
        // (Spanning), HunkHeader (Spanning), Added-only Pair.
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

    // T6 — Multi-line comment-broken pairing: a comment between two `-` lines
    // and two `+` lines splits each run, producing four left-only / right-only
    // Pair rows around the Spanning comment row. Pins option 2 (comment
    // breaks the run) which is what `pair_rows` does today.
    #[test]
    fn pair_rows_comment_in_middle_of_runs_breaks_each_side() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old1"),
            line(RenderedLineKind::Removed, "old2"),
            line(
                RenderedLineKind::InlineCommentMeta { comment_index: 0 },
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

    // DiffView: with_inline_comments rebuilds paired_rows so a freshly
    // injected comment row appears in the side-by-side projection.
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
            comment_index: 0,
        };
        let augmented = view.with_inline_comments(&[comment]);
        assert!(
            augmented.paired_rows.len() > before,
            "comment injection should add at least one paired row"
        );
    }

    fn make_change_comment(
        change_id: &crate::change_id::ChangeId,
        severity: Severity,
        body: &str,
        created_at: time::OffsetDateTime,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: change_id.clone(),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at,
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
        }
    }

    #[test]
    fn change_comment_to_inline_returns_some_for_matching_change_id() {
        let change_id = crate::change_id::ChangeId::parse(&"a".repeat(32)).unwrap();
        let comment = make_change_comment(
            &change_id,
            Severity::Required,
            "split this commit",
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline =
            change_comment_to_inline(&comment, 4, &change_id, now).expect("matching change_id");
        assert_eq!(inline.source_line, None);
        assert_eq!(inline.target_line, None);
        assert_eq!(inline.severity, Severity::Required);
        assert_eq!(inline.comment_index, 4);
        assert_eq!(inline.body_lines, vec!["split this commit".to_owned()]);
    }

    #[test]
    fn change_comments_append_to_description_view_with_separator() {
        let change_id = crate::change_id::ChangeId::parse(&"a".repeat(32)).unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH;

        let desc_view = DiffView::from_description("first description line");
        let desc_inline = description_comment_to_inline(
            &make_description_comment(Some(1), Severity::Note),
            0,
            now,
        )
        .expect("description anchor yields Some");
        let change_inline = change_comment_to_inline(
            &make_change_comment(&change_id, Severity::Required, "split", now),
            1,
            &change_id,
            now,
        )
        .expect("matching change_id");
        let augmented_desc = desc_view
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
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let file_view = DiffView::from_file(&sample_modified());
        let line_inline = comment_to_inline(
            &make_line_comment("foo.txt", Severity::Note),
            2,
            Some(std::path::Path::new("foo.txt")),
            now,
        )
        .expect("matching file path");
        let augmented_file = file_view.with_inline_comments(&[line_inline]);
        let change_metas = augmented_file
            .lines
            .iter()
            .filter(|l| {
                matches!(l.kind, RenderedLineKind::InlineCommentMeta { .. })
                    && l.source_line.is_none()
                    && l.target_line.is_none()
            })
            .count();
        assert_eq!(change_metas, 0);
    }

    // The "(no description)" placeholder is itself signal that what follows is
    // not anchored to any description line; an additional separator would be
    // visual noise.
    #[test]
    fn change_comments_skip_separator_when_description_is_empty() {
        let change_id = crate::change_id::ChangeId::parse(&"a".repeat(32)).unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let inline = change_comment_to_inline(
            &make_change_comment(&change_id, Severity::Note, "body", now),
            0,
            &change_id,
            now,
        )
        .expect("matching change_id");
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
}
