use std::path::{Path, PathBuf};

use unidiff::{PatchSet, PatchedFile};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffFile {
    Modified {
        path: PathBuf,
        hunks: Vec<Hunk>,
    },
    Added {
        path: PathBuf,
        hunks: Vec<Hunk>,
    },
    Removed {
        path: PathBuf,
        hunks: Vec<Hunk>,
    },
    Renamed {
        from: PathBuf,
        to: PathBuf,
        hunks: Vec<Hunk>,
    },
    Binary {
        path: PathBuf,
    },
}

impl DiffFile {
    pub fn display_path(&self) -> &Path {
        match self {
            Self::Modified { path, .. }
            | Self::Added { path, .. }
            | Self::Removed { path, .. }
            | Self::Binary { path } => path,
            Self::Renamed { to, .. } => to,
        }
    }

    pub fn hunks(&self) -> &[Hunk] {
        match self {
            Self::Modified { hunks, .. }
            | Self::Added { hunks, .. }
            | Self::Removed { hunks, .. }
            | Self::Renamed { hunks, .. } => hunks,
            Self::Binary { .. } => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub function_context: Option<String>,
    pub source_start: u32,
    pub source_length: u32,
    pub target_start: u32,
    pub target_length: u32,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    pub source_line: Option<u32>,
    pub target_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

pub fn parse(input: &str) -> Result<Diff> {
    let mut files = Vec::new();

    for section in split_into_sections(input) {
        if let Some(path) = detect_binary(&section) {
            files.push(DiffFile::Binary { path });
            continue;
        }

        let mut patch = PatchSet::new();
        patch.parse(&section).map_err(|e| Error::DiffParse {
            file: section_file_hint(&section),
            message: format!("unidiff: {e}"),
        })?;

        let pf_list: Vec<_> = patch.files().iter().collect();

        if pf_list.is_empty() {
            // unidiff produces no PatchedFile for sections that have no hunk
            // content: pure renames with similarity 100%, and empty file
            // creates/deletes (no `---`/`+++` headers because there is no
            // textual content to diff). Detect and emit those manually;
            // everything else is a parse failure we surface rather than
            // silently drop.
            if let Some(file) = detect_metadata_only_section(&section) {
                files.push(file);
                continue;
            }
            return Err(Error::DiffParse {
                file: section_file_hint(&section),
                message: "section produced no patched files and has no recognised metadata-only \
                          shape (rename, empty add, empty delete)"
                    .to_owned(),
            });
        }

        for pf in pf_list {
            files.push(convert_patched_file(pf)?);
        }
    }

    Ok(Diff { files })
}

fn split_into_sections(input: &str) -> Vec<String> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    let mut current: Option<Vec<&str>> = None;

    for line in input.lines() {
        if line.starts_with("diff --git ") {
            if let Some(taken) = current.take() {
                sections.push(taken);
            }
            current = Some(vec![line]);
        } else if let Some(buf) = current.as_mut() {
            buf.push(line);
        }
    }

    if let Some(taken) = current {
        sections.push(taken);
    }

    sections.into_iter().map(|lines| lines.join("\n")).collect()
}

fn detect_binary(section: &str) -> Option<PathBuf> {
    let mut diff_paths: Option<(&str, &str)> = None;

    for line in section.lines() {
        // Only the diff header carries binary-marker lines; once a hunk
        // (`@@ ...`) starts, any subsequent line is `+`/`-`/` ` content and
        // could legitimately contain the literal strings below — including
        // this file's own source.
        if line.starts_with("@@ ") {
            break;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            diff_paths = parse_diff_git_line(rest);
        } else if line.starts_with("Binary files ")
            || line.starts_with("Binary file ")
            || line == "GIT binary patch"
        {
            return diff_paths.map(|(_, b)| PathBuf::from(strip_b_prefix(b)));
        }
    }

    None
}

/// Detect a section with no hunk content but a recognisable metadata-only
/// shape: pure rename, empty file create, or empty file delete.
///
/// `unidiff` returns no `PatchedFile` for these because there are no
/// `---`/`+++` headers (no textual content to diff). We synthesise a
/// `DiffFile` with empty hunks from the header lines instead. Rename takes
/// priority over add/delete because a pure rename can carry both `similarity
/// index 100%` and a file mode header.
fn detect_metadata_only_section(section: &str) -> Option<DiffFile> {
    let mut rename_from: Option<PathBuf> = None;
    let mut rename_to: Option<PathBuf> = None;
    let mut new_file = false;
    let mut deleted_file = false;
    let mut is_binary = false;
    let mut diff_target: Option<PathBuf> = None;

    for line in section.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some((_, b)) = parse_diff_git_line(rest) {
                diff_target = Some(PathBuf::from(strip_b_prefix(b)));
            }
        } else if let Some(path) = line.strip_prefix("rename from ") {
            rename_from = Some(PathBuf::from(path));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            rename_to = Some(PathBuf::from(path));
        } else if line.starts_with("new file mode ") {
            new_file = true;
        } else if line.starts_with("deleted file mode ") {
            deleted_file = true;
        } else if line.starts_with("Binary files ") {
            is_binary = true;
        }
    }

    if let (Some(from), Some(to)) = (rename_from, rename_to) {
        return Some(DiffFile::Renamed {
            from,
            to,
            hunks: vec![],
        });
    }

    let path = diff_target?;

    if new_file {
        return Some(DiffFile::Added {
            path,
            hunks: vec![],
        });
    }
    if deleted_file {
        return Some(DiffFile::Removed {
            path,
            hunks: vec![],
        });
    }
    if is_binary {
        // Binary files have no hunk content and cannot carry inline comments;
        // emit a Modified entry with empty hunks so the file appears in the
        // file picker without blocking the rest of the diff parse.
        return Some(DiffFile::Binary { path });
    }

    None
}

/// Parse the path pair from a `diff --git a/X b/Y` header.
///
/// Git does not quote spaces in paths; instead it relies on the symmetric
/// `a/` and `b/` prefix structure. For non-renamed files both sides are
/// identical so we find the last occurrence of ` b/` to split the string.
/// This covers files with spaces in their names. For renames the paths differ
/// and git may use a tab separator, but the `unidiff` library handles the hunk
/// bodies — we only need the path for error messages and binary detection, so
/// we use the target (`b/`) side.
///
/// Limitation: paths containing the literal substring ` b/` in the middle will
/// be split incorrectly. This is an inherent ambiguity in the git diff header
/// format without quoting.
fn parse_diff_git_line(rest: &str) -> Option<(&str, &str)> {
    let b_marker = " b/";
    let split_pos = rest.rfind(b_marker)?;
    let a = &rest[..split_pos];
    let b = &rest[split_pos + 1..];
    Some((a, b))
}

fn section_file_hint(section: &str) -> PathBuf {
    section
        .lines()
        .find_map(|line| line.strip_prefix("diff --git "))
        .and_then(parse_diff_git_line)
        .map(|(_, b)| PathBuf::from(strip_b_prefix(b)))
        .unwrap_or_else(|| PathBuf::from("<unknown>"))
}

fn convert_patched_file(pf: &PatchedFile) -> Result<DiffFile> {
    let file_path = PathBuf::from(pf.path());

    let hunks = pf
        .hunks()
        .iter()
        .map(|h| convert_hunk(h, &file_path))
        .collect::<Result<Vec<_>>>()?;

    if pf.is_added_file() {
        debug_assert!(
            hunks
                .iter()
                .all(|h| h.lines.iter().all(|l| l.kind == LineKind::Added)),
            "Added file must only contain Added lines; type-level enforcement is a planned refactor"
        );
        return Ok(DiffFile::Added {
            path: file_path,
            hunks,
        });
    }

    if pf.is_removed_file() {
        debug_assert!(
            hunks.iter().all(|h| h.lines.iter().all(|l| l.kind == LineKind::Removed)),
            "Removed file must only contain Removed lines; type-level enforcement is a planned refactor"
        );
        return Ok(DiffFile::Removed {
            path: file_path,
            hunks,
        });
    }

    let source = strip_a_prefix(&pf.source_file);
    let target = strip_b_prefix(&pf.target_file);

    if source == target {
        Ok(DiffFile::Modified {
            path: PathBuf::from(target),
            hunks,
        })
    } else {
        Ok(DiffFile::Renamed {
            from: PathBuf::from(source),
            to: PathBuf::from(target),
            hunks,
        })
    }
}

fn strip_a_prefix(raw: &str) -> &str {
    raw.strip_prefix("a/").unwrap_or(raw)
}

fn strip_b_prefix(raw: &str) -> &str {
    raw.strip_prefix("b/").unwrap_or(raw)
}

fn convert_hunk(h: &unidiff::Hunk, file: &Path) -> Result<Hunk> {
    let function_context = if h.section_header.is_empty() {
        None
    } else {
        Some(h.section_header.clone())
    };

    let source_start = u32::try_from(h.source_start).map_err(|_| Error::DiffParse {
        file: file.to_owned(),
        message: format!("source_start {} exceeds u32::MAX", h.source_start),
    })?;
    let source_length = u32::try_from(h.source_length).map_err(|_| Error::DiffParse {
        file: file.to_owned(),
        message: format!("source_length {} exceeds u32::MAX", h.source_length),
    })?;
    let target_start = u32::try_from(h.target_start).map_err(|_| Error::DiffParse {
        file: file.to_owned(),
        message: format!("target_start {} exceeds u32::MAX", h.target_start),
    })?;
    let target_length = u32::try_from(h.target_length).map_err(|_| Error::DiffParse {
        file: file.to_owned(),
        message: format!("target_length {} exceeds u32::MAX", h.target_length),
    })?;

    let header = render_hunk_header(
        source_start,
        source_length,
        target_start,
        target_length,
        function_context.as_deref(),
    );

    let lines = h
        .lines()
        .iter()
        .map(|l| convert_line(l, file))
        .collect::<Result<Vec<_>>>()?;

    Ok(Hunk {
        header,
        function_context,
        source_start,
        source_length,
        target_start,
        target_length,
        lines,
    })
}

fn render_hunk_header(
    src_start: u32,
    src_len: u32,
    tgt_start: u32,
    tgt_len: u32,
    function_context: Option<&str>,
) -> String {
    let src = format_range(src_start, src_len);
    let tgt = format_range(tgt_start, tgt_len);
    match function_context {
        Some(ctx) => format!("@@ -{src} +{tgt} @@ {ctx}"),
        None => format!("@@ -{src} +{tgt} @@"),
    }
}

fn format_range(start: u32, length: u32) -> String {
    if length == 1 {
        format!("{start}")
    } else {
        format!("{start},{length}")
    }
}

fn convert_line(l: &unidiff::Line, file: &Path) -> Result<Line> {
    let kind = if l.is_added() {
        LineKind::Added
    } else if l.is_removed() {
        LineKind::Removed
    } else if l.is_context() {
        LineKind::Context
    } else {
        return Err(Error::DiffParse {
            file: file.to_owned(),
            message: format!("unrecognized line type: {:?}", l.line_type),
        });
    };

    let source_line = l
        .source_line_no
        .map(u32::try_from)
        .transpose()
        .map_err(|_| Error::DiffParse {
            file: file.to_owned(),
            message: "source_line_no exceeds u32::MAX".to_owned(),
        })?;
    let target_line = l
        .target_line_no
        .map(u32::try_from)
        .transpose()
        .map_err(|_| Error::DiffParse {
            file: file.to_owned(),
            message: "target_line_no exceeds u32::MAX".to_owned(),
        })?;

    Ok(Line {
        kind,
        text: l.value.clone(),
        source_line,
        target_line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_DIFF: &str = "diff --git a/foo.txt b/foo.txt\n\
index abc..def 100644\n\
--- a/foo.txt\n\
+++ b/foo.txt\n\
@@ -1,3 +1,4 @@ ctx\n\
 line1\n\
-line2\n\
+line2-modified\n\
+line3-new\n\
 line4\n";

    #[test]
    fn parses_modified_file() {
        let diff = parse(SIMPLE_DIFF).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Modified { path, hunks } = &diff.files[0] else {
            panic!("expected Modified, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("foo.txt"));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].function_context.as_deref(), Some("ctx"));
        assert_eq!(hunks[0].header, "@@ -1,3 +1,4 @@ ctx");
        assert_eq!(hunks[0].lines.len(), 5);
    }

    #[test]
    fn parses_added_file() {
        let input = "diff --git a/new.txt b/new.txt\n\
new file mode 100644\n\
index 0000000..abc\n\
--- /dev/null\n\
+++ b/new.txt\n\
@@ -0,0 +1,2 @@\n\
+line1\n\
+line2\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Added { path, hunks } = &diff.files[0] else {
            panic!("expected Added, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("new.txt"));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header, "@@ -0,0 +1,2 @@");
    }

    #[test]
    fn parses_removed_file() {
        let input = "diff --git a/old.txt b/old.txt\n\
deleted file mode 100644\n\
index abc..0000000\n\
--- a/old.txt\n\
+++ /dev/null\n\
@@ -1,2 +0,0 @@\n\
-line1\n\
-line2\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Removed { path, hunks } = &diff.files[0] else {
            panic!("expected Removed, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("old.txt"));
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn parses_renamed_file_with_changes() {
        let input = "diff --git a/old.rs b/new.rs\n\
similarity index 50%\n\
rename from old.rs\n\
rename to new.rs\n\
--- a/old.rs\n\
+++ b/new.rs\n\
@@ -10,3 +10,5 @@ impl Foo\n\
 a\n\
-b\n\
+c\n\
 d\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Renamed { from, to, hunks } = &diff.files[0] else {
            panic!("expected Renamed, got {:?}", diff.files[0]);
        };
        assert_eq!(from, &PathBuf::from("old.rs"));
        assert_eq!(to, &PathBuf::from("new.rs"));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].function_context.as_deref(), Some("impl Foo"));
    }

    #[test]
    fn parses_pure_rename_no_hunks() {
        let input = "diff --git a/old.rs b/new.rs\n\
similarity index 100%\n\
rename from old.rs\n\
rename to new.rs\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Renamed { from, to, hunks } = &diff.files[0] else {
            panic!("expected Renamed, got {:?}", diff.files[0]);
        };
        assert_eq!(from, &PathBuf::from("old.rs"));
        assert_eq!(to, &PathBuf::from("new.rs"));
        assert!(hunks.is_empty());
    }

    #[test]
    fn malformed_section_with_no_files_or_rename_returns_error() {
        let input = "diff --git a/foo.rs b/foo.rs\n\
old mode 100644\n\
new mode 100755\n";
        let result = parse(input);
        assert!(matches!(result, Err(Error::DiffParse { .. })));
    }

    #[test]
    fn parses_empty_new_file_as_added_with_no_hunks() {
        // jj/git emits no `---`/`+++` headers for a brand-new zero-byte file,
        // so unidiff returns no PatchedFile. We must still surface it as
        // `Added` so the TUI can list it (with the "No textual changes"
        // notice) instead of erroring.
        let input = "diff --git a/empty.rs b/empty.rs\n\
new file mode 100644\n\
index 0000000000..e69de29bb2\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Added { path, hunks } = &diff.files[0] else {
            panic!("expected Added, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("empty.rs"));
        assert!(hunks.is_empty());
    }

    #[test]
    fn parses_empty_deleted_file_as_removed_with_no_hunks() {
        let input = "diff --git a/empty.rs b/empty.rs\n\
deleted file mode 100644\n\
index e69de29bb2..0000000000\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Removed { path, hunks } = &diff.files[0] else {
            panic!("expected Removed, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("empty.rs"));
        assert!(hunks.is_empty());
    }

    #[test]
    fn parses_binary_file_as_binary_variant() {
        let input = "diff --git a/foo.bin b/foo.bin\n\
index abc..def 100644\n\
Binary files a/foo.bin and b/foo.bin differ\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 1);
        let DiffFile::Binary { path } = &diff.files[0] else {
            panic!("expected Binary, got {:?}", diff.files[0]);
        };
        assert_eq!(path, &PathBuf::from("foo.bin"));
    }

    #[test]
    fn parses_multifile_diff_preserving_order() {
        let input = "diff --git a/a.txt b/a.txt\n\
index 1..2 100644\n\
--- a/a.txt\n\
+++ b/a.txt\n\
@@ -1 +1 @@\n\
-old\n\
+new\n\
diff --git a/b.bin b/b.bin\n\
Binary files a/b.bin and b/b.bin differ\n\
diff --git a/c.txt b/c.txt\n\
index 3..4 100644\n\
--- a/c.txt\n\
+++ b/c.txt\n\
@@ -1 +1 @@\n\
-foo\n\
+bar\n";
        let diff = parse(input).unwrap();
        assert_eq!(diff.files.len(), 3);
        assert!(matches!(diff.files[0], DiffFile::Modified { .. }));
        assert!(matches!(diff.files[1], DiffFile::Binary { .. }));
        assert!(matches!(diff.files[2], DiffFile::Modified { .. }));
    }

    #[test]
    fn empty_input_yields_empty_diff() {
        let diff = parse("").unwrap();
        assert!(diff.files.is_empty());
    }

    #[test]
    fn single_line_hunk_omits_length_in_header() {
        let input = "diff --git a/foo.txt b/foo.txt\n\
index 1..2 100644\n\
--- a/foo.txt\n\
+++ b/foo.txt\n\
@@ -5 +5 @@\n\
-old\n\
+new\n";
        let diff = parse(input).unwrap();
        let DiffFile::Modified { hunks, .. } = &diff.files[0] else {
            panic!("expected Modified, got {:?}", diff.files[0]);
        };
        assert_eq!(hunks[0].header, "@@ -5 +5 @@");
    }

    #[test]
    fn classifies_line_kinds() {
        let diff = parse(SIMPLE_DIFF).unwrap();
        let hunks = diff.files[0].hunks();
        let lines = &hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[2].kind, LineKind::Added);
        assert_eq!(lines[3].kind, LineKind::Added);
        assert_eq!(lines[4].kind, LineKind::Context);
    }

    #[test]
    fn line_numbers_correct_for_simple_diff() {
        let diff = parse(SIMPLE_DIFF).unwrap();
        let lines = &diff.files[0].hunks()[0].lines;
        // Context line1: source=1, target=1
        assert_eq!(lines[0].source_line, Some(1));
        assert_eq!(lines[0].target_line, Some(1));
        // Removed line2: source=2, target=None
        assert_eq!(lines[1].source_line, Some(2));
        assert_eq!(lines[1].target_line, None);
        // Added line2-modified: source=None, target=2
        assert_eq!(lines[2].source_line, None);
        assert_eq!(lines[2].target_line, Some(2));
        // Added line3-new: source=None, target=3
        assert_eq!(lines[3].source_line, None);
        assert_eq!(lines[3].target_line, Some(3));
        // Context line4: source=3, target=4
        assert_eq!(lines[4].source_line, Some(3));
        assert_eq!(lines[4].target_line, Some(4));
    }

    #[test]
    fn line_text_does_not_include_diff_prefix() {
        let diff = parse(SIMPLE_DIFF).unwrap();
        let lines = &diff.files[0].hunks()[0].lines;
        assert_eq!(lines[0].text, "line1");
        assert_eq!(lines[1].text, "line2");
        assert_eq!(lines[2].text, "line2-modified");
    }

    #[test]
    fn detect_binary_returns_none_for_non_binary() {
        assert!(detect_binary(SIMPLE_DIFF).is_none());
    }

    #[test]
    fn detect_binary_ignores_marker_strings_inside_hunk_content() {
        // Regression: the binary-detection logic used to scan every line of
        // the section, including `+`/`-` content. A new file whose body
        // happened to contain "GIT binary patch" or "Binary files " (such
        // as this very parser source) self-misclassified as binary.
        let section = "\
diff --git a/src/diff.rs b/src/diff.rs
new file mode 100644
--- /dev/null
+++ b/src/diff.rs
@@ -0,0 +1,3 @@
+// Detect the GIT binary patch marker in real git output.
+// Also detect Binary files a/X and b/X differ.
+let _ = ();
";
        assert!(detect_binary(section).is_none());
    }

    #[test]
    fn detect_binary_handles_singular_binary_file_phrasing() {
        let section = "diff --git a/foo.bin b/foo.bin\nBinary file foo.bin matches\n";
        let path = detect_binary(section).expect("expected binary path");
        assert_eq!(path, PathBuf::from("foo.bin"));
    }

    #[test]
    fn detect_binary_handles_git_binary_patch_marker() {
        let section = "diff --git a/foo.bin b/foo.bin\nGIT binary patch\n<blob data>\n";
        let path = detect_binary(section).expect("expected binary path");
        assert_eq!(path, PathBuf::from("foo.bin"));
    }

    #[test]
    fn section_file_hint_picks_target_path_from_diff_git_header() {
        let section = "diff --git a/old.txt b/new.txt\n--- a/old.txt\n+++ b/new.txt\n";
        assert_eq!(section_file_hint(section), PathBuf::from("new.txt"));
    }

    #[test]
    fn section_file_hint_falls_back_to_unknown_when_no_diff_git() {
        let section = "this section has no diff --git header at all\n";
        assert_eq!(section_file_hint(section), PathBuf::from("<unknown>"));
    }

    #[test]
    fn parse_diff_git_line_handles_path_with_space() {
        let rest = "a/path with spaces.txt b/path with spaces.txt";
        let (a, b) = parse_diff_git_line(rest).expect("should parse");
        assert_eq!(a, "a/path with spaces.txt");
        assert_eq!(b, "b/path with spaces.txt");
    }

    #[test]
    fn render_hunk_header_omits_function_context_when_empty() {
        let header = render_hunk_header(1, 3, 1, 4, None);
        assert_eq!(header, "@@ -1,3 +1,4 @@");
    }

    #[test]
    fn render_hunk_header_includes_function_context_when_present() {
        let header = render_hunk_header(1, 3, 1, 4, Some("impl Foo"));
        assert_eq!(header, "@@ -1,3 +1,4 @@ impl Foo");
    }

    #[test]
    fn format_range_uses_just_start_for_single_line() {
        assert_eq!(format_range(5, 1), "5");
    }

    #[test]
    fn format_range_includes_zero_length() {
        assert_eq!(format_range(0, 0), "0,0");
    }

    #[test]
    fn diff_file_display_path_picks_target_for_renamed() {
        let file = DiffFile::Renamed {
            from: PathBuf::from("old.rs"),
            to: PathBuf::from("new.rs"),
            hunks: vec![],
        };
        assert_eq!(file.display_path(), Path::new("new.rs"));
    }

    #[test]
    fn diff_file_display_path_for_modified() {
        let file = DiffFile::Modified {
            path: PathBuf::from("foo.rs"),
            hunks: vec![],
        };
        assert_eq!(file.display_path(), Path::new("foo.rs"));
    }

    #[test]
    fn diff_file_display_path_for_added() {
        let file = DiffFile::Added {
            path: PathBuf::from("foo.rs"),
            hunks: vec![],
        };
        assert_eq!(file.display_path(), Path::new("foo.rs"));
    }

    #[test]
    fn diff_file_display_path_for_removed() {
        let file = DiffFile::Removed {
            path: PathBuf::from("foo.rs"),
            hunks: vec![],
        };
        assert_eq!(file.display_path(), Path::new("foo.rs"));
    }

    #[test]
    fn diff_file_display_path_for_binary() {
        let file = DiffFile::Binary {
            path: PathBuf::from("foo.bin"),
        };
        assert_eq!(file.display_path(), Path::new("foo.bin"));
    }

    #[test]
    fn diff_file_hunks_returns_empty_slice_for_binary() {
        let file = DiffFile::Binary {
            path: PathBuf::from("foo.bin"),
        };
        assert!(file.hunks().is_empty());
    }

    #[test]
    fn input_with_no_diff_git_lines_yields_empty_diff() {
        let input = "this text has no diff markers at all\nsecond line\n";
        let diff = parse(input).unwrap();
        assert!(diff.files.is_empty());
    }
}
