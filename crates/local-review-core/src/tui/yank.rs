//! Yank a window of diff lines around the cursor to the system clipboard.
//!
//! The user can press `y` in the file or entity diff view to copy a
//! `file:start-end` header plus the surrounding diff text (with `+`/`-`/` `
//! prefixes intact) as a fenced `diff` code block. The intended workflow is
//! to paste the result into a Claude conversation as ready-made context.
//!
//! The formatter is pure (no clipboard I/O); `copy_to_clipboard` is the thin
//! shell that calls `arboard`. Keeping the format function separate makes
//! the rendering testable without spawning a clipboard.

use crate::tui::diff_view::{DiffView, RenderedLine, RenderedLineKind};

/// Number of lines on each side of the cursor to include in the yanked
/// window. Picked to match what fits in a normal Claude prompt comfortably
/// while still giving enough context to ask about non-trivial changes.
pub const YANK_RADIUS: usize = 10;

/// Format a `±YANK_RADIUS` window of diff lines around `cursor_idx` into
/// markdown ready for pasting into a chat.
///
/// The window is bounded by view edges and skips synthetic rows (hunk
/// separators, notices, inline-comment annotations) that aren't part of the
/// underlying source diff. The header reports the file's after-state line
/// range covered by the kept rows; when no after-state lines are present
/// (a pure-removal window), it falls back to the source range.
pub fn format_yank(view: &DiffView, cursor_idx: usize) -> Option<String> {
    if view.lines.is_empty() {
        return None;
    }
    let lo = cursor_idx.saturating_sub(YANK_RADIUS);
    let hi = cursor_idx
        .saturating_add(YANK_RADIUS)
        .min(view.lines.len() - 1);

    let kept: Vec<&RenderedLine> = view.lines[lo..=hi]
        .iter()
        .filter(|l| is_source_line(l))
        .collect();
    if kept.is_empty() {
        return None;
    }

    let (lo_num, hi_num) = numeric_range(&kept);
    let header = format_header(&view.title, lo_num, hi_num);

    let mut out = String::with_capacity(256);
    out.push_str(&header);
    out.push_str("\n\n```diff\n");
    for line in &kept {
        out.push_str(&format_line(line));
        out.push('\n');
    }
    out.push_str("```\n");
    Some(out)
}

/// Push the formatted window into the system clipboard via `arboard`.
///
/// Returns the human-friendly summary for the status bar on success; on
/// failure returns the underlying clipboard error so the caller can show it.
/// The function is deliberately small and side-effect-only — keeping the
/// `arboard::Clipboard` instance out of long-lived state avoids platform
/// pitfalls where the clipboard handle locks resources after the TUI is
/// already in raw mode (we open, write, drop in one call).
pub fn copy_to_clipboard(content: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(content).map_err(|e| e.to_string())
}

/// Lines that participate in the underlying diff text. Hunk headers are
/// included so the pasted block reads like `git diff` output (Claude
/// recognises `@@ -N,N +N,N @@` and the surrounding `+`/`-`/` ` prefixes
/// as a unified diff).
fn is_source_line(line: &RenderedLine) -> bool {
    matches!(
        line.kind,
        RenderedLineKind::Added
            | RenderedLineKind::Removed
            | RenderedLineKind::Context
            | RenderedLineKind::HunkHeader
            | RenderedLineKind::DescriptionLine
    )
}

/// Apply the unified-diff prefix the renderer normally paints separately.
/// The `text` field on `RenderedLine` carries the raw source (no prefix).
///
/// The `format_yank` filter rejects every kind other than `Added` /
/// `Removed` / `Context` / `HunkHeader` / `DescriptionLine`, so this
/// function is only ever called on those. The catch-all arm exists so the
/// match stays exhaustive without panicking on a hypothetical future variant.
fn format_line(line: &RenderedLine) -> String {
    match line.kind {
        RenderedLineKind::Added => format!("+{}", line.text),
        RenderedLineKind::Removed => format!("-{}", line.text),
        RenderedLineKind::Context => format!(" {}", line.text),
        // Hunk headers already start with `@@`; description lines are free-form
        // prose; the synthetic variants (filtered out upstream) get the same
        // pass-through treatment for forward-compat.
        RenderedLineKind::HunkHeader
        | RenderedLineKind::DescriptionLine
        | RenderedLineKind::HunkSeparator
        | RenderedLineKind::Notice
        | RenderedLineKind::InlineCommentMeta { .. }
        | RenderedLineKind::InlineCommentBody => line.text.clone(),
    }
}

/// Compute the (lo, hi) numeric range across `kept`, preferring after-state
/// (`target_line`) numbers and falling back to source-state for pure
/// removals. Returns `None` when neither side has any line numbers (an
/// all-header or all-description window).
fn numeric_range(kept: &[&RenderedLine]) -> (Option<u32>, Option<u32>) {
    let targets: Vec<u32> = kept.iter().filter_map(|l| l.target_line).collect();
    if !targets.is_empty() {
        let lo = targets.iter().min().copied();
        let hi = targets.iter().max().copied();
        return (lo, hi);
    }
    let sources: Vec<u32> = kept.iter().filter_map(|l| l.source_line).collect();
    if sources.is_empty() {
        return (None, None);
    }
    (sources.iter().min().copied(), sources.iter().max().copied())
}

fn format_header(title: &str, lo: Option<u32>, hi: Option<u32>) -> String {
    let base = strip_status_suffix(title);
    match (lo, hi) {
        (Some(a), Some(b)) if a == b => format!("`{base}:{a}`"),
        (Some(a), Some(b)) => format!("`{base}:{a}-{b}`"),
        _ => format!("`{base}`"),
    }
}

/// Drop trailing ` (added)`, ` (removed)`, ` (binary)` annotations the
/// renderer appends to titles — they're useful in the UI but the pasted
/// header should look like a plain file path.
fn strip_status_suffix(title: &str) -> &str {
    for suffix in [" (added)", " (removed)", " (binary)"] {
        if let Some(stripped) = title.strip_suffix(suffix) {
            return stripped;
        }
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::diff_view::DiffView;

    fn rendered(
        kind: RenderedLineKind,
        text: &str,
        src: Option<u32>,
        tgt: Option<u32>,
    ) -> RenderedLine {
        RenderedLine {
            kind,
            text: text.to_owned(),
            source_line: src,
            target_line: tgt,
            hunk_header: None,
            comment_severity: None,
        }
    }

    fn ctx(text: &str, src: u32, tgt: u32) -> RenderedLine {
        rendered(RenderedLineKind::Context, text, Some(src), Some(tgt))
    }

    fn added(text: &str, tgt: u32) -> RenderedLine {
        rendered(RenderedLineKind::Added, text, None, Some(tgt))
    }

    fn removed(text: &str, src: u32) -> RenderedLine {
        rendered(RenderedLineKind::Removed, text, Some(src), None)
    }

    fn header(text: &str) -> RenderedLine {
        rendered(RenderedLineKind::HunkHeader, text, None, None)
    }

    fn build_view(title: &str, lines: Vec<RenderedLine>) -> DiffView {
        DiffView::from_lines(title.to_owned(), lines)
    }

    #[test]
    fn format_empty_view_returns_none() {
        let view = build_view("foo.rs", vec![]);
        assert!(format_yank(&view, 0).is_none());
    }

    #[test]
    fn format_window_clipped_to_view_bounds() {
        // 3 lines total, cursor at 1, radius 10 → window covers all 3.
        let view = build_view(
            "foo.rs",
            vec![ctx("a", 1, 1), ctx("b", 2, 2), ctx("c", 3, 3)],
        );
        let out = format_yank(&view, 1).expect("non-empty");
        assert!(
            out.contains("`foo.rs:1-3`"),
            "header missing range; got {out}"
        );
        assert!(out.contains(" a"), "context `a` missing; got {out}");
        assert!(out.contains(" c"), "context `c` missing; got {out}");
    }

    #[test]
    fn format_applies_prefix_per_line_kind() {
        let view = build_view(
            "foo.rs",
            vec![
                ctx("ctx", 1, 1),
                added("new", 2),
                removed("old", 2),
                ctx("more ctx", 2, 3),
            ],
        );
        let out = format_yank(&view, 0).expect("non-empty");
        assert!(out.contains(" ctx"), "context prefix missing");
        assert!(out.contains("+new"), "added prefix missing");
        assert!(out.contains("-old"), "removed prefix missing");
    }

    #[test]
    fn format_skips_synthetic_rows() {
        let view = build_view(
            "foo.rs",
            vec![
                ctx("real", 1, 1),
                rendered(RenderedLineKind::HunkSeparator, "---", None, None),
                rendered(RenderedLineKind::Notice, "(binary)", None, None),
                ctx("real2", 2, 2),
            ],
        );
        let out = format_yank(&view, 1).expect("non-empty");
        assert!(
            !out.contains("(binary)"),
            "notice should not appear; got {out}"
        );
        assert!(
            !out.contains("---"),
            "separator should not appear; got {out}"
        );
        assert!(out.contains(" real") && out.contains(" real2"));
    }

    #[test]
    fn format_falls_back_to_source_when_only_removals_present() {
        // A pure-removal window: every kept line lacks `target_line`. The
        // header should still produce a range using `source_line`.
        let view = build_view(
            "foo.rs",
            vec![removed("a", 10), removed("b", 11), removed("c", 12)],
        );
        let out = format_yank(&view, 1).expect("non-empty");
        assert!(
            out.contains("`foo.rs:10-12`"),
            "expected source-line range fallback; got {out}"
        );
    }

    #[test]
    fn format_keeps_hunk_header_text_verbatim() {
        let view = build_view(
            "foo.rs",
            vec![
                header("@@ -10,3 +10,3 @@ fn bar"),
                ctx("a", 10, 10),
                added("b", 11),
            ],
        );
        let out = format_yank(&view, 1).expect("non-empty");
        assert!(
            out.contains("@@ -10,3 +10,3 @@ fn bar"),
            "hunk header should appear verbatim; got {out}"
        );
    }

    #[test]
    fn format_strips_status_suffix_from_title() {
        // Renderer titles often look like "foo.rs (added)"; the yank header
        // should drop the parenthetical so the pasted reference is a clean
        // path that Claude can navigate to in the user's repo.
        let view = build_view("foo.rs (added)", vec![ctx("a", 1, 1)]);
        let out = format_yank(&view, 0).expect("non-empty");
        assert!(
            out.starts_with("`foo.rs:"),
            "suffix not stripped; got {out}"
        );
    }

    #[test]
    fn format_single_line_range_uses_single_number() {
        let view = build_view("foo.rs", vec![ctx("only", 42, 42)]);
        let out = format_yank(&view, 0).expect("non-empty");
        assert!(
            out.contains("`foo.rs:42`"),
            "single-line header wrong; got {out}"
        );
        assert!(!out.contains("42-42"));
    }
}
