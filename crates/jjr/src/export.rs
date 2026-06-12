use std::fmt::{self, Display, Write as _};
use std::io;
use std::path::Path;

use local_review_core::util::strip_controls;

use crate::comment::{Anchor, Comment, LineAnchor};
use crate::error::{JjrError, Result};
use crate::packet::severity_label;
use crate::stack::{ResolvedStack, StackEntry};
use crate::store;

/// Output format for `jjr export`.
#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub enum ExportFormat {
    #[default]
    Jsonl,
    Markdown,
}

impl Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jsonl => f.write_str("jsonl"),
            Self::Markdown => f.write_str("markdown"),
        }
    }
}

/// All comments collected for a single stack, structured for export rendering.
pub struct ExportData {
    pub revset: String,
    pub stack_comments: Vec<Comment>,
    pub per_change: Vec<ChangeExport>,
}

/// Comments for one change.
pub struct ChangeExport {
    pub entry: StackEntry,
    pub comments: Vec<Comment>,
}

/// Load all comments for the resolved stack.
///
/// Stack-scoped comments come from `_stack.jsonl` filtered by `revset_hash`.
/// Per-change comments come from each change's JSONL file. No filtering by
/// status — export emits everything on disk for changes IN the resolved stack,
/// including records with `status=Some(Status::Stale)` and
/// `status=Some(Status::Orphaned)`. Export is a backup / archive operation
/// (raw disk dump), distinct from the packet path which sanitizes for Claude.
///
/// Note: comment files for change IDs that are NOT in the resolved stack
/// (orphan files left behind after abandon/rebase) are NOT loaded by this
/// function. Use `jjr clear --orphaned` to manage those separately.
pub fn collect_export_data(
    data_home: &Path,
    repo_root: &Path,
    resolved: &ResolvedStack,
) -> Result<ExportData> {
    let stack_comments = store::load_stack_comments(data_home, repo_root, &resolved.revset_hash)?;

    let per_change = resolved
        .entries
        .iter()
        .map(|entry| {
            let comments = store::load_change_comments(data_home, repo_root, &entry.change_id)?;
            Ok(ChangeExport {
                entry: entry.clone(),
                comments,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(ExportData {
        revset: resolved.revset.clone(),
        stack_comments,
        per_change,
    })
}

/// Stack-scoped comments come first (sorted by `created_at`), then per-change
/// in stack order, within each change sorted by `created_at`. Same `ExportData`
/// always produces byte-identical output.
///
/// Returns `Err` when any comment fails to serialize. The error is wrapped in
/// `JjrError::Io` so the caller can use the standard error path.
pub fn render_export_jsonl(data: &ExportData) -> Result<String> {
    let mut out = String::new();

    let mut stack = data.stack_comments.clone();
    stack.sort_by_key(|c| c.created_at);
    for comment in &stack {
        write_jsonl_line(&mut out, comment)?;
    }

    for change in &data.per_change {
        let mut comments = change.comments.clone();
        comments.sort_by_key(|c| c.created_at);
        for comment in &comments {
            write_jsonl_line(&mut out, comment)?;
        }
    }

    Ok(out)
}

fn write_jsonl_line(out: &mut String, comment: &Comment) -> Result<()> {
    let line = serde_json::to_string(comment).map_err(|e| JjrError::Io {
        source: io::Error::other(e),
    })?;
    out.push_str(&line);
    out.push('\n');
    Ok(())
}

/// Render `ExportData` as human-readable Markdown.
///
/// The format is intentionally simple and byte-stable — same input always
/// produces the same bytes. Snapshot tests pin this contract.
pub fn render_export_markdown(data: &ExportData) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# Review export — {}", strip_controls(&data.revset));

    let mut stack = data.stack_comments.clone();
    stack.sort_by_key(|c| c.created_at);

    if !stack.is_empty() {
        out.push('\n');
        out.push_str("## Stack-level comments\n");
        for comment in &stack {
            out.push('\n');
            let _ = writeln!(out, "### [{}]", severity_label(comment.severity));
            out.push('\n');
            write_body_lines(&mut out, &comment.body);
        }
    }

    for change in &data.per_change {
        let mut comments = change.comments.clone();
        comments.sort_by_key(|c| c.created_at);
        if comments.is_empty() {
            continue;
        }

        out.push('\n');
        let _ = writeln!(
            out,
            "## {} — {}",
            change.entry.change_id.as_str(),
            strip_controls(&change.entry.description)
        );

        let change_scoped: Vec<&Comment> = comments
            .iter()
            .filter(|c| matches!(c.anchor, Anchor::Change { .. }))
            .collect();
        // Description-anchored records are intentionally excluded: markdown
        // export renders diff-line context only.
        let line_scoped: Vec<(&Comment, &LineAnchor)> = comments
            .iter()
            .filter_map(|c| {
                if let Anchor::Line { location, .. } = &c.anchor {
                    Some((c, location))
                } else {
                    None
                }
            })
            .collect();

        if !change_scoped.is_empty() {
            out.push('\n');
            out.push_str("### Change-level comments\n");
            for comment in change_scoped {
                out.push('\n');
                let _ = writeln!(out, "#### [{}]", severity_label(comment.severity));
                out.push('\n');
                write_body_lines(&mut out, &comment.body);
            }
        }

        if !line_scoped.is_empty() {
            write_line_comments(&mut out, &line_scoped);
        }
    }

    out
}

fn write_body_lines(out: &mut String, body: &str) {
    for line in body.lines() {
        let _ = writeln!(out, "> {}", strip_controls(line));
    }
}

fn write_line_comments(out: &mut String, comments: &[(&Comment, &LineAnchor)]) {
    out.push('\n');
    out.push_str("### Line-level comments\n");
    for (comment, location) in comments {
        out.push('\n');
        let line_num = location
            .new_line
            .or(location.old_line)
            .map_or_else(String::new, |n| n.to_string());
        let _ = writeln!(
            out,
            "#### [{}] {}:{}",
            severity_label(comment.severity),
            strip_controls(&location.file.display().to_string()),
            line_num,
        );
        let _ = writeln!(out, "Hunk: {}", strip_controls(&location.hunk_header));
        let _ = writeln!(out, "Target: {}", strip_controls(&location.target_text));
        out.push('\n');
        write_body_lines(out, &comment.body);
    }
}

/// Returns `true` when `data` has no comments at all.
pub fn is_empty(data: &ExportData) -> bool {
    if !data.stack_comments.is_empty() {
        return false;
    }
    data.per_change.iter().all(|c| c.comments.is_empty())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::comment::{Anchor, LineAnchor, SchemaVersion, Severity, Side, Status};
    use crate::stack::{ResolvedStack, RevsetHash, StackEntry};

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn make_stack_comment(body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_change_comment(change_id: &ChangeId, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: change_id.clone(),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Suggestion,
            created_at: datetime!(2026-04-29 10:01:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_line_comment(change_id: &ChangeId, body: &str) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("src/lib.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(10),
                    hunk_header: "@@ -8,3 +8,5 @@".to_owned(),
                    target_text: "let x = 1;".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity: Severity::Required,
            created_at: datetime!(2026-04-29 10:02:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_entry(change_id_str: &str, description: &str) -> StackEntry {
        StackEntry {
            change_id: cid(change_id_str),
            commit_id: CommitId::parse("aabbccdd11223344").unwrap(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn is_empty_true_when_no_comments() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![],
            }],
        };
        assert!(is_empty(&data));
    }

    #[test]
    fn is_empty_false_when_stack_comment_present() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment("cross-cutting")],
            per_change: vec![],
        };
        assert!(!is_empty(&data));
    }

    #[test]
    fn is_empty_false_when_change_comment_present() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![make_change_comment(&id, "change body")],
            }],
        };
        assert!(!is_empty(&data));
    }

    #[test]
    fn render_jsonl_empty_data_produces_empty_string() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![],
        };
        assert_eq!(render_export_jsonl(&data).unwrap(), "");
    }

    #[test]
    fn render_jsonl_stack_comment_produces_one_line() {
        let data = ExportData {
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![make_stack_comment("stack note")],
            per_change: vec![],
        };
        let out = render_export_jsonl(&data).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["scope"], "stack");
        assert_eq!(v["comment"], "stack note");
    }

    #[test]
    fn render_jsonl_ordering_stack_then_per_change() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![make_stack_comment("stack first")],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![make_change_comment(&id, "change second")],
            }],
        };
        let out = render_export_jsonl(&data).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v0["scope"], "stack");
        assert_eq!(v1["scope"], "change");
    }

    #[test]
    fn render_jsonl_parses_back_to_identical_comments() {
        let id = cid("abc12345");
        let change_comment = make_change_comment(&id, "change body");
        let line_comment = make_line_comment(&id, "line body");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment("stack")],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![change_comment, line_comment],
            }],
        };
        let out = render_export_jsonl(&data).unwrap();
        for line in out.lines() {
            let parsed: Comment = serde_json::from_str(line).unwrap();
            let re_serialized = serde_json::to_string(&parsed).unwrap();
            assert_eq!(line, re_serialized, "round-trip must be identity");
        }
    }

    #[test]
    fn render_jsonl_within_change_sorted_by_created_at() {
        let id = cid("abc12345");
        let mut earlier = make_change_comment(&id, "earlier");
        earlier.created_at = datetime!(2026-04-29 09:00:00 UTC);
        let mut later = make_line_comment(&id, "later");
        later.created_at = datetime!(2026-04-29 11:00:00 UTC);

        // Insert in reverse order to confirm sort.
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![later, earlier],
            }],
        };
        let out = render_export_jsonl(&data).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v0["comment"], "earlier");
    }

    #[test]
    fn render_markdown_header_contains_revset() {
        let data = ExportData {
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![],
            per_change: vec![],
        };
        let out = render_export_markdown(&data);
        assert!(
            out.starts_with("# Review export — trunk()..@\n"),
            "got: {out:?}"
        );
    }

    #[test]
    fn render_markdown_stack_section_present_when_comments_exist() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment("cross-cutting note")],
            per_change: vec![],
        };
        let out = render_export_markdown(&data);
        assert!(out.contains("## Stack-level comments\n"));
        assert!(out.contains("[NOTE]"));
        assert!(out.contains("> cross-cutting note\n"));
    }

    #[test]
    fn render_markdown_no_stack_section_when_no_stack_comments() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![],
        };
        let out = render_export_markdown(&data);
        assert!(!out.contains("## Stack-level comments"));
    }

    #[test]
    fn render_markdown_change_section_contains_change_id_and_desc() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "Add feature"),
                comments: vec![make_change_comment(&id, "too large")],
            }],
        };
        let out = render_export_markdown(&data);
        assert!(out.contains("## abc12345 — Add feature\n"));
        assert!(out.contains("### Change-level comments\n"));
        assert!(out.contains("[SUGGESTION]"));
        assert!(out.contains("> too large\n"));
    }

    #[test]
    fn render_markdown_line_comment_section() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![make_line_comment(&id, "fix this")],
            }],
        };
        let out = render_export_markdown(&data);
        assert!(out.contains("### Line-level comments\n"));
        assert!(out.contains("[REQUIRED] src/lib.rs:10\n"));
        assert!(out.contains("Hunk: @@ -8,3 +8,5 @@\n"));
        assert!(out.contains("Target: let x = 1;\n"));
        assert!(out.contains("> fix this\n"));
    }

    #[test]
    fn render_markdown_strips_controls_from_hunk_and_target() {
        let id = cid("abc12345");
        let mut comment = make_line_comment(&id, "fix this");
        let Anchor::Line {
            ref mut location, ..
        } = comment.anchor
        else {
            panic!("expected Line anchor");
        };
        // Embed an ANSI escape sequence in hunk_header, target_text, and file path.
        location.hunk_header = "\x1b[31m@@ -8,3 +8,5 @@\x1b[0m".to_owned();
        location.target_text = "\x1b[32mlet x = 1;\x1b[0m".to_owned();
        location.file = PathBuf::from("src/\x1b[33mlib\x1b[0m.rs");

        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "first"),
                comments: vec![comment],
            }],
        };
        let out = render_export_markdown(&data);
        assert!(
            !out.contains('\x1b'),
            "markdown output must not contain ESC characters; got: {out:?}"
        );
        assert!(
            out.contains("Hunk: [31m@@ -8,3 +8,5 @@[0m\n"),
            "hunk header content preserved after stripping ESC; got: {out:?}"
        );
        assert!(
            out.contains("Target: [32mlet x = 1;[0m\n"),
            "target text content preserved after stripping ESC; got: {out:?}"
        );
        assert!(
            out.contains("src/[33mlib[0m.rs:10\n"),
            "file path content preserved after stripping ESC; got: {out:?}"
        );
    }

    #[test]
    fn render_markdown_strips_controls_from_comment_body() {
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment("\x1b[31mcross-cutting\x1b[0m")],
            per_change: vec![],
        };
        let out = render_export_markdown(&data);
        assert!(
            !out.contains('\x1b'),
            "markdown output must not contain ESC in comment body; got: {out:?}"
        );
        assert!(
            out.contains("[31mcross-cutting[0m"),
            "body content preserved after stripping ESC; got: {out:?}"
        );
    }

    #[test]
    fn render_markdown_strips_controls_from_revset() {
        let data = ExportData {
            revset: "\x1b[31mtrunk()..@\x1b[0m".to_owned(),
            stack_comments: vec![],
            per_change: vec![],
        };
        let out = render_export_markdown(&data);
        assert!(
            !out.contains('\x1b'),
            "markdown output must not contain ESC in revset; got: {out:?}"
        );
        assert!(
            out.starts_with("# Review export — [31mtrunk()..@[0m\n"),
            "revset content preserved after stripping ESC; got: {out:?}"
        );
    }

    #[test]
    fn render_markdown_strips_controls_from_description() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "\x1b[31mAdd feature\x1b[0m"),
                comments: vec![make_change_comment(&id, "note")],
            }],
        };
        let out = render_export_markdown(&data);
        assert!(
            !out.contains('\x1b'),
            "markdown output must not contain ESC in description; got: {out:?}"
        );
        assert!(
            out.contains("## abc12345 — [31mAdd feature[0m\n"),
            "description content preserved after stripping ESC; got: {out:?}"
        );
    }

    #[test]
    fn render_markdown_skips_change_with_no_comments() {
        let id = cid("abc12345");
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![
                ChangeExport {
                    entry: make_entry("abc12345", "with comment"),
                    comments: vec![make_change_comment(&id, "note")],
                },
                ChangeExport {
                    entry: make_entry("abc22222", "no comment"),
                    comments: vec![],
                },
            ],
        };
        let out = render_export_markdown(&data);
        assert!(out.contains("## abc12345"));
        assert!(!out.contains("## abc22222"));
    }

    /// Snapshot test: pins the exact byte output of `render_export_markdown`
    /// for a fixture with one comment per scope. Any change to the format must
    /// be reflected here deliberately.
    #[test]
    fn render_markdown_snapshot() {
        let id = cid("abc12345");
        let stack_comment = Comment {
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            ..make_stack_comment("cross-cutting concern")
        };
        let change_comment = Comment {
            created_at: datetime!(2026-04-29 10:01:00 UTC),
            ..make_change_comment(&id, "this change is too large")
        };
        let line_comment = Comment {
            created_at: datetime!(2026-04-29 10:02:00 UTC),
            ..make_line_comment(&id, "fix the retry logic")
        };

        let data = ExportData {
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![stack_comment],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "Add client"),
                comments: vec![change_comment, line_comment],
            }],
        };

        let out = render_export_markdown(&data);
        let expected = concat!(
            "# Review export — trunk()..@\n",
            "\n",
            "## Stack-level comments\n",
            "\n",
            "### [NOTE]\n",
            "\n",
            "> cross-cutting concern\n",
            "\n",
            "## abc12345 — Add client\n",
            "\n",
            "### Change-level comments\n",
            "\n",
            "#### [SUGGESTION]\n",
            "\n",
            "> this change is too large\n",
            "\n",
            "### Line-level comments\n",
            "\n",
            "#### [REQUIRED] src/lib.rs:10\n",
            "Hunk: @@ -8,3 +8,5 @@\n",
            "Target: let x = 1;\n",
            "\n",
            "> fix the retry logic\n",
        );
        assert_eq!(out, expected, "markdown snapshot mismatch");
    }

    #[test]
    fn render_markdown_description_comment_excluded_from_output() {
        let id = cid("abc12345");
        let description_comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: id,
                location: local_review_core::comment::DescriptionAnchor {
                    display_line: Some(1),
                    target_text: "Fix the bug".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "should not appear".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let data = ExportData {
            revset: "@".to_owned(),
            stack_comments: vec![],
            per_change: vec![ChangeExport {
                entry: make_entry("abc12345", "some desc"),
                comments: vec![description_comment],
            }],
        };
        let out = render_export_markdown(&data);
        assert!(out.contains("## abc12345 — some desc\n"), "got: {out:?}");
        assert!(!out.contains("### Line-level comments"), "got: {out:?}");
        assert!(!out.contains("#### ["), "got: {out:?}");
    }

    #[test]
    fn collect_export_data_returns_all_comments_unfiltered() {
        use crate::comment::MismatchReason;
        use crate::store;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let id = cid("abc12345");
        let hash = RevsetHash::from_revset("trunk()..@");

        let resolved = ResolvedStack {
            revset: "trunk()..@".to_owned(),
            revset_hash: hash,
            entries: vec![make_entry("abc12345", "first")],
        };

        // Save a stale comment (would be filtered by packet/build_packet).
        let stale = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("f.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@".to_owned(),
                    target_text: "x".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: dir.path().to_owned(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "stale comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: Some(Status::Stale),
            mismatch_reason: Some(MismatchReason::AnchorNotFound),
            entity_id: None,
            anchor_fingerprint: None,
        };
        store::save_comment(dir.path(), dir.path(), &stale).unwrap();

        let data = collect_export_data(dir.path(), dir.path(), &resolved).unwrap();
        assert_eq!(data.per_change.len(), 1);
        assert_eq!(
            data.per_change[0].comments.len(),
            1,
            "export must include stale comments"
        );
        assert_eq!(data.per_change[0].comments[0].body, "stale comment");
    }

    /// A stack-member comment file with `status=Some(Status::Orphaned)` is a
    /// degenerate-but-possible state. Export's contract is a raw disk dump:
    /// it must surface the record, distinct from the packet path which
    /// sanitizes orphans out before sending to Claude.
    #[test]
    fn collect_export_data_includes_orphaned_status_comments_in_stack() {
        use crate::store;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let id = cid("abc12345");
        let hash = RevsetHash::from_revset("trunk()..@");

        let resolved = ResolvedStack {
            revset: "trunk()..@".to_owned(),
            revset_hash: hash,
            entries: vec![make_entry("abc12345", "first")],
        };

        let orphaned = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: id.clone(),
                location: LineAnchor {
                    file: PathBuf::from("f.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@".to_owned(),
                    target_text: "x".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: dir.path().to_owned(),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: "orphaned status".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: Some(Status::Orphaned),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        store::save_comment(dir.path(), dir.path(), &orphaned).unwrap();

        let data = collect_export_data(dir.path(), dir.path(), &resolved).unwrap();
        assert_eq!(data.per_change.len(), 1);
        assert_eq!(
            data.per_change[0].comments.len(),
            1,
            "export must include orphaned-status records in stack files"
        );
        assert_eq!(data.per_change[0].comments[0].body, "orphaned status");
    }
}
