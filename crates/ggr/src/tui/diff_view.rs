//! Diff view data model for ggr.
use std::ops::Range;

use local_review_core::diff::{DiffFile, Hunk, Line, LineKind};

#[derive(Debug, Clone)]
pub(crate) struct DiffView {
    pub(crate) title: String,
    pub(crate) lines: Vec<RenderedLine>,
    /// Side-by-side row projection: each `PairedRow` maps to one terminal row.
    /// Removed lines go left, Added lines go right; Context lines self-pair.
    /// Computed eagerly and kept in sync with `lines`.
    pub(crate) paired_rows: Vec<PairedRow>,
}

#[derive(Debug, Clone)]
pub(crate) struct RenderedLine {
    pub(crate) kind: RenderedLineKind,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderedLineKind {
    HunkHeader,
    HunkSeparator,
    Context,
    Added,
    Removed,
    Notice,
}

/// One row of the side-by-side diff view.
///
/// `Spanning(idx)` rows (hunk headers, separators, notices) render across the
/// full body width. `Pair { left, right }` rows put the Removed line in the
/// left column and the Added line in the right column; either may be `None`
/// for unpaired tails. Context lines self-pair (`left == right == Some(idx)`)
/// so each column truncates independently to `side_width`.
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
}

/// Build the side-by-side row projection from a unified `lines` sequence.
///
/// Pairing rule: a contiguous `Removed` run immediately followed by a
/// contiguous `Added` run pairs index-by-index. Unequal-length runs leave
/// a `None` cell on the shorter side. `Context` lines self-pair so each
/// column truncates independently. All other kinds (`HunkHeader`,
/// `HunkSeparator`, `Notice`) produce a `Spanning` row.
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
            let added_start = idx;
            while idx < lines.len() && matches!(lines[idx].kind, RenderedLineKind::Added) {
                idx += 1;
            }
            push_paired_run(&mut rows, added_start..added_start, added_start..idx);
        } else if matches!(kind, RenderedLineKind::Context) {
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

fn push_paired_run(rows: &mut Vec<PairedRow>, removed: Range<usize>, added: Range<usize>) {
    let max = removed.len().max(added.len());
    for n in 0..max {
        let left = removed.start.checked_add(n).filter(|i| *i < removed.end);
        let right = added.start.checked_add(n).filter(|i| *i < added.end);
        rows.push(PairedRow::Pair { left, right });
    }
}

fn render_title(file: &DiffFile) -> String {
    match file {
        DiffFile::Modified { path, .. } => path.display().to_string(),
        DiffFile::Added { path, .. } => format!("{} (added)", path.display()),
        DiffFile::Removed { path, .. } => format!("{} (removed)", path.display()),
        DiffFile::Renamed { from, to, .. } => format!("{} -> {}", from.display(), to.display()),
        DiffFile::Binary { path } => format!("{} (binary)", path.display()),
    }
}

fn render_lines(file: &DiffFile) -> Vec<RenderedLine> {
    if let DiffFile::Binary { .. } = file {
        return vec![RenderedLine {
            kind: RenderedLineKind::Notice,
            text: "Binary file not shown".to_owned(),
        }];
    }

    let mut output = Vec::new();
    let hunks = file.hunks();
    if hunks.is_empty() {
        output.push(RenderedLine {
            kind: RenderedLineKind::Notice,
            text: "No textual changes in this file.".to_owned(),
        });
        return output;
    }

    for (index, hunk) in hunks.iter().enumerate() {
        if index > 0 {
            output.push(RenderedLine {
                kind: RenderedLineKind::HunkSeparator,
                text: String::new(),
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
    });
    for line in &hunk.lines {
        output.push(render_line(line));
    }
}

fn render_line(line: &Line) -> RenderedLine {
    let kind = match line.kind {
        LineKind::Context => RenderedLineKind::Context,
        LineKind::Added => RenderedLineKind::Added,
        LineKind::Removed => RenderedLineKind::Removed,
    };
    RenderedLine {
        kind,
        text: line.text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_review_core::diff::{DiffFile, Hunk, Line, LineKind};
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
    fn renders_renamed_title() {
        let file = DiffFile::Renamed {
            from: PathBuf::from("old.rs"),
            to: PathBuf::from("new.rs"),
            hunks: vec![],
        };
        let view = DiffView::from_file(&file);
        assert_eq!(view.title, "old.rs -> new.rs");
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
        let sep_count = view
            .lines
            .iter()
            .filter(|l| l.kind == RenderedLineKind::HunkSeparator)
            .count();
        assert_eq!(sep_count, 1);
    }

    fn line(kind: RenderedLineKind, text: &str) -> RenderedLine {
        RenderedLine {
            kind,
            text: text.to_owned(),
        }
    }

    #[test]
    fn pair_rows_pairs_removed_with_following_added() {
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
                right: Some(2)
            }
        );
    }

    #[test]
    fn pair_rows_context_self_pairs() {
        let lines = vec![
            line(RenderedLineKind::Context, "ctx"),
            line(RenderedLineKind::Context, "ctx2"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: Some(0)
            }
        );
        assert_eq!(
            rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(1)
            }
        );
    }

    #[test]
    fn pair_rows_added_only_is_right_only() {
        let lines = vec![line(RenderedLineKind::Added, "new")];
        let rows = pair_rows(&lines);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: None,
                right: Some(0)
            }
        );
    }

    #[test]
    fn pair_rows_removed_only_is_left_only() {
        let lines = vec![line(RenderedLineKind::Removed, "old")];
        let rows = pair_rows(&lines);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: None
            }
        );
    }

    #[test]
    fn pair_rows_does_not_pair_across_hunk_boundary() {
        let lines = vec![
            line(RenderedLineKind::Removed, "old"),
            line(RenderedLineKind::HunkSeparator, ""),
            line(RenderedLineKind::Added, "new"),
        ];
        let rows = pair_rows(&lines);
        assert_eq!(
            rows[0],
            PairedRow::Pair {
                left: Some(0),
                right: None
            }
        );
        assert_eq!(rows[1], PairedRow::Spanning(1));
        assert_eq!(
            rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(2)
            }
        );
    }

    #[test]
    fn from_file_populates_paired_rows() {
        let view = DiffView::from_file(&sample_modified());
        // [HunkHeader(Span), Context(self-pair), Added(right-only), Removed(left-only)]
        assert_eq!(view.paired_rows.len(), 4);
        assert_eq!(view.paired_rows[0], PairedRow::Spanning(0));
        assert_eq!(
            view.paired_rows[1],
            PairedRow::Pair {
                left: Some(1),
                right: Some(1)
            }
        );
        assert_eq!(
            view.paired_rows[2],
            PairedRow::Pair {
                left: None,
                right: Some(2)
            }
        );
        assert_eq!(
            view.paired_rows[3],
            PairedRow::Pair {
                left: Some(3),
                right: None
            }
        );
    }
}
