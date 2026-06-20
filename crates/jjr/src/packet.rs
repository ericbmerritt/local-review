use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::change_id::{ChangeId, CommitId};
use crate::comment::{Anchor, Comment, Severity, Side, Status};
use crate::diff::{Diff, Hunk, LineKind};
use crate::error::{JjrError, Result};
use crate::stack::ResolvedStack;
use crate::store;

/// A fully-resolved, inclusion-filtered set of comments ready to render.
pub struct Packet {
    pub data_home: PathBuf,
    pub repo_root: PathBuf,
    pub revset: String,
    /// Stack-scoped comments in created-at order.
    pub stack_comments: Vec<Comment>,
    /// Per-change entries in stack order.
    pub changes: Vec<ChangePacket>,
}

/// Per-change contribution to a packet.
pub struct ChangePacket {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    pub description: String,
    /// Change-scoped comments in created-at order.
    pub change_comments: Vec<Comment>,
    /// Description-scoped comments in created-at order.
    pub description_comments: Vec<Comment>,
    /// Line-scoped comments in file-then-line order.
    pub line_comments: Vec<Comment>,
    /// Diff for the change. `None` when there are no line comments.
    pub diff: Option<Diff>,
}

/// Build a `Packet` from the resolved stack.
///
/// `diff_fn` fetches the diff for a single change. Separated from the rest so
/// callers in tests can inject a stub rather than spawning jj.
pub fn build_packet(
    data_home: &Path,
    repo_root: &Path,
    resolved: &ResolvedStack,
    include_stale: bool,
    diff_fn: impl Fn(&ChangeId) -> Result<Diff>,
) -> Result<Packet> {
    let stack_comments = {
        let raw = store::load_stack_comments(data_home, repo_root, &resolved.revset_hash)?;
        filter_comments(raw, include_stale)
    };

    let mut changes = Vec::new();
    for entry in &resolved.entries {
        let raw = store::load_change_comments(data_home, repo_root, &entry.change_id)?;
        let filtered = filter_comments(raw, include_stale);

        let (change_comments, line_comments, description_comments) = partition_by_scope(filtered);

        let diff = if line_comments.is_empty() {
            None
        } else {
            Some(diff_fn(&entry.change_id)?)
        };

        let sorted_line = sort_line_comments(line_comments);

        if !change_comments.is_empty()
            || !sorted_line.is_empty()
            || !description_comments.is_empty()
        {
            changes.push(ChangePacket {
                change_id: entry.change_id.clone(),
                commit_id: entry.commit_id.clone(),
                description: entry.description.clone(),
                change_comments,
                description_comments,
                line_comments: sorted_line,
                diff,
            });
        }
    }

    if stack_comments.is_empty() && changes.is_empty() {
        return Err(JjrError::EmptyPacket {
            revset: resolved.revset.clone(),
        });
    }

    Ok(Packet {
        data_home: data_home.to_owned(),
        repo_root: repo_root.to_owned(),
        revset: resolved.revset.clone(),
        stack_comments,
        changes,
    })
}

#[derive(Copy, Clone, Debug)]
pub enum PromptMode {
    /// Used by `jjr packet` for self-contained piped output.
    Inline,
    /// Used by the in-process C-key path; references on-disk JSONL files
    /// instead of inlining comment bodies, keeping the prompt small.
    JsonlPaths,
}

/// Inline-mode renderer used by `jjr packet`.
pub fn render_prompt(packet: &Packet) -> String {
    render_prompt_with_mode(packet, PromptMode::Inline)
}

/// Render a `Packet` into the canonical Claude prompt string.
///
/// Same `(packet, mode)` input always produces byte-identical output.
///
/// `Inline` arm splices each change as `header → comments → diff` to keep
/// `jjr packet`'s output byte-stable. `JsonlPaths` arm omits the comments
/// section per change since the JSONL files referenced up top carry it.
/// System-instruction preamble shared by all prompt modes. Callers that build
/// non-standard prompt layouts (e.g. the entity-bundle renderer in `tui.rs`)
/// call this to obtain the canonical header text rather than duplicating it.
pub(crate) fn system_preamble(packet: &Packet) -> String {
    let mut out = String::new();
    out.push_str("You are editing a local jj working copy.\n");
    out.push('\n');
    out.push_str(
        "A human reviewer reviewed a stack of generated changes and left comments at three\n",
    );
    out.push_str(
        "scopes: stack-level (cross-cutting concerns), change-level (concerns about a whole\n",
    );
    out.push_str("change), and line-level (concerns about specific diff lines).\n");
    out.push('\n');
    out.push_str("Your job:\n");
    out.push_str("1. Address each comment by editing the code at the appropriate location.\n");
    out.push_str(
        "2. Required comments must be addressed. Suggestion comments should be addressed\n",
    );
    out.push_str("   when safe and consistent with the change's existing design. Notes are\n");
    out.push_str("   informational; do not act on notes unless explicitly asked.\n");
    out.push_str("3. Preserve the original intent of each change. Make the smallest safe edits.\n");
    out.push_str("4. Do not broaden scope. Do not rewrite unrelated code.\n");
    out.push_str(
        "5. If you cannot safely address a comment, leave the relevant code alone. Do not\n",
    );
    out.push_str(
        "   write justifications, summaries, or status reports — the reviewer reads the\n",
    );
    out.push_str(
        "   resulting diff on the next cycle and adjudicates. The codebase is the reply.\n",
    );
    out.push_str(
        "6. Edit changes in place using jj's mutability model. Do not create new fix-up\n",
    );
    out.push_str("   commits unless the comment explicitly asks for one.\n");
    out.push('\n');
    let _ = writeln!(out, "Repository: {}", packet.repo_root.display());
    let _ = writeln!(out, "Revision: {}", packet.revset);
    out
}

pub fn render_prompt_with_mode(packet: &Packet, mode: PromptMode) -> String {
    let mut out = system_preamble(packet);

    match mode {
        PromptMode::Inline => {
            if !packet.stack_comments.is_empty() {
                out.push('\n');
                out.push_str("## Stack-Level Review Comments\n");
                for comment in &packet.stack_comments {
                    out.push('\n');
                    out.push_str(&render_stack_comment_block(comment));
                }
            }

            if !packet.changes.is_empty() {
                out.push('\n');
                out.push_str("## Changes\n");
                for cp in &packet.changes {
                    out.push_str(&render_change_header(cp));
                    out.push_str(&render_inline_change_comments(cp));
                    out.push_str(&render_change_diff_context(cp));
                }
            }
        }
        PromptMode::JsonlPaths => {
            let comments_section = render_jsonl_paths_section(packet);
            if !comments_section.is_empty() {
                out.push('\n');
                out.push_str(&comments_section);
            }

            if !packet.changes.is_empty() {
                out.push('\n');
                out.push_str("## Changes\n");
                for cp in &packet.changes {
                    out.push_str(&render_change_header(cp));
                    out.push_str(&render_change_diff_context(cp));
                }
            }
        }
    }

    out
}

/// Build the "Review comments" section for `JsonlPaths` mode. Returns an
/// empty string when neither per-change nor stack files would be referenced
/// — preserves the "no comments" semantics so the prompt doesn't carry a
/// dangling section heading.
pub(crate) fn render_jsonl_paths_section(packet: &Packet) -> String {
    let include_stack = !packet.stack_comments.is_empty();

    let change_paths: Vec<(String, PathBuf)> = packet
        .changes
        .iter()
        .filter(|cp| {
            !cp.line_comments.is_empty()
                || !cp.change_comments.is_empty()
                || !cp.description_comments.is_empty()
        })
        .map(|cp| {
            (
                cp.change_id.as_str().to_owned(),
                store::change_file(&packet.data_home, &packet.repo_root, &cp.change_id),
            )
        })
        .collect();

    if !include_stack && change_paths.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Review comments\n");
    out.push('\n');
    out.push_str("Comments live in JSONL files. Schema (see src/comment.rs):\n");
    out.push_str("- `comment`: comment text (body)\n");
    out.push_str("- `severity`: \"required\" | \"suggestion\" | \"note\"\n");
    out.push_str("- `created_at`: RFC3339 timestamp\n");
    out.push_str(
        "- `status`: \"pending\" | \"stale\" | \"orphaned\" (optional; treat absent as pending)\n",
    );
    out.push_str("- `scope`: \"line\" | \"change\" | \"stack\" | \"description\"\n");
    out.push_str("  - line: { file, side: \"old\"|\"new\", old_line?, new_line?, hunk_header, target_text, context_before, context_after, change_id }\n");
    out.push_str("  - change: { change_id }\n");
    out.push_str(
        "  - stack: { revset_hash } — filter to only records matching this stack's revset_hash\n",
    );
    out.push_str("  - description: { display_line?, target_text, context_before, context_after, change_id }\n");
    out.push_str(
        "Address only records with status \"pending\" (or absent). Skip records whose status is \"stale\" or \"orphaned\".\n",
    );
    out.push('\n');
    out.push_str("Comment files for this review:\n");
    for (change_id, path) in &change_paths {
        let _ = writeln!(out, "- Per-change ({change_id}): `{}`", path.display());
    }
    if include_stack {
        let stack_path = store::stack_file(&packet.data_home, &packet.repo_root);
        let _ = writeln!(
            out,
            "- Stack-scope: `{}` (filter records by revset_hash)",
            stack_path.display()
        );
    }
    out
}

pub(crate) fn render_change_header(cp: &ChangePacket) -> String {
    let mut out = String::new();
    out.push('\n');
    let _ = writeln!(out, "Change ID: {}", cp.change_id.as_str());
    let _ = writeln!(out, "Commit: {}", cp.commit_id.as_str());
    out.push_str("Description:\n");
    write_fenced_block(&mut out, &cp.description);
    out
}

fn render_inline_change_comments(cp: &ChangePacket) -> String {
    let mut out = String::new();

    if !cp.change_comments.is_empty() {
        out.push('\n');
        out.push_str("### Change-Level Review Comments\n");
        for comment in &cp.change_comments {
            out.push('\n');
            out.push_str(&render_change_comment_block(comment));
        }
    }

    if !cp.description_comments.is_empty() {
        out.push_str(&render_description_comments_section(cp));
    }

    if !cp.line_comments.is_empty() {
        out.push('\n');
        out.push_str("### Line-Level Review Comments\n");
        for comment in &cp.line_comments {
            out.push('\n');
            out.push_str(&render_line_comment_block(comment));
        }
    }

    out
}

pub(crate) fn render_change_diff_context(cp: &ChangePacket) -> String {
    let mut out = String::new();
    if let Some(diff) = &cp.diff {
        if !cp.line_comments.is_empty() {
            out.push('\n');
            out.push_str("### Relevant Diff Context\n");
            out.push('\n');
            out.push_str(&render_diff_context(diff, &cp.line_comments));
        }
    }
    out
}

fn render_description_comments_section(cp: &ChangePacket) -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str("### Description Comments\n");

    if cp.description.is_empty() {
        // No global description text to anchor the section. Each comment owns
        // its own reconstructed context block so the reader sees exactly the
        // window that comment was anchored against — no overlap between
        // adjacent comments, no false-aggregate header.
        for comment in &cp.description_comments {
            out.push('\n');
            out.push_str(&render_description_comment_block(comment));
            out.push('\n');
            out.push_str("#### Description Context\n");
            let payload = anchor_context_payload(comment);
            write_fenced_block(&mut out, &payload);
        }
    } else {
        for comment in &cp.description_comments {
            out.push('\n');
            out.push_str(&render_description_comment_block(comment));
        }
        out.push('\n');
        out.push_str("#### Description Context\n");
        write_fenced_block(&mut out, &cp.description);
    }
    out
}

/// Reconstruct the context window from a single description comment's anchor:
/// `context_before` lines, then `target_text`, then `context_after` lines.
/// Empty when the comment is not a description-anchored variant.
fn anchor_context_payload(comment: &Comment) -> String {
    let Anchor::Description { location, .. } = &comment.anchor else {
        return String::new();
    };
    let mut out = String::new();
    for line in &location.context_before {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&location.target_text);
    if !location.context_after.is_empty() {
        out.push('\n');
        out.push_str(&location.context_after.join("\n"));
    }
    out
}

/// Emit `text` inside a triple-backtick fenced block. Escalates the fence
/// length when `text` contains a backtick run that would otherwise close the
/// fence prematurely. The closing fence matches the opening length.
fn write_fenced_block(out: &mut String, text: &str) {
    let longest_run = longest_backtick_run(text);
    let fence_len = longest_run.max(2) + 1;
    let fence: String = "`".repeat(fence_len);
    out.push_str(&fence);
    out.push('\n');
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
}

fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for ch in s.chars() {
        if ch == '`' {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 0;
        }
    }
    longest
}

pub(crate) fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Required => "REQUIRED",
        Severity::Suggestion => "SUGGESTION",
        Severity::Note => "NOTE",
    }
}

fn render_stack_comment_block(comment: &Comment) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "### [{}] (stack-level)",
        severity_label(comment.severity)
    );
    out.push('\n');
    out.push_str("Comment:\n");
    out.push_str(&comment.body);
    out.push('\n');
    out
}

fn render_description_comment_block(comment: &Comment) -> String {
    let Anchor::Description { location, .. } = &comment.anchor else {
        return String::new();
    };

    let label = severity_label(comment.severity);
    let mut out = String::new();
    // Drop the trailing colon when display_line is absent; otherwise the
    // heading reads as `description:` with nothing after it. A no-line
    // description anchor is a re-anchored stale that lost its line number.
    match location.display_line {
        Some(n) => {
            let _ = writeln!(out, "### [{label}] description:{n}");
        }
        None => {
            let _ = writeln!(out, "### [{label}] description");
        }
    }
    write_anchor_context(
        &mut out,
        &location.target_text,
        &location.context_before,
        &location.context_after,
    );
    out.push('\n');
    out.push_str("Comment:\n");
    out.push_str(&comment.body);
    out.push('\n');
    out
}

/// Single source of truth for the anchor-context block shared by line- and
/// description-anchored comment renders. Format pinned in
/// `write_anchor_context_emits_full_block`.
fn write_anchor_context(out: &mut String, target_text: &str, before: &[String], after: &[String]) {
    out.push_str("Target line:\n");
    let _ = writeln!(out, "    {target_text}");
    out.push_str("Context:\n");
    for line in before {
        let _ = writeln!(out, "    {line}");
    }
    let _ = writeln!(out, ">>> {target_text}");
    for line in after {
        let _ = writeln!(out, "    {line}");
    }
}

fn render_change_comment_block(comment: &Comment) -> String {
    debug_assert!(
        matches!(comment.anchor, Anchor::Change { .. }),
        "render_change_comment_block called with non-Change anchor"
    );
    let change_id = match &comment.anchor {
        Anchor::Change { change_id } => change_id.as_str().to_owned(),
        Anchor::Line { .. } | Anchor::Stack { .. } | Anchor::Description { .. } => String::new(),
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "### [{}] (change-level) {}",
        severity_label(comment.severity),
        change_id
    );
    out.push('\n');
    out.push_str("Comment:\n");
    out.push_str(&comment.body);
    out.push('\n');
    out
}

pub(crate) fn render_line_comment_block(comment: &Comment) -> String {
    let Anchor::Line {
        location: anchor, ..
    } = &comment.anchor
    else {
        return String::new();
    };

    let side_str = match anchor.side {
        Side::Old => "old",
        Side::New => "new",
    };

    let line_num = anchor
        .new_line
        .or(anchor.old_line)
        .map_or_else(String::new, |n| n.to_string());

    let file_str = anchor.file.display().to_string();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "### [{}] {}:{} ({})",
        severity_label(comment.severity),
        file_str,
        line_num,
        side_str
    );
    let _ = writeln!(out, "Hunk: {}", anchor.hunk_header);
    write_anchor_context(
        &mut out,
        &anchor.target_text,
        &anchor.context_before,
        &anchor.context_after,
    );
    out.push('\n');
    out.push_str("Comment:\n");
    out.push_str(&comment.body);
    out.push('\n');
    out
}

fn render_diff_context(diff: &Diff, line_comments: &[Comment]) -> String {
    let mut out = String::new();

    for diff_file in &diff.files {
        let file_path = diff_file.display_path();

        let matching_comments: Vec<&Comment> = line_comments
            .iter()
            .filter(|c| {
                if let Anchor::Line { location, .. } = &c.anchor {
                    location.file == file_path
                } else {
                    false
                }
            })
            .collect();

        if matching_comments.is_empty() {
            continue;
        }

        let anchored_line_nums: Vec<u32> = matching_comments
            .iter()
            .filter_map(|c| {
                if let Anchor::Line { location, .. } = &c.anchor {
                    location.new_line.or(location.old_line)
                } else {
                    None
                }
            })
            .collect();

        let mut rendered_hunk_indices: HashSet<usize> = HashSet::new();

        for (idx, hunk) in diff_file.hunks().iter().enumerate() {
            if rendered_hunk_indices.contains(&idx) {
                continue;
            }
            if hunk_contains_anchored_line(hunk, &anchored_line_nums, &matching_comments) {
                rendered_hunk_indices.insert(idx);
                out.push_str(&render_hunk(hunk));
            }
        }
    }

    out
}

fn hunk_contains_anchored_line(
    hunk: &Hunk,
    anchored_line_nums: &[u32],
    comments: &[&Comment],
) -> bool {
    anchored_line_nums.iter().any(|&n| {
        comments.iter().any(|c| {
            let Anchor::Line { location, .. } = &c.anchor else {
                return false;
            };
            let target = location.new_line.or(location.old_line);
            if target != Some(n) {
                return false;
            }
            hunk.lines.iter().any(|l| match location.side {
                Side::New => l.target_line == Some(n),
                Side::Old => l.source_line == Some(n),
            })
        })
    })
}

pub(crate) fn render_hunk(hunk: &Hunk) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", hunk.header);
    for line in &hunk.lines {
        let prefix = match line.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        };
        out.push(prefix);
        out.push_str(&line.text);
        out.push('\n');
    }
    out
}

fn filter_comments(comments: Vec<Comment>, include_stale: bool) -> Vec<Comment> {
    comments
        .into_iter()
        .filter(|c| match c.status {
            Some(Status::Orphaned) => false,
            Some(Status::Stale) => include_stale,
            Some(Status::Pending) | None => true,
        })
        .collect()
}

fn partition_by_scope(comments: Vec<Comment>) -> (Vec<Comment>, Vec<Comment>, Vec<Comment>) {
    let mut change_comments = Vec::new();
    let mut line_comments = Vec::new();
    let mut description_comments = Vec::new();
    for c in comments {
        match &c.anchor {
            Anchor::Change { .. } => change_comments.push(c),
            Anchor::Line { .. } => line_comments.push(c),
            Anchor::Description { .. } => description_comments.push(c),
            Anchor::Stack { .. } => {
                // Stack-scoped comments are handled at the packet level, not
                // per-change. They must not appear in per-change JSONL files.
                debug_assert!(
                    false,
                    "stack-scoped comment in per-change store is a bug: {:?}",
                    c.anchor
                );
            }
        }
    }
    (change_comments, line_comments, description_comments)
}

fn sort_line_comments(mut comments: Vec<Comment>) -> Vec<Comment> {
    comments.sort_by(|a, b| {
        let (file_a, line_a) = line_comment_sort_key(a);
        let (file_b, line_b) = line_comment_sort_key(b);
        file_a.cmp(&file_b).then(line_a.cmp(&line_b))
    });
    comments
}

fn line_comment_sort_key(comment: &Comment) -> (PathBuf, u32) {
    match &comment.anchor {
        Anchor::Line { location, .. } => {
            let line = location.new_line.or(location.old_line).unwrap_or(0);
            (location.file.clone(), line)
        }
        Anchor::Change { .. } | Anchor::Stack { .. } | Anchor::Description { .. } => {
            (PathBuf::new(), 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;
    use crate::change_id::{ChangeId, CommitId};
    use crate::comment::{Anchor, Comment, LineAnchor, SchemaVersion, Severity, Side, Status};
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};
    use crate::stack::{ResolvedStack, RevsetHash, StackEntry};

    fn cid(s: &str) -> ChangeId {
        ChangeId::parse(s).unwrap()
    }

    fn commit_id(s: &str) -> CommitId {
        CommitId::parse(s).unwrap()
    }

    fn make_stack_comment(body: &str, severity: Severity) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            },
            repo_root: PathBuf::from("/repo"),
            revset: "trunk()..@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_change_comment(change_id: &ChangeId, body: &str, severity: Severity) -> Comment {
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
            created_at: datetime!(2026-04-29 10:01:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_line_comment(
        change_id: &ChangeId,
        file: &str,
        line: u32,
        body: &str,
        severity: Severity,
    ) -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: change_id.clone(),
                location: LineAnchor {
                    file: PathBuf::from(file),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(line),
                    hunk_header: "@@ -140,7 +140,12 @@ impl Client {".to_owned(),
                    target_text: "let resp = self.inner.request(req).await?;".to_owned(),
                    context_before: vec![
                        "pub async fn send(&self, req: Request) -> Result<Response> {".to_owned(),
                        "    let req = self.prepare(req)?;".to_owned(),
                    ],
                    context_after: vec!["    Ok(resp)".to_owned(), "}".to_owned()],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at: datetime!(2026-04-29 10:02:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    fn make_stale_line_comment(change_id: &ChangeId, body: &str) -> Comment {
        Comment {
            status: Some(Status::Stale),
            ..make_line_comment(change_id, "src/lib.rs", 10, body, Severity::Note)
        }
    }

    fn make_orphaned_line_comment(change_id: &ChangeId, body: &str) -> Comment {
        Comment {
            status: Some(Status::Orphaned),
            ..make_line_comment(change_id, "src/lib.rs", 10, body, Severity::Note)
        }
    }

    fn simple_diff() -> Diff {
        Diff {
            files: vec![DiffFile::Modified {
                path: PathBuf::from("src/client.rs"),
                hunks: vec![Hunk {
                    header: "@@ -140,7 +140,12 @@ impl Client {".to_owned(),
                    function_context: Some("impl Client {".to_owned()),
                    source_start: 140,
                    source_length: 7,
                    target_start: 140,
                    target_length: 12,
                    lines: vec![
                        Line {
                            kind: LineKind::Context,
                            text: "pub async fn send(&self, req: Request) -> Result<Response> {"
                                .to_owned(),
                            source_line: Some(140),
                            target_line: Some(140),
                        },
                        Line {
                            kind: LineKind::Context,
                            text: "    let req = self.prepare(req)?;".to_owned(),
                            source_line: Some(141),
                            target_line: Some(141),
                        },
                        Line {
                            kind: LineKind::Added,
                            text: "    let resp = self.inner.request(req).await?;".to_owned(),
                            source_line: None,
                            target_line: Some(142),
                        },
                        Line {
                            kind: LineKind::Context,
                            text: "    Ok(resp)".to_owned(),
                            source_line: Some(143),
                            target_line: Some(143),
                        },
                        Line {
                            kind: LineKind::Context,
                            text: "}".to_owned(),
                            source_line: Some(144),
                            target_line: Some(144),
                        },
                    ],
                }],
            }],
        }
    }

    fn make_resolved_stack(entries: &[(&str, &str)]) -> ResolvedStack {
        ResolvedStack {
            revset: "trunk()..@".to_owned(),
            revset_hash: RevsetHash::from_revset("trunk()..@"),
            entries: entries
                .iter()
                .map(|(cid_str, desc)| StackEntry {
                    change_id: cid(cid_str),
                    commit_id: commit_id("aabbccdd11223344"),
                    description: desc.to_string(),
                })
                .collect(),
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "must match the Fn(&ChangeId) -> Result<Diff> signature required by build_packet"
    )]
    fn no_diff(_id: &ChangeId) -> Result<Diff> {
        Ok(Diff { files: vec![] })
    }

    // --- filter_comments ---

    #[test]
    fn filter_excludes_stale_by_default() {
        let id = cid("abc11111");
        let stale = make_stale_line_comment(&id, "stale body");
        let pending = make_line_comment(&id, "f.rs", 1, "pending body", Severity::Note);
        let filtered = filter_comments(vec![stale, pending], false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].body, "pending body");
    }

    #[test]
    fn filter_includes_stale_when_flag_set() {
        let id = cid("abc11111");
        let stale = make_stale_line_comment(&id, "stale body");
        let pending = make_line_comment(&id, "f.rs", 1, "pending body", Severity::Note);
        let filtered = filter_comments(vec![stale, pending], true);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_always_excludes_orphaned() {
        let id = cid("abc11111");
        let orphaned = make_orphaned_line_comment(&id, "orphaned body");
        let pending = make_line_comment(&id, "f.rs", 1, "pending body", Severity::Note);

        let without_flag = filter_comments(vec![orphaned.clone(), pending.clone()], false);
        assert_eq!(without_flag.len(), 1);

        let with_flag = filter_comments(vec![orphaned, pending], true);
        assert_eq!(with_flag.len(), 1);
        assert_eq!(with_flag[0].body, "pending body");
    }

    // --- sort_line_comments ---

    #[test]
    fn line_comments_sorted_by_file_then_line() {
        let id = cid("abc11111");
        let c1 = make_line_comment(&id, "z.rs", 5, "z5", Severity::Note);
        let c2 = make_line_comment(&id, "a.rs", 10, "a10", Severity::Note);
        let c3 = make_line_comment(&id, "a.rs", 2, "a2", Severity::Note);
        let sorted = sort_line_comments(vec![c1, c2, c3]);
        assert_eq!(sorted[0].body, "a2");
        assert_eq!(sorted[1].body, "a10");
        assert_eq!(sorted[2].body, "z5");
    }

    // --- render_prompt: stack-only ---

    #[test]
    fn render_stack_only_has_no_changes_section() {
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![make_stack_comment("rename everything", Severity::Required)],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("## Stack-Level Review Comments"));
        assert!(!out.contains("## Changes"));
        assert!(out.contains("[REQUIRED] (stack-level)"));
        assert!(out.contains("rename everything"));
    }

    #[test]
    fn render_change_only_no_line_level_section() {
        let id = cid("abc11111");
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id.clone(),
                commit_id: commit_id("aabbccdd11223344"),
                description: "Add feature".to_owned(),
                change_comments: vec![make_change_comment(
                    &id,
                    "this change is too large",
                    Severity::Suggestion,
                )],
                description_comments: vec![],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("### Change-Level Review Comments"));
        assert!(!out.contains("### Line-Level Review Comments"));
        assert!(!out.contains("### Relevant Diff Context"));
        assert!(out.contains("[SUGGESTION] (change-level)"));
    }

    #[test]
    fn render_line_only_no_change_level_section() {
        let id = cid("abc11111");
        let line_comment = make_line_comment(
            &id,
            "src/client.rs",
            142,
            "use retry wrapper",
            Severity::Required,
        );
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: "Add feature".to_owned(),
                change_comments: vec![],
                description_comments: vec![],
                line_comments: vec![line_comment],
                diff: Some(simple_diff()),
            }],
        };
        let out = render_prompt(&packet);
        assert!(!out.contains("### Change-Level Review Comments"));
        assert!(out.contains("### Line-Level Review Comments"));
        assert!(out.contains("### Relevant Diff Context"));
    }

    #[test]
    fn render_all_severity_labels() {
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![
                make_stack_comment("req", Severity::Required),
                make_stack_comment("sug", Severity::Suggestion),
                make_stack_comment("note", Severity::Note),
            ],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("[REQUIRED]"));
        assert!(out.contains("[SUGGESTION]"));
        assert!(out.contains("[NOTE]"));
    }

    #[test]
    fn render_preserves_body_newlines() {
        let body = "line one\nline two\nline three";
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment(body, Severity::Note)],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("line one\nline two\nline three"));
    }

    #[test]
    fn render_is_deterministic() {
        let id = cid("abc11111");
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![make_stack_comment("stack note", Severity::Note)],
            changes: vec![ChangePacket {
                change_id: id.clone(),
                commit_id: commit_id("aabbccdd11223344"),
                description: "first".to_owned(),
                change_comments: vec![make_change_comment(
                    &id,
                    "change note",
                    Severity::Suggestion,
                )],
                description_comments: vec![],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out1 = render_prompt(&packet);
        let out2 = render_prompt(&packet);
        assert_eq!(out1, out2);
    }

    #[test]
    fn render_line_comment_matches_spec_example() {
        let id = cid("abc11111");
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: id,
                location: LineAnchor {
                    file: PathBuf::from("src/client.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(142),
                    hunk_header: "@@ -140,7 +140,12 @@ impl Client {".to_owned(),
                    target_text: "let resp = self.inner.request(req).await?;".to_owned(),
                    context_before: vec![
                        "pub async fn send(&self, req: Request) -> Result<Response> {".to_owned(),
                        "    let req = self.prepare(req)?;".to_owned(),
                    ],
                    context_after: vec!["    Ok(resp)".to_owned(), "}".to_owned()],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "This bypasses the retry policy configured on Self. The rest of the module\nis built around honoring that policy; this path needs to call the retry\nwrapper, not the inner client directly.".to_owned(),
            severity: Severity::Required,
            created_at: datetime!(2026-04-29 10:00:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };

        let block = render_line_comment_block(&comment);

        let expected = concat!(
            "### [REQUIRED] src/client.rs:142 (new)\n",
            "Hunk: @@ -140,7 +140,12 @@ impl Client {\n",
            "Target line:\n",
            "    let resp = self.inner.request(req).await?;\n",
            "Context:\n",
            "    pub async fn send(&self, req: Request) -> Result<Response> {\n",
            "        let req = self.prepare(req)?;\n",
            ">>> let resp = self.inner.request(req).await?;\n",
            "        Ok(resp)\n",
            "    }\n",
            "\n",
            "Comment:\n",
            "This bypasses the retry policy configured on Self. The rest of the module\n",
            "is built around honoring that policy; this path needs to call the retry\n",
            "wrapper, not the inner client directly.\n",
        );

        assert_eq!(
            block, expected,
            "line comment block does not match spec example"
        );
    }

    #[test]
    fn render_prompt_contains_repo_and_revision() {
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/workspace/project"),
            revset: "trunk()..@".to_owned(),
            stack_comments: vec![make_stack_comment("note", Severity::Note)],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("Repository: /workspace/project\n"));
        assert!(out.contains("Revision: trunk()..@\n"));
    }

    #[test]
    fn render_prompt_contains_prelude() {
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment("x", Severity::Note)],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("You are editing a local jj working copy."));
        assert!(out.contains("Your job:"));
        assert!(out.contains("1. Address each comment"));
    }

    // --- build_packet ---

    #[test]
    fn build_packet_empty_stack_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        let result = build_packet(dir.path(), dir.path(), &stack, false, no_diff);
        assert!(
            matches!(result, Err(JjrError::EmptyPacket { .. })),
            "expected EmptyPacket"
        );
    }

    #[test]
    fn build_packet_stale_excluded_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        let stale = Comment {
            repo_root: dir.path().to_owned(),
            ..make_stale_line_comment(&id, "stale")
        };
        store::save_comment(dir.path(), dir.path(), &stale).unwrap();

        let result = build_packet(dir.path(), dir.path(), &stack, false, no_diff);
        assert!(
            matches!(result, Err(JjrError::EmptyPacket { .. })),
            "stale comment should be excluded by default"
        );
    }

    #[test]
    fn build_packet_stale_included_with_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        let stale = Comment {
            repo_root: dir.path().to_owned(),
            ..make_stale_line_comment(&id, "stale body")
        };
        store::save_comment(dir.path(), dir.path(), &stale).unwrap();

        let packet = build_packet(dir.path(), dir.path(), &stack, true, no_diff).unwrap();
        assert_eq!(packet.changes.len(), 1);
        assert_eq!(packet.changes[0].line_comments.len(), 1);
        assert_eq!(packet.changes[0].line_comments[0].body, "stale body");
    }

    #[test]
    fn build_packet_orphaned_never_included() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        let orphaned = Comment {
            repo_root: dir.path().to_owned(),
            ..make_orphaned_line_comment(&id, "orphaned")
        };
        store::save_comment(dir.path(), dir.path(), &orphaned).unwrap();

        let result = build_packet(dir.path(), dir.path(), &stack, true, no_diff);
        assert!(
            matches!(result, Err(JjrError::EmptyPacket { .. })),
            "orphaned comment must never be included even with include_stale"
        );
    }

    #[test]
    fn build_packet_aggregates_stack_and_change_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = ResolvedStack {
            revset: "trunk()..@".to_owned(),
            revset_hash: RevsetHash::from_revset("trunk()..@"),
            entries: vec![StackEntry {
                change_id: id.clone(),
                commit_id: commit_id("aabbccdd11223344"),
                description: "first".to_owned(),
            }],
        };

        let mut stack_comment = make_stack_comment("cross-cutting", Severity::Note);
        stack_comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &stack_comment).unwrap();

        let mut change_comment = make_change_comment(&id, "change body", Severity::Suggestion);
        change_comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &change_comment).unwrap();

        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        assert_eq!(packet.stack_comments.len(), 1);
        assert_eq!(packet.stack_comments[0].body, "cross-cutting");
        assert_eq!(packet.changes.len(), 1);
        assert_eq!(packet.changes[0].change_comments.len(), 1);
    }

    #[test]
    fn build_packet_change_without_comments_excluded() {
        let dir = tempfile::TempDir::new().unwrap();
        let id1 = cid("abc11111");
        let id2 = cid("abc22222");
        let stack = make_resolved_stack(&[("abc11111", "first"), ("abc22222", "second")]);

        let mut comment = make_change_comment(&id1, "first change note", Severity::Note);
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        assert_eq!(packet.changes.len(), 1);
        assert_eq!(packet.changes[0].change_id, id1);

        let has_second = packet.changes.iter().any(|cp| cp.change_id == id2);
        assert!(!has_second, "change with no comments must be excluded");
    }

    #[test]
    fn render_diff_context_renders_hunk_for_anchored_line() {
        let id = cid("abc11111");
        let line_comment =
            make_line_comment(&id, "src/client.rs", 142, "fix this", Severity::Required);
        let diff = simple_diff();

        let context = render_diff_context(&diff, &[line_comment]);
        assert!(context.contains("@@ -140,7 +140,12 @@ impl Client {"));
        assert!(context.contains("+    let resp = self.inner.request(req).await?;"));
    }

    #[test]
    fn render_diff_context_skips_file_with_no_matching_comments() {
        let id = cid("abc11111");
        let line_comment = make_line_comment(&id, "src/other.rs", 5, "fix", Severity::Note);
        let diff = simple_diff();

        let context = render_diff_context(&diff, &[line_comment]);
        assert!(
            context.is_empty(),
            "no hunk should render when file does not match"
        );
    }

    // -- B1: two line comments in the same hunk render the hunk exactly once.
    //   Pins the dedup behavior in `render_diff_context` (HashSet of rendered
    //   hunk indices); a regression that double-renders the hunk would inflate
    //   the prompt and confuse downstream tooling.

    #[test]
    fn render_diff_context_renders_shared_hunk_exactly_once() {
        let id = cid("abc11111");
        let comment_a = make_line_comment(&id, "src/client.rs", 142, "first", Severity::Required);
        // Second comment anchored on a context line in the SAME hunk (line 143
        // is the `Ok(resp)` context line in `simple_diff()`).
        let comment_b = Comment {
            anchor: Anchor::Line {
                change_id: id,
                location: LineAnchor {
                    file: PathBuf::from("src/client.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(143),
                    hunk_header: "@@ -140,7 +140,12 @@ impl Client {".to_owned(),
                    target_text: "    Ok(resp)".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            ..make_line_comment(
                &cid("abc11111"),
                "src/client.rs",
                143,
                "second",
                Severity::Note,
            )
        };
        let diff = simple_diff();

        let context = render_diff_context(&diff, &[comment_a, comment_b]);
        let header_count = context
            .matches("@@ -140,7 +140,12 @@ impl Client {")
            .count();
        assert_eq!(
            header_count, 1,
            "hunk header must appear exactly once even with multiple anchored lines; got {header_count}"
        );
    }

    // -- B2: comments in two different files of the same change render both
    //   files' hunks. Pins per-file iteration in `render_diff_context`.

    #[test]
    fn render_diff_context_renders_hunks_for_each_file_with_anchors() {
        let id = cid("abc11111");
        let comment_a = make_line_comment(&id, "src/a.rs", 10, "in a", Severity::Required);
        let comment_b = make_line_comment(&id, "src/b.rs", 20, "in b", Severity::Note);

        let diff = Diff {
            files: vec![
                DiffFile::Modified {
                    path: PathBuf::from("src/a.rs"),
                    hunks: vec![Hunk {
                        header: "@@ -8,3 +8,4 @@ fn a {".to_owned(),
                        function_context: Some("fn a {".to_owned()),
                        source_start: 8,
                        source_length: 3,
                        target_start: 8,
                        target_length: 4,
                        lines: vec![Line {
                            kind: LineKind::Added,
                            text: "    let x = 1;".to_owned(),
                            source_line: None,
                            target_line: Some(10),
                        }],
                    }],
                },
                DiffFile::Modified {
                    path: PathBuf::from("src/b.rs"),
                    hunks: vec![Hunk {
                        header: "@@ -18,3 +18,4 @@ fn b {".to_owned(),
                        function_context: Some("fn b {".to_owned()),
                        source_start: 18,
                        source_length: 3,
                        target_start: 18,
                        target_length: 4,
                        lines: vec![Line {
                            kind: LineKind::Added,
                            text: "    let y = 2;".to_owned(),
                            source_line: None,
                            target_line: Some(20),
                        }],
                    }],
                },
            ],
        };

        let context = render_diff_context(&diff, &[comment_a, comment_b]);
        assert!(
            context.contains("@@ -8,3 +8,4 @@ fn a {"),
            "expected hunk for src/a.rs, got: {context}"
        );
        assert!(
            context.contains("@@ -18,3 +18,4 @@ fn b {"),
            "expected hunk for src/b.rs, got: {context}"
        );
    }

    // -- B3: include_stale=true on a change with both Stale and Pending line
    //   comments renders both, ordered by created_at. Pins the contract that
    //   stale-inclusion preserves chronological order rather than re-sorting
    //   by status.

    #[test]
    fn build_packet_include_stale_orders_stale_and_pending_by_created_at() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        // Pending comment created earlier.
        let pending = Comment {
            repo_root: dir.path().to_owned(),
            created_at: datetime!(2026-04-29 09:00:00 UTC),
            ..make_line_comment(&id, "src/lib.rs", 5, "earlier pending", Severity::Note)
        };
        // Stale comment created later — same file/line so sort_line_comments
        // doesn't reorder by file/line.
        let stale = Comment {
            repo_root: dir.path().to_owned(),
            created_at: datetime!(2026-04-29 11:00:00 UTC),
            ..make_stale_line_comment(&id, "later stale")
        };
        store::save_comment(dir.path(), dir.path(), &pending).unwrap();
        store::save_comment(dir.path(), dir.path(), &stale).unwrap();

        let packet = build_packet(dir.path(), dir.path(), &stack, true, no_diff).unwrap();
        assert_eq!(packet.changes.len(), 1);
        let bodies: Vec<&str> = packet.changes[0]
            .line_comments
            .iter()
            .map(|c| c.body.as_str())
            .collect();
        assert_eq!(
            bodies,
            vec!["earlier pending", "later stale"],
            "comments should be in created_at order when include_stale=true"
        );
    }

    // -- C: comment body is rendered verbatim, including content that looks
    //   like the prompt's own structural markers. By-spec invariant: the
    //   body is not reflowed or sanitized at render time. Downstream parsers
    //   own the responsibility for handling embedded markers.

    #[test]
    fn render_preserves_structural_markers_in_body_verbatim() {
        let body = "My note about ### [REQUIRED] in the code, also >>> arrows.";
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![make_stack_comment(body, Severity::Note)],
            changes: vec![],
        };
        let out = render_prompt(&packet);
        assert!(
            out.contains(body),
            "body must be rendered verbatim; got:\n{out}"
        );
    }

    // --- description-comment rendering ---

    fn make_description_comment(
        change_id: &ChangeId,
        body: &str,
        severity: Severity,
        target_text: &str,
        display_line: Option<u32>,
    ) -> Comment {
        use crate::comment::DescriptionAnchor;
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: change_id.clone(),
                location: DescriptionAnchor {
                    display_line,
                    target_text: target_text.to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: body.to_owned(),
            severity,
            created_at: datetime!(2026-04-29 10:03:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    // -- B3: heading hierarchy. The Description Comments section emits
    //   `### Description Comments` and a `#### Description Context` child;
    //   no second `###` sibling under the same parent.
    #[test]
    fn render_description_section_uses_h4_for_context_block() {
        let id = cid("abc11111");
        let comment = make_description_comment(
            &id,
            "tighten this wording",
            Severity::Suggestion,
            "summary line",
            Some(1),
        );
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: "summary line\nbody line".to_owned(),
                change_comments: vec![],
                description_comments: vec![comment],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        assert!(
            out.contains("### Description Comments\n"),
            "expected `### Description Comments` heading"
        );
        assert!(
            out.contains("#### Description Context\n"),
            "context block must be `####`, not `###`; got:\n{out}"
        );
        // The h3 form must not appear; only the h4 nested form is permitted.
        // Search for `### Description Context` as a line-prefix to avoid the
        // false-positive caused by `####` having `###` as a prefix substring.
        assert!(
            !out
                .lines()
                .any(|l| l == "### Description Context"
                    || l.starts_with("### Description Context ")),
            "must not emit `### Description Context` (sibling of Description Comments); got:\n{out}"
        );
    }

    // -- T1a: populated description renders with target text, line marker,
    //   the context block, and the comment body.
    #[test]
    fn render_description_section_populated_includes_target_and_context() {
        let id = cid("abc11111");
        let comment = make_description_comment(
            &id,
            "the body of the description note",
            Severity::Required,
            "summary line",
            Some(1),
        );
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: "summary line\nbody line".to_owned(),
                change_comments: vec![],
                description_comments: vec![comment],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("### Description Comments"));
        assert!(out.contains("#### Description Context"));
        // Fenced code with the description text.
        assert!(
            out.contains("```\nsummary line\nbody line\n```\n"),
            "expected fenced description block; got:\n{out}"
        );
        // Description-comment heading with severity + display_line marker.
        assert!(
            out.contains("[REQUIRED] description:1"),
            "expected severity+line heading; got:\n{out}"
        );
        // Target text rendered.
        assert!(out.contains("    summary line"));
        // Comment body verbatim.
        assert!(out.contains("the body of the description note"));
    }

    // -- T1b: when there are no description comments, the section is omitted
    //   entirely from the rendered prompt — not an empty heading.
    #[test]
    fn render_description_section_omitted_when_no_comments() {
        let id = cid("abc11111");
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id.clone(),
                commit_id: commit_id("aabbccdd11223344"),
                description: "summary line".to_owned(),
                change_comments: vec![make_change_comment(&id, "change body", Severity::Note)],
                description_comments: vec![],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        assert!(
            !out.contains("Description Comments"),
            "Description Comments section must be omitted when empty; got:\n{out}"
        );
        assert!(
            !out.contains("Description Context"),
            "Description Context must be omitted when empty; got:\n{out}"
        );
    }

    // -- CV3: write_fenced_block escalates fence length when the payload
    //   contains a triple-backtick run that would close a 3-tick fence early.
    #[test]
    fn write_fenced_block_escapes_triple_backtick_in_payload() {
        let mut out = String::new();
        write_fenced_block(&mut out, "before\n```\nafter");
        // Payload's longest run is 3 ticks; fence must be >= 4.
        assert!(
            out.starts_with("````\n"),
            "fence must escalate to >= 4 ticks; got:\n{out}"
        );
        assert!(
            out.ends_with("````\n"),
            "closing fence must match opening length; got:\n{out}"
        );
        assert!(out.contains("before\n```\nafter"));
    }

    #[test]
    fn write_fenced_block_uses_three_ticks_when_payload_has_no_run() {
        let mut out = String::new();
        write_fenced_block(&mut out, "no backticks here\n");
        assert!(out.starts_with("```\n"), "default fence is 3 ticks");
        assert!(out.ends_with("```\n"));
    }

    #[test]
    fn write_fenced_block_escalates_for_quintuple_run() {
        let mut out = String::new();
        write_fenced_block(&mut out, "weird ````` run");
        assert!(
            out.starts_with("``````\n"),
            "5-tick run forces 6-tick fence; got:\n{out}"
        );
    }

    // -- T-CV3-leading-fence: payload that opens with a bare 3-tick fence at
    //   byte zero. Pins the position-agnostic scan; a token-only check would
    //   miss this and emit a 3-tick wrapper that closes at the embedded run.
    #[test]
    fn write_fenced_block_handles_leading_bare_fence_at_byte_zero() {
        let mut out = String::new();
        write_fenced_block(&mut out, "```\nfoo");
        assert!(
            out.starts_with("````\n"),
            "leading bare fence forces >= 4-tick wrapper; got:\n{out}"
        );
        assert!(
            out.ends_with("````\n"),
            "closing fence must match opening length; got:\n{out}"
        );
        assert!(out.contains("```\nfoo"));
    }

    // -- C2: pin the shared anchor-context block format. Single source of
    //   truth for line-anchored and description-anchored comment renders.
    #[test]
    fn write_anchor_context_emits_full_block() {
        let mut out = String::new();
        write_anchor_context(
            &mut out,
            "the target",
            &["before-1".to_owned(), "before-2".to_owned()],
            &["after-1".to_owned(), "after-2".to_owned()],
        );
        let expected = concat!(
            "Target line:\n",
            "    the target\n",
            "Context:\n",
            "    before-1\n",
            "    before-2\n",
            ">>> the target\n",
            "    after-1\n",
            "    after-2\n",
        );
        assert_eq!(out, expected, "anchor-context block format drifted");
    }

    // -- T3-A1: multiple comments + empty description preserve order and emit
    //   a per-comment context block, no overlap between adjacent comments.
    #[test]
    fn render_description_section_preserves_comment_order_with_empty_description() {
        let id = cid("abc11111");
        let c1 =
            make_description_comment(&id, "first body", Severity::Note, "first target", Some(1));
        let c2 = make_description_comment(
            &id,
            "second body",
            Severity::Required,
            "second target",
            Some(2),
        );
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: String::new(),
                change_comments: vec![],
                description_comments: vec![c1, c2],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        // Both comment bodies present.
        assert!(out.contains("first body"));
        assert!(out.contains("second body"));
        // Both anchors' target_text surface in their own context blocks.
        let first_target_pos = out
            .find("first target\n```")
            .or_else(|| out.find("first target"))
            .expect("first target text must appear in a context block");
        let second_target_pos = out
            .find("second target\n```")
            .or_else(|| out.find("second target"))
            .expect("second target text must appear in a context block");
        assert!(
            first_target_pos < second_target_pos,
            "context blocks must appear in description_comments order"
        );
        // Two `#### Description Context` headings (one per comment).
        let ctx_count = out.matches("#### Description Context\n").count();
        assert_eq!(
            ctx_count, 2,
            "expected one context block per comment; got:\n{out}"
        );
    }

    // -- T3-A2: empty anchor window (target_text + context_*) yields a
    //   minimal but well-formed fence — no missing closing tick, no
    //   surprise content. Pins the shape so a future change is intentional.
    #[test]
    fn render_description_section_with_empty_anchor_window_renders_minimal_fence() {
        use crate::comment::DescriptionAnchor;
        let id = cid("abc11111");
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: id.clone(),
                location: DescriptionAnchor {
                    display_line: Some(1),
                    target_text: String::new(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "the body".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 10:03:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: String::new(),
                change_comments: vec![],
                description_comments: vec![comment],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        // Minimal fence: opening tick, single `\n` (target_text was empty),
        // closing tick. Both `#### Description Context\n```\n\n```\n` —
        // exactly one blank line inside.
        assert!(
            out.contains("#### Description Context\n```\n\n```\n"),
            "minimal fence shape must be `````` + blank + ``````; got:\n{out}"
        );
    }

    // -- partition routing: Anchor::Description is bucketed into the
    //   description list, not change or line.
    #[test]
    fn partition_by_scope_routes_description_to_description_bucket() {
        let id = cid("abc11111");
        let desc = make_description_comment(&id, "d", Severity::Note, "x", Some(1));
        let change = make_change_comment(&id, "c", Severity::Note);
        let line = make_line_comment(&id, "src/x.rs", 5, "l", Severity::Note);
        let (changes, lines, descriptions) = partition_by_scope(vec![desc, change, line]);
        assert_eq!(descriptions.len(), 1);
        assert_eq!(descriptions[0].body, "d");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].body, "c");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].body, "l");
    }

    // -- T2: heading drops the trailing colon when display_line is None. A
    //   re-anchored stale that lost its line number renders as
    //   `### [SEV] description`, not `### [SEV] description:`.
    #[test]
    fn render_description_comment_block_heading_format_with_no_display_line() {
        let id = cid("abc11111");
        let comment =
            make_description_comment(&id, "the body", Severity::Required, "summary line", None);
        let block = render_description_comment_block(&comment);
        assert!(
            block.contains("### [REQUIRED] description\n"),
            "expected heading without trailing colon; got:\n{block}"
        );
        assert!(
            !block.contains("description:\n"),
            "must not emit `description:` when display_line is None; got:\n{block}"
        );
    }

    // -- T2 (paired): when display_line is Some, the heading still includes
    //   the line number with the colon. Pins both branches of the format.
    #[test]
    fn render_description_comment_block_heading_with_display_line_includes_colon() {
        let id = cid("abc11111");
        let comment = make_description_comment(
            &id,
            "the body",
            Severity::Suggestion,
            "summary line",
            Some(7),
        );
        let block = render_description_comment_block(&comment);
        assert!(
            block.contains("### [SUGGESTION] description:7\n"),
            "expected heading with line number; got:\n{block}"
        );
    }

    // -- T3: half-state — comments present but description text empty. The
    //   section still renders; the context block is reconstructed from the
    //   comments' anchor windows rather than left empty.
    #[test]
    fn render_description_section_uses_anchor_context_when_description_text_empty() {
        use crate::comment::DescriptionAnchor;
        let id = cid("abc11111");
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: id.clone(),
                location: DescriptionAnchor {
                    display_line: Some(2),
                    target_text: "anchor target text".to_owned(),
                    context_before: vec!["anchor before".to_owned()],
                    context_after: vec!["anchor after".to_owned()],
                },
            },
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "the comment body".to_owned(),
            severity: Severity::Required,
            created_at: datetime!(2026-04-29 10:03:00 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let packet = Packet {
            data_home: PathBuf::from("/data"),
            repo_root: PathBuf::from("/repo"),
            revset: "@".to_owned(),
            stack_comments: vec![],
            changes: vec![ChangePacket {
                change_id: id,
                commit_id: commit_id("aabbccdd11223344"),
                description: String::new(),
                change_comments: vec![],
                description_comments: vec![comment],
                line_comments: vec![],
                diff: None,
            }],
        };
        let out = render_prompt(&packet);
        assert!(out.contains("### Description Comments\n"));
        assert!(
            out.contains("#### Description Context\n"),
            "context heading must still appear; got:\n{out}"
        );
        // Context block is reconstructed from the anchor's window.
        assert!(
            out.contains("anchor before\n"),
            "anchor context_before must surface; got:\n{out}"
        );
        assert!(
            out.contains("anchor target text\n"),
            "anchor target_text must surface; got:\n{out}"
        );
        assert!(
            out.contains("anchor after\n"),
            "anchor context_after must surface; got:\n{out}"
        );
        // Comment body still rendered.
        assert!(out.contains("the comment body"));
    }

    // --- render_prompt_with_mode: JsonlPaths variant ---

    #[test]
    fn jsonl_mode_per_change_file_replaces_inline_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let stack = make_resolved_stack(&[("abc11111", "first")]);

        let mut comment = make_change_comment(&id, "secret body text", Severity::Required);
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        let out = render_prompt_with_mode(&packet, PromptMode::JsonlPaths);

        assert!(
            out.contains("## Review comments\n"),
            "Review comments section must be present; got:\n{out}"
        );
        assert!(out.contains("`severity`"));
        assert!(out.contains("`scope`"));
        assert!(out.contains("\"line\""));
        assert!(out.contains("\"description\""));

        let expected_path = store::change_file(dir.path(), dir.path(), &id);
        assert!(expected_path.is_absolute());
        assert!(
            out.contains(&expected_path.display().to_string()),
            "absolute per-change path must appear in prompt; got:\n{out}"
        );
        assert!(
            out.contains("Per-change ("),
            "per-change bullet must label the change id; got:\n{out}"
        );

        assert!(
            !out.contains("secret body text"),
            "inline comment body must NOT appear in JsonlPaths mode; got:\n{out}"
        );
        assert!(
            !out.contains("### Change-Level Review Comments"),
            "inline comment headings must be dropped; got:\n{out}"
        );
    }

    /// Stack-file bullet appears iff the packet carries stack-scoped comments.
    /// Renderer derives this from `packet.stack_comments` — no caller-supplied
    /// flag, no fs probe.
    #[test]
    fn jsonl_mode_stack_file_bullet_keyed_off_packet_stack_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");

        let mut stack_comment = make_stack_comment("cross-cutting", Severity::Required);
        stack_comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &stack_comment).unwrap();

        let mut change_comment = make_change_comment(&id, "change body", Severity::Note);
        change_comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &change_comment).unwrap();

        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let with_stack = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        let stack_path = store::stack_file(dir.path(), dir.path());

        let stacked_out = render_prompt_with_mode(&with_stack, PromptMode::JsonlPaths);
        assert!(
            stacked_out.contains(&stack_path.display().to_string()),
            "stack file path must appear when packet has stack comments; got:\n{stacked_out}"
        );
        assert!(stacked_out.contains("Stack-scope:"));
        assert!(stacked_out.contains("revset_hash"));

        // Now drop the stack comment from the packet (simulating a single-change
        // review): no stack-scope bullet must appear.
        let no_stack = Packet {
            stack_comments: vec![],
            ..with_stack
        };
        let unstacked_out = render_prompt_with_mode(&no_stack, PromptMode::JsonlPaths);
        assert!(
            !unstacked_out.contains("Stack-scope:"),
            "stack-scope bullet must NOT appear when packet has no stack comments; got:\n{unstacked_out}"
        );
        assert!(
            !unstacked_out.contains(&stack_path.display().to_string()),
            "stack file path must NOT appear when packet has no stack comments; got:\n{unstacked_out}"
        );
    }

    #[test]
    fn jsonl_mode_keeps_diff_context_inline() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut line_comment = make_line_comment(
            &id,
            "src/client.rs",
            142,
            "use retry wrapper",
            Severity::Required,
        );
        line_comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &line_comment).unwrap();

        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet =
            build_packet(dir.path(), dir.path(), &stack, false, |_| Ok(simple_diff())).unwrap();

        let out = render_prompt_with_mode(&packet, PromptMode::JsonlPaths);

        assert!(
            out.contains("### Relevant Diff Context"),
            "diff context section must remain inline; got:\n{out}"
        );
        assert!(
            out.contains("@@ -140,7 +140,12 @@ impl Client {"),
            "hunk header must render inline; got:\n{out}"
        );
        assert!(
            !out.contains("### Line-Level Review Comments"),
            "inline line-level heading must be dropped; got:\n{out}"
        );
        assert!(
            !out.contains("use retry wrapper"),
            "inline line-comment body must be dropped; got:\n{out}"
        );
    }

    /// Schema instruction must direct claude to skip both stale and orphaned —
    /// inline mode drops stale via `filter_comments`; `JsonlPaths` mode points
    /// at JSONL files containing all statuses, so the prompt must enforce the
    /// equivalent filter.
    #[test]
    fn jsonl_mode_schema_excludes_both_stale_and_orphaned() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut comment = make_change_comment(&id, "body", Severity::Note);
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();
        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();

        let out = render_prompt_with_mode(&packet, PromptMode::JsonlPaths);
        assert!(
            out.contains("\"stale\"") && out.contains("\"orphaned\""),
            "schema filter line must name both stale and orphaned; got:\n{out}"
        );
        assert!(
            out.contains("Skip records whose status is \"stale\" or \"orphaned\""),
            "schema filter line drift; got:\n{out}"
        );
    }

    /// Paths in the prompt must be wrapped in backticks so a path with spaces
    /// or unusual characters cannot confuse claude's tokenizer.
    #[test]
    fn jsonl_mode_quotes_paths_with_backticks() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut sc = make_stack_comment("cross", Severity::Note);
        sc.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &sc).unwrap();
        let mut cc = make_change_comment(&id, "body", Severity::Note);
        cc.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &cc).unwrap();
        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();

        let out = render_prompt_with_mode(&packet, PromptMode::JsonlPaths);

        let change_path = store::change_file(dir.path(), dir.path(), &id);
        let stack_path = store::stack_file(dir.path(), dir.path());
        assert!(
            out.contains(&format!("`{}`", change_path.display())),
            "per-change path must be wrapped in backticks; got:\n{out}"
        );
        assert!(
            out.contains(&format!("`{}`", stack_path.display())),
            "stack-scope path must be wrapped in backticks; got:\n{out}"
        );
    }

    /// A change with only description-scoped comments must still get a
    /// per-change bullet — description comments live in the same JSONL file
    /// as line- and change-scoped comments. The renderer keys off the union
    /// of all three lists, not just `line_comments`.
    #[test]
    fn jsonl_mode_description_only_change_still_gets_per_change_bullet() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut comment = make_description_comment(
            &id,
            "tighten wording",
            Severity::Suggestion,
            "summary",
            Some(1),
        );
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        // Sanity: the change's only comment lives in description_comments.
        assert!(packet.changes[0].line_comments.is_empty());
        assert!(packet.changes[0].change_comments.is_empty());
        assert!(!packet.changes[0].description_comments.is_empty());

        let out = render_prompt_with_mode(&packet, PromptMode::JsonlPaths);
        let change_path = store::change_file(dir.path(), dir.path(), &id);
        assert!(
            out.contains(&format!("`{}`", change_path.display())),
            "description-only change must still get a per-change bullet; got:\n{out}"
        );
    }

    #[test]
    fn inline_mode_still_inlines_comments() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut comment = make_change_comment(&id, "inline body text", Severity::Suggestion);
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();
        let out = render_prompt_with_mode(&packet, PromptMode::Inline);

        assert!(out.contains("### Change-Level Review Comments"));
        assert!(out.contains("inline body text"));
        assert!(!out.contains("## Review comments\n"));
    }

    /// Pin `render_prompt` ≡ `render_prompt_with_mode(_, Inline)` so a future
    /// split cannot silently change `jjr packet`.
    #[test]
    fn render_prompt_equals_render_prompt_with_inline_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let id = cid("abc11111");
        let mut comment = make_change_comment(&id, "body", Severity::Note);
        comment.repo_root = dir.path().to_owned();
        store::save_comment(dir.path(), dir.path(), &comment).unwrap();

        let stack = make_resolved_stack(&[("abc11111", "first")]);
        let packet = build_packet(dir.path(), dir.path(), &stack, false, no_diff).unwrap();

        assert_eq!(
            render_prompt(&packet),
            render_prompt_with_mode(&packet, PromptMode::Inline),
        );
    }
}
