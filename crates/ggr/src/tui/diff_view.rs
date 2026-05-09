//! Diff view rendering for ggr Phase 1 (read-only, no comments).
use local_review_core::diff::{DiffFile, Hunk, Line, LineKind};

#[derive(Debug, Clone)]
pub(crate) struct DiffView {
    pub(crate) title: String,
    pub(crate) lines: Vec<RenderedLine>,
}

#[derive(Debug, Clone)]
pub(crate) struct RenderedLine {
    pub(crate) kind: RenderedLineKind,
    pub(crate) text: String,
    /// 1-based old-side line number. `None` for added lines, headers, notices.
    pub(crate) source_line: Option<u32>,
    /// 1-based new-side line number. `None` for removed lines, headers, notices.
    pub(crate) target_line: Option<u32>,
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

impl DiffView {
    pub(crate) fn from_file(file: &DiffFile) -> Self {
        let title = render_title(file);
        let lines = render_lines(file);
        Self { title, lines }
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
            source_line: None,
            target_line: None,
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
        source_line: line.source_line,
        target_line: line.target_line,
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

    #[test]
    fn line_numbers_preserved() {
        let view = DiffView::from_file(&sample_modified());
        assert_eq!(view.lines[1].source_line, Some(1));
        assert_eq!(view.lines[1].target_line, Some(1));
        assert_eq!(view.lines[2].source_line, None);
        assert_eq!(view.lines[2].target_line, Some(2));
        assert_eq!(view.lines[3].source_line, Some(2));
        assert_eq!(view.lines[3].target_line, None);
    }
}
