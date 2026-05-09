//! Pure anchoring algorithm.
//!
//! An *anchor* is the saved coordinates of a comment: the file, line, hunk
//! header, target text, and surrounding context lines (for line anchors); or
//! the description-line text and its neighbours (for description anchors).
//! Coordinates drift across review cycles as the agent edits the change, so
//! every reload must reconcile the saved anchor against the current state of
//! the change before the comment can be displayed.
//!
//! Given a saved anchor and the current state of the change (diff for line
//! anchors, description text for description anchors), the algorithm decides
//! whether the comment can be re-anchored cleanly or must be marked stale
//! (and why).

use crate::comment::{DescriptionAnchor, LineAnchor, MismatchReason, Side};
use crate::diff::{Diff, DiffFile, Hunk};

/// Outcome of attempting to re-anchor a saved comment.
///
/// The type parameter `A` is the anchor type returned on success:
/// [`LineAnchor`] for diff-line comments, [`DescriptionAnchor`] for
/// description-scoped comments. Using a single generic enum avoids two
/// parallel enums that would otherwise drift.
///
/// The wiring layer translates this into a Comment status update + persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorOutcome<A> {
    /// Exact match found. The returned anchor has updated position fields;
    /// the textual fields (`target_text`, `context_before`, `context_after`)
    /// are unchanged.
    ReAnchored(A),

    /// No exact match. The anchor is now stale; the variant carries the most
    /// informative [`MismatchReason`] the algorithm can determine.
    Stale(MismatchReason),
}

/// Stack-membership / orphaned status is checked elsewhere.
pub fn match_anchor(anchor: &LineAnchor, diff: &Diff) -> AnchorOutcome<LineAnchor> {
    let Some(diff_file) = diff
        .files
        .iter()
        .find(|f| f.display_path() == anchor.file.as_path())
    else {
        return AnchorOutcome::Stale(MismatchReason::FileNotInDiff);
    };

    let candidates = select_hunks(diff_file, anchor);
    let exact_matches = collect_exact_matches(&candidates, anchor);

    match exact_matches.len() {
        0 => fuzzy_match(&candidates, anchor),
        1 => AnchorOutcome::ReAnchored(build_updated_anchor(
            anchor,
            exact_matches[0].0,
            exact_matches[0].1,
        )),
        _ => resolve_multiple_exact(anchor, &exact_matches),
    }
}

/// An empty `description` is treated as "description not present" and returns
/// `Stale(AnchorNotFound)`.
pub fn match_description_anchor(
    anchor: &DescriptionAnchor,
    description: &str,
) -> AnchorOutcome<DescriptionAnchor> {
    if description.is_empty() {
        return AnchorOutcome::Stale(MismatchReason::AnchorNotFound);
    }

    let lines: Vec<&str> = description.lines().collect();

    let exact_matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(idx, _)| desc_is_exact_match(&lines, *idx, anchor))
        .map(|(idx, _)| idx)
        .collect();

    match exact_matches.len() {
        0 => desc_fuzzy_match(&lines, anchor),
        1 => AnchorOutcome::ReAnchored(desc_build_updated_anchor(anchor, exact_matches[0])),
        _ => desc_resolve_multiple_exact(anchor, &exact_matches),
    }
}

/// A hunk header has the form `@@ -N,N +N,N @@ <function-context>`.
pub(crate) fn extract_function_context(hunk_header: &str) -> Option<&str> {
    let after_first = hunk_header.strip_prefix("@@")?;
    let closing = after_first.find("@@").map(|i| &after_first[i + 2..])?;
    let trimmed = closing.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn select_hunks<'h>(file: &'h DiffFile, anchor: &LineAnchor) -> Vec<&'h Hunk> {
    let all: Vec<&Hunk> = file.hunks().iter().collect();

    let Some(ctx) = extract_function_context(&anchor.hunk_header) else {
        return all;
    };

    let filtered: Vec<&Hunk> = all
        .iter()
        .copied()
        .filter(|h| h.function_context.as_deref() == Some(ctx))
        .collect();

    if filtered.is_empty() {
        all
    } else {
        filtered
    }
}

fn collect_exact_matches<'h>(
    candidates: &[&'h Hunk],
    anchor: &LineAnchor,
) -> Vec<(&'h Hunk, usize)> {
    let mut matches = Vec::new();
    for &hunk in candidates {
        for idx in 0..hunk.lines.len() {
            if is_exact_match(hunk, idx, anchor) {
                matches.push((hunk, idx));
            }
        }
    }
    matches
}

pub(crate) fn is_exact_match(hunk: &Hunk, line_idx: usize, anchor: &LineAnchor) -> bool {
    let line = &hunk.lines[line_idx];
    if line.text != anchor.target_text {
        return false;
    }
    let before = gather_context_before(hunk, line_idx, anchor.context_before.len());
    let after = gather_context_after(hunk, line_idx, anchor.context_after.len());
    context_window_matches(&before, &anchor.context_before)
        && context_window_matches(&after, &anchor.context_after)
}

pub(crate) fn gather_context_before(hunk: &Hunk, line_idx: usize, n: usize) -> Vec<&str> {
    hunk.lines[..line_idx]
        .iter()
        .rev()
        .take(n)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub(crate) fn gather_context_after(hunk: &Hunk, line_idx: usize, n: usize) -> Vec<&str> {
    hunk.lines[line_idx + 1..]
        .iter()
        .take(n)
        .map(|l| l.text.as_str())
        .collect()
}

/// At a hunk boundary the stored context cannot be confirmed, so we
/// return `false` rather than vacuously accepting the match.
fn context_window_matches(available: &[&str], stored: &[String]) -> bool {
    if stored.is_empty() {
        return true;
    }
    if available.is_empty() {
        return false;
    }
    if available.len() > stored.len() {
        return false;
    }
    let n = available.len();
    available == stored[..n].iter().map(String::as_str).collect::<Vec<_>>()
}

fn resolve_multiple_exact(
    anchor: &LineAnchor,
    matches: &[(&Hunk, usize)],
) -> AnchorOutcome<LineAnchor> {
    let reference_line = match anchor.side {
        Side::New => anchor.new_line,
        Side::Old => anchor.old_line,
    };

    let Some(ref_num) = reference_line else {
        return AnchorOutcome::Stale(MismatchReason::AnchorNotFound);
    };

    match find_closest_match(anchor, matches, ref_num) {
        ClosestMatch::Unique(hunk, line_idx) => {
            AnchorOutcome::ReAnchored(build_updated_anchor(anchor, hunk, line_idx))
        }
        ClosestMatch::Tied | ClosestMatch::None => {
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        }
    }
}

enum ClosestMatch<'h> {
    Unique(&'h Hunk, usize),
    Tied,
    None,
}

fn find_closest_match<'h>(
    anchor: &LineAnchor,
    matches: &[(&'h Hunk, usize)],
    ref_num: u32,
) -> ClosestMatch<'h> {
    let mut best_dist: Option<u32> = None;
    let mut best: Option<(&Hunk, usize)> = None;
    let mut tied = false;

    for &(hunk, line_idx) in matches {
        let line = &hunk.lines[line_idx];
        let line_num = match anchor.side {
            Side::New => line.target_line,
            Side::Old => line.source_line,
        };
        let Some(num) = line_num else {
            continue;
        };
        let dist = ref_num.abs_diff(num);
        match best_dist {
            None => {
                best_dist = Some(dist);
                best = Some((hunk, line_idx));
                tied = false;
            }
            Some(d) if dist < d => {
                best_dist = Some(dist);
                best = Some((hunk, line_idx));
                tied = false;
            }
            Some(d) if dist == d => {
                tied = true;
            }
            _ => {}
        }
    }

    if tied {
        return ClosestMatch::Tied;
    }
    match best {
        Some((h, i)) => ClosestMatch::Unique(h, i),
        None => ClosestMatch::None,
    }
}

fn build_updated_anchor(anchor: &LineAnchor, hunk: &Hunk, line_idx: usize) -> LineAnchor {
    let line = &hunk.lines[line_idx];
    LineAnchor {
        file: anchor.file.clone(),
        side: anchor.side,
        old_line: line.source_line,
        new_line: line.target_line,
        hunk_header: hunk.header.clone(),
        target_text: anchor.target_text.clone(),
        context_before: anchor.context_before.clone(),
        context_after: anchor.context_after.clone(),
    }
    .normalized()
}

fn fuzzy_match(candidates: &[&Hunk], anchor: &LineAnchor) -> AnchorOutcome<LineAnchor> {
    if let Some(reason) = check_body_changed(candidates, anchor) {
        return AnchorOutcome::Stale(reason);
    }
    if let Some(reason) = check_context_drifted(candidates, anchor) {
        return AnchorOutcome::Stale(reason);
    }
    AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
}

fn check_body_changed(candidates: &[&Hunk], anchor: &LineAnchor) -> Option<MismatchReason> {
    // Without context brackets the body-changed signal is unconfirmable.
    if anchor.context_before.is_empty() && anchor.context_after.is_empty() {
        return None;
    }
    for &hunk in candidates {
        for line_idx in 0..hunk.lines.len() {
            let before = gather_context_before(hunk, line_idx, anchor.context_before.len());
            let after = gather_context_after(hunk, line_idx, anchor.context_after.len());
            let before_ok = context_window_matches(&before, &anchor.context_before);
            let after_ok = context_window_matches(&after, &anchor.context_after);
            let text_differs = hunk.lines[line_idx].text != anchor.target_text;
            if before_ok && after_ok && text_differs {
                return Some(MismatchReason::TargetTextChanged);
            }
        }
    }
    None
}

fn check_context_drifted(candidates: &[&Hunk], anchor: &LineAnchor) -> Option<MismatchReason> {
    let matching_lines: Vec<(&Hunk, usize)> = candidates
        .iter()
        .flat_map(|&hunk| {
            hunk.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.text == anchor.target_text)
                .map(move |(i, _)| (hunk, i))
        })
        .collect();

    if matching_lines.len() != 1 {
        return None;
    }

    let (hunk, line_idx) = matching_lines[0];
    let before = gather_context_before(hunk, line_idx, anchor.context_before.len());
    let after = gather_context_after(hunk, line_idx, anchor.context_after.len());
    let before_ok = context_window_matches(&before, &anchor.context_before);
    let after_ok = context_window_matches(&after, &anchor.context_after);

    match (before_ok, after_ok) {
        (true, false) => Some(MismatchReason::ContextAfterChanged),
        (false, true) => Some(MismatchReason::ContextBeforeChanged),
        _ => None,
    }
}

fn desc_context_before<'a>(lines: &[&'a str], line_idx: usize, n: usize) -> Vec<&'a str> {
    lines[..line_idx]
        .iter()
        .rev()
        .take(n)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn desc_context_after<'a>(lines: &[&'a str], line_idx: usize, n: usize) -> Vec<&'a str> {
    lines[line_idx + 1..].iter().take(n).copied().collect()
}

fn desc_is_exact_match(lines: &[&str], line_idx: usize, anchor: &DescriptionAnchor) -> bool {
    if lines[line_idx] != anchor.target_text {
        return false;
    }
    let before = desc_context_before(lines, line_idx, anchor.context_before.len());
    let after = desc_context_after(lines, line_idx, anchor.context_after.len());
    context_window_matches(&before, &anchor.context_before)
        && context_window_matches(&after, &anchor.context_after)
}

fn desc_build_updated_anchor(anchor: &DescriptionAnchor, line_idx: usize) -> DescriptionAnchor {
    DescriptionAnchor {
        display_line: u32::try_from(line_idx + 1).ok(),
        target_text: anchor.target_text.clone(),
        context_before: anchor.context_before.clone(),
        context_after: anchor.context_after.clone(),
    }
    .normalized()
}

fn desc_resolve_multiple_exact(
    anchor: &DescriptionAnchor,
    matches: &[usize],
) -> AnchorOutcome<DescriptionAnchor> {
    let Some(ref_line) = anchor.display_line else {
        return AnchorOutcome::Stale(MismatchReason::AnchorNotFound);
    };

    let mut best_dist: Option<u32> = None;
    let mut best_idx: Option<usize> = None;
    let mut tied = false;

    for &line_idx in matches {
        let candidate_line = u32::try_from(line_idx + 1).unwrap_or(u32::MAX);
        let dist = ref_line.abs_diff(candidate_line);
        match best_dist {
            None => {
                best_dist = Some(dist);
                best_idx = Some(line_idx);
                tied = false;
            }
            Some(d) if dist < d => {
                best_dist = Some(dist);
                best_idx = Some(line_idx);
                tied = false;
            }
            Some(d) if dist == d => {
                tied = true;
            }
            _ => {}
        }
    }

    if tied {
        return AnchorOutcome::Stale(MismatchReason::AnchorNotFound);
    }
    match best_idx {
        Some(idx) => AnchorOutcome::ReAnchored(desc_build_updated_anchor(anchor, idx)),
        None => AnchorOutcome::Stale(MismatchReason::AnchorNotFound),
    }
}

fn desc_fuzzy_match(
    lines: &[&str],
    anchor: &DescriptionAnchor,
) -> AnchorOutcome<DescriptionAnchor> {
    if let Some(reason) = desc_check_body_changed(lines, anchor) {
        return AnchorOutcome::Stale(reason);
    }
    if let Some(reason) = desc_check_context_drifted(lines, anchor) {
        return AnchorOutcome::Stale(reason);
    }
    AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
}

fn desc_check_body_changed(lines: &[&str], anchor: &DescriptionAnchor) -> Option<MismatchReason> {
    if anchor.context_before.is_empty() && anchor.context_after.is_empty() {
        return None;
    }
    for (line_idx, _) in lines.iter().enumerate() {
        let before = desc_context_before(lines, line_idx, anchor.context_before.len());
        let after = desc_context_after(lines, line_idx, anchor.context_after.len());
        let before_ok = context_window_matches(&before, &anchor.context_before);
        let after_ok = context_window_matches(&after, &anchor.context_after);
        let text_differs = lines[line_idx] != anchor.target_text;
        if before_ok && after_ok && text_differs {
            return Some(MismatchReason::TargetTextChanged);
        }
    }
    None
}

fn desc_check_context_drifted(
    lines: &[&str],
    anchor: &DescriptionAnchor,
) -> Option<MismatchReason> {
    let matching: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == anchor.target_text)
        .map(|(i, _)| i)
        .collect();

    if matching.len() != 1 {
        return None;
    }

    let line_idx = matching[0];
    let before = desc_context_before(lines, line_idx, anchor.context_before.len());
    let after = desc_context_after(lines, line_idx, anchor.context_after.len());
    let before_ok = context_window_matches(&before, &anchor.context_before);
    let after_ok = context_window_matches(&after, &anchor.context_after);

    match (before_ok, after_ok) {
        (true, false) => Some(MismatchReason::ContextAfterChanged),
        (false, true) => Some(MismatchReason::ContextBeforeChanged),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::comment::{DescriptionAnchor, LineAnchor, MismatchReason, Side};
    use crate::diff::{Diff, DiffFile, Hunk, Line, LineKind};

    fn make_line(kind: LineKind, text: &str, src: Option<u32>, tgt: Option<u32>) -> Line {
        Line {
            kind,
            text: text.to_owned(),
            source_line: src,
            target_line: tgt,
        }
    }

    fn ctx_line(text: &str, src: u32, tgt: u32) -> Line {
        make_line(LineKind::Context, text, Some(src), Some(tgt))
    }

    fn added_line(text: &str, tgt: u32) -> Line {
        make_line(LineKind::Added, text, None, Some(tgt))
    }

    fn removed_line(text: &str, src: u32) -> Line {
        make_line(LineKind::Removed, text, Some(src), None)
    }

    fn make_hunk(header: &str, fn_ctx: Option<&str>, lines: Vec<Line>) -> Hunk {
        let source_length =
            u32::try_from(lines.iter().filter(|l| l.source_line.is_some()).count()).unwrap();
        let target_length =
            u32::try_from(lines.iter().filter(|l| l.target_line.is_some()).count()).unwrap();
        Hunk {
            header: header.to_owned(),
            function_context: fn_ctx.map(str::to_owned),
            source_start: 1,
            source_length,
            target_start: 1,
            target_length,
            lines,
        }
    }

    fn make_diff(files: Vec<DiffFile>) -> Diff {
        Diff { files }
    }

    fn modified_file(path: &str, hunks: Vec<Hunk>) -> DiffFile {
        DiffFile::Modified {
            path: PathBuf::from(path),
            hunks,
        }
    }

    fn renamed_file(from: &str, to: &str, hunks: Vec<Hunk>) -> DiffFile {
        DiffFile::Renamed {
            from: PathBuf::from(from),
            to: PathBuf::from(to),
            hunks,
        }
    }

    struct AnchorSpec<'a> {
        file: &'a str,
        side: Side,
        old_line: Option<u32>,
        new_line: Option<u32>,
        hunk_header: &'a str,
        target_text: &'a str,
        before: Vec<&'a str>,
        after: Vec<&'a str>,
    }

    impl AnchorSpec<'_> {
        fn build(self) -> LineAnchor {
            LineAnchor {
                file: PathBuf::from(self.file),
                side: self.side,
                old_line: self.old_line,
                new_line: self.new_line,
                hunk_header: self.hunk_header.to_owned(),
                target_text: self.target_text.to_owned(),
                context_before: self.before.into_iter().map(str::to_owned).collect(),
                context_after: self.after.into_iter().map(str::to_owned).collect(),
            }
        }
    }

    #[test]
    fn extract_function_context_with_fn_segment() {
        assert_eq!(
            extract_function_context("@@ -1,3 +1,4 @@ impl Foo"),
            Some("impl Foo")
        );
    }

    #[test]
    fn extract_function_context_no_fn_segment() {
        assert_eq!(extract_function_context("@@ -1,3 +1,4 @@"), None);
    }

    #[test]
    fn extract_function_context_empty_fn_segment_after_trim() {
        assert_eq!(extract_function_context("@@ -1,3 +1,4 @@   "), None);
    }

    #[test]
    fn gather_context_before_returns_ordered_lines() {
        let hunk = make_hunk(
            "@@ -1,4 +1,4 @@",
            None,
            vec![
                ctx_line("a", 1, 1),
                ctx_line("b", 2, 2),
                ctx_line("c", 3, 3),
                ctx_line("target", 4, 4),
            ],
        );
        assert_eq!(gather_context_before(&hunk, 3, 3), vec!["a", "b", "c"]);
    }

    #[test]
    fn gather_context_before_capped_at_available() {
        let hunk = make_hunk(
            "@@ -1,2 +1,2 @@",
            None,
            vec![ctx_line("only", 1, 1), ctx_line("target", 2, 2)],
        );
        assert_eq!(gather_context_before(&hunk, 1, 3), vec!["only"]);
    }

    #[test]
    fn gather_context_after_returns_ordered_lines() {
        let hunk = make_hunk(
            "@@ -1,4 +1,4 @@",
            None,
            vec![
                ctx_line("target", 1, 1),
                ctx_line("x", 2, 2),
                ctx_line("y", 3, 3),
                ctx_line("z", 4, 4),
            ],
        );
        assert_eq!(gather_context_after(&hunk, 0, 3), vec!["x", "y", "z"]);
    }

    #[test]
    fn gather_context_after_capped_at_available() {
        let hunk = make_hunk(
            "@@ -1,2 +1,2 @@",
            None,
            vec![ctx_line("target", 1, 1), ctx_line("last", 2, 2)],
        );
        assert_eq!(gather_context_after(&hunk, 0, 3), vec!["last"]);
    }

    #[test]
    fn file_not_in_diff_returns_file_not_in_diff() {
        let diff = make_diff(vec![modified_file("other.rs", vec![])]);
        let a = AnchorSpec {
            file: "src/foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(5),
            hunk_header: "@@ -1 +1 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::FileNotInDiff)
        );
    }

    #[test]
    fn file_present_no_hunks_returns_anchor_not_found() {
        let diff = make_diff(vec![modified_file("foo.rs", vec![])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@ -1 +1 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn exact_match_unique_produces_reanchored_with_updated_line_numbers_and_header() {
        let hunk = make_hunk(
            "@@ -10,4 +10,4 @@ impl Foo",
            Some("impl Foo"),
            vec![
                ctx_line("before1", 10, 10),
                ctx_line("before2", 11, 11),
                ctx_line("target line", 12, 12),
                ctx_line("after1", 13, 13),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: Some(12),
            new_line: Some(12),
            hunk_header: "@@ -10,4 +10,4 @@ impl Foo",
            target_text: "target line",
            before: vec!["before1", "before2"],
            after: vec!["after1"],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.new_line, Some(12));
        assert_eq!(updated.old_line, Some(12));
        assert_eq!(updated.hunk_header, "@@ -10,4 +10,4 @@ impl Foo");
        assert_eq!(updated.target_text, "target line");
    }

    #[test]
    fn exact_match_on_old_side_for_removed_line() {
        let hunk = make_hunk(
            "@@ -5,3 +5,2 @@",
            None,
            vec![
                ctx_line("before", 4, 4),
                removed_line("removed target", 5),
                ctx_line("after", 6, 5),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::Old,
            old_line: Some(5),
            new_line: None,
            hunk_header: "@@ -5,3 +5,2 @@",
            target_text: "removed target",
            before: vec!["before"],
            after: vec!["after"],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.old_line, Some(5));
        assert_eq!(updated.new_line, None);
    }

    #[test]
    fn exact_match_with_shorter_context_window_at_hunk_start_accepted() {
        let hunk = make_hunk(
            "@@ -1,5 +1,5 @@",
            None,
            vec![
                ctx_line("a", 1, 1),
                ctx_line("b", 2, 2),
                ctx_line("target", 3, 3),
                ctx_line("after1", 4, 4),
                ctx_line("after2", 5, 5),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(3),
            hunk_header: "@@ -1,5 +1,5 @@",
            target_text: "target",
            before: vec!["a", "b", "older-than-hunk"],
            after: vec!["after1", "after2"],
        }
        .build();
        assert!(matches!(
            match_anchor(&a, &diff),
            AnchorOutcome::ReAnchored(_)
        ));
    }

    #[test]
    fn exact_match_with_shorter_context_window_at_hunk_end_accepted() {
        let hunk = make_hunk(
            "@@ -1,5 +1,5 @@",
            None,
            vec![
                ctx_line("before1", 1, 1),
                ctx_line("before2", 2, 2),
                ctx_line("target", 3, 3),
                ctx_line("a", 4, 4),
                ctx_line("b", 5, 5),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(3),
            hunk_header: "@@ -1,5 +1,5 @@",
            target_text: "target",
            before: vec!["before1", "before2"],
            after: vec!["a", "b", "newer-than-hunk"],
        }
        .build();
        assert!(matches!(
            match_anchor(&a, &diff),
            AnchorOutcome::ReAnchored(_)
        ));
    }

    #[test]
    fn multiple_exact_matches_closest_by_display_line_wins() {
        let hunk = make_hunk(
            "@@ -1,5 +1,5 @@",
            None,
            vec![
                ctx_line("target", 5, 5),
                ctx_line("middle", 6, 6),
                ctx_line("filler", 7, 7),
                ctx_line("filler2", 8, 8),
                ctx_line("target", 9, 9),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(6),
            hunk_header: "@@ -1,5 +1,5 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.new_line, Some(5));
    }

    #[test]
    fn multiple_exact_matches_no_recorded_line_number_returns_anchor_not_found() {
        let hunk = make_hunk(
            "@@ -0,0 +1,2 @@",
            None,
            vec![added_line("target", 1), added_line("target", 2)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::Old,
            old_line: None,
            new_line: None,
            hunk_header: "@@ -0,0 +1,2 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn multiple_exact_matches_equal_distance_returns_anchor_not_found() {
        let hunk = make_hunk(
            "@@ -3,5 +3,5 @@",
            None,
            vec![
                ctx_line("filler", 3, 3),
                ctx_line("target", 4, 4),
                ctx_line("ref", 5, 5),
                ctx_line("target", 6, 6),
                ctx_line("filler2", 7, 7),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(5),
            hunk_header: "@@ -3,5 +3,5 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn fuzzy_context_bracket_matches_but_target_changed() {
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                ctx_line("before", 1, 1),
                ctx_line("new body", 2, 2),
                ctx_line("after", 3, 3),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@",
            target_text: "old body",
            before: vec!["before"],
            after: vec!["after"],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::TargetTextChanged)
        );
    }

    #[test]
    fn fuzzy_target_matches_context_before_differs() {
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                ctx_line("different_before", 1, 1),
                ctx_line("target", 2, 2),
                ctx_line("same_after", 3, 3),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@",
            target_text: "target",
            before: vec!["original_before"],
            after: vec!["same_after"],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::ContextBeforeChanged)
        );
    }

    #[test]
    fn fuzzy_target_matches_context_after_differs() {
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                ctx_line("same_before", 1, 1),
                ctx_line("target", 2, 2),
                ctx_line("different_after", 3, 3),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@",
            target_text: "target",
            before: vec!["same_before"],
            after: vec!["original_after"],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::ContextAfterChanged)
        );
    }

    #[test]
    fn fuzzy_target_matches_both_contexts_differ_returns_anchor_not_found() {
        let hunk = make_hunk(
            "@@ -1,3 +1,3 @@",
            None,
            vec![
                ctx_line("different_before", 1, 1),
                ctx_line("target", 2, 2),
                ctx_line("different_after", 3, 3),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,3 +1,3 @@",
            target_text: "target",
            before: vec!["original_before"],
            after: vec!["original_after"],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn function_context_filtering_finds_matching_hunk() {
        let hunk_a = make_hunk(
            "@@ -1,2 +1,2 @@ fn alpha",
            Some("fn alpha"),
            vec![ctx_line("target", 1, 1), ctx_line("other", 2, 2)],
        );
        let hunk_b = make_hunk(
            "@@ -10,2 +10,2 @@ fn beta",
            Some("fn beta"),
            vec![ctx_line("target", 10, 10), ctx_line("other2", 11, 11)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk_a, hunk_b])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(10),
            hunk_header: "@@ -10,2 +10,2 @@ fn beta",
            target_text: "target",
            before: vec![],
            after: vec!["other2"],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.new_line, Some(10));
    }

    #[test]
    fn function_context_no_matching_hunk_falls_back_to_all_hunks() {
        let hunk_a = make_hunk(
            "@@ -1,2 +1,2 @@ fn alpha",
            Some("fn alpha"),
            vec![ctx_line("target line", 1, 1), ctx_line("other", 2, 2)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk_a])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@ -1,2 +1,2 @@ fn nonexistent",
            target_text: "target line",
            before: vec![],
            after: vec!["other"],
        }
        .build();
        assert!(
            matches!(match_anchor(&a, &diff), AnchorOutcome::ReAnchored(_)),
            "fallback to all hunks should find match"
        );
    }

    #[test]
    fn all_hunks_searched_when_anchor_has_no_function_context() {
        let hunk_a = make_hunk("@@ -1,1 +1,1 @@", None, vec![ctx_line("not here", 1, 1)]);
        let hunk_b = make_hunk("@@ -20,1 +20,1 @@", None, vec![ctx_line("target", 20, 20)]);
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk_a, hunk_b])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(20),
            hunk_header: "@@ -20,1 +20,1 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert!(
            matches!(match_anchor(&a, &diff), AnchorOutcome::ReAnchored(_)),
            "should search all hunks"
        );
    }

    #[test]
    fn renamed_file_matched_by_to_path() {
        let hunk = make_hunk("@@ -1,1 +1,1 @@", None, vec![ctx_line("target", 1, 1)]);
        let diff = make_diff(vec![renamed_file("old.rs", "new.rs", vec![hunk])]);
        // Anchor stored against `to` path (new.rs), which is what display_path() returns.
        let a = AnchorSpec {
            file: "new.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@ -1,1 +1,1 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert!(
            matches!(match_anchor(&a, &diff), AnchorOutcome::ReAnchored(_)),
            "renamed file should match by `to` path"
        );
    }

    #[test]
    fn hunk_header_updated_on_reanchor() {
        let hunk = make_hunk(
            "@@ -20,3 +20,3 @@ fn new_location",
            Some("fn new_location"),
            vec![
                ctx_line("before", 20, 20),
                ctx_line("target", 21, 21),
                ctx_line("after", 22, 22),
            ],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(21),
            hunk_header: "@@ -5,3 +5,3 @@ fn old_location",
            target_text: "target",
            before: vec!["before"],
            after: vec!["after"],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.hunk_header, "@@ -20,3 +20,3 @@ fn new_location");
    }

    #[test]
    fn multiple_exact_matches_all_candidates_lack_side_line_returns_anchor_not_found() {
        let hunk = make_hunk(
            "@@ -1,2 +0,0 @@",
            None,
            vec![removed_line("target", 1), removed_line("target", 2)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(5),
            hunk_header: "@@ -1,2 +0,0 @@",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn fuzzy_body_changed_with_asymmetric_context_only_before_populated() {
        let hunk = make_hunk(
            "@@ -1,2 +1,2 @@",
            None,
            vec![ctx_line("before", 1, 1), ctx_line("new body", 2, 2)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(2),
            hunk_header: "@@ -1,2 +1,2 @@",
            target_text: "old body",
            before: vec!["before"],
            after: vec![],
        }
        .build();
        assert_eq!(
            match_anchor(&a, &diff),
            AnchorOutcome::Stale(MismatchReason::TargetTextChanged)
        );
    }

    #[test]
    fn function_context_filter_with_multiple_exact_matches_picks_closest() {
        let hunk_a = make_hunk(
            "@@ -10,1 +10,1 @@ fn shared",
            Some("fn shared"),
            vec![ctx_line("target", 10, 10)],
        );
        let hunk_b = make_hunk(
            "@@ -50,1 +50,1 @@ fn shared",
            Some("fn shared"),
            vec![ctx_line("target", 50, 50)],
        );
        let diff = make_diff(vec![modified_file("foo.rs", vec![hunk_a, hunk_b])]);
        let a = AnchorSpec {
            file: "foo.rs",
            side: Side::New,
            old_line: None,
            new_line: Some(48),
            hunk_header: "@@ -50,1 +50,1 @@ fn shared",
            target_text: "target",
            before: vec![],
            after: vec![],
        }
        .build();
        let AnchorOutcome::ReAnchored(updated) = match_anchor(&a, &diff) else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.new_line, Some(50));
    }

    #[test]
    fn context_window_matches_returns_false_when_available_exceeds_stored() {
        let available = vec!["a", "b", "c", "d"];
        let stored = vec!["a".to_owned(), "b".to_owned()];
        assert!(!context_window_matches(&available, &stored));
    }

    fn make_desc_anchor(
        target: &str,
        display_line: Option<u32>,
        before: Vec<&str>,
        after: Vec<&str>,
    ) -> DescriptionAnchor {
        DescriptionAnchor {
            display_line,
            target_text: target.to_owned(),
            context_before: before.into_iter().map(str::to_owned).collect(),
            context_after: after.into_iter().map(str::to_owned).collect(),
        }
    }

    #[test]
    fn desc_exact_match_unique_returns_reanchored_with_updated_display_line() {
        let description = "First line\nTarget line\nThird line";
        let anchor = make_desc_anchor(
            "Target line",
            Some(2),
            vec!["First line"],
            vec!["Third line"],
        );
        let outcome = match_description_anchor(&anchor, description);
        let AnchorOutcome::ReAnchored(updated) = outcome else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.display_line, Some(2));
        assert_eq!(updated.target_text, "Target line");
    }

    #[test]
    fn desc_exact_match_on_first_line() {
        let description = "Target line\nSecond line\nThird line";
        let anchor = make_desc_anchor("Target line", Some(1), vec![], vec!["Second line"]);
        let outcome = match_description_anchor(&anchor, description);
        assert!(matches!(outcome, AnchorOutcome::ReAnchored(_)));
        let AnchorOutcome::ReAnchored(updated) = outcome else {
            unreachable!()
        };
        assert_eq!(updated.display_line, Some(1));
    }

    #[test]
    fn desc_exact_match_shorter_context_at_description_start() {
        let description = "A\nB\nTarget\nD\nE";
        // anchor was saved with 3-line context_before; at re-anchor time only 2 lines
        // are before target. The algorithm checks stored[..available.len()], so the
        // available lines ("A","B") must be the leading entries of stored, with the
        // third entry ("pre-A") trailing.
        let anchor = make_desc_anchor("Target", Some(3), vec!["A", "B", "pre-A"], vec!["D"]);
        let outcome = match_description_anchor(&anchor, description);
        assert!(
            matches!(outcome, AnchorOutcome::ReAnchored(_)),
            "shorter available context should still match"
        );
    }

    #[test]
    fn desc_multiple_exact_matches_closest_by_display_line_wins() {
        let description = "target\nfiller\nfiller2\ntarget\nfiller3";
        let anchor = make_desc_anchor("target", Some(1), vec![], vec![]);
        let AnchorOutcome::ReAnchored(updated) = match_description_anchor(&anchor, description)
        else {
            panic!("expected ReAnchored");
        };
        assert_eq!(updated.display_line, Some(1));
    }

    #[test]
    fn desc_multiple_exact_matches_no_display_line_returns_anchor_not_found() {
        let description = "target\nfiller\ntarget";
        let anchor = make_desc_anchor("target", None, vec![], vec![]);
        assert_eq!(
            match_description_anchor(&anchor, description),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn desc_multiple_exact_matches_equal_distance_returns_anchor_not_found() {
        let description = "target\nref\ntarget";
        // display_line=2 (the "ref" line index+1) is equidistant from lines 1 and 3
        let anchor = make_desc_anchor("target", Some(2), vec![], vec![]);
        assert_eq!(
            match_description_anchor(&anchor, description),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn desc_fuzzy_body_changed_context_matches() {
        let description = "before\nnew body\nafter";
        let anchor = make_desc_anchor("old body", Some(2), vec!["before"], vec!["after"]);
        assert_eq!(
            match_description_anchor(&anchor, description),
            AnchorOutcome::Stale(MismatchReason::TargetTextChanged)
        );
    }

    #[test]
    fn desc_fuzzy_target_moved_context_before_differs() {
        let description = "different_before\ntarget\nsame_after";
        let anchor = make_desc_anchor(
            "target",
            Some(2),
            vec!["original_before"],
            vec!["same_after"],
        );
        assert_eq!(
            match_description_anchor(&anchor, description),
            AnchorOutcome::Stale(MismatchReason::ContextBeforeChanged)
        );
    }

    #[test]
    fn desc_fuzzy_target_moved_context_after_differs() {
        let description = "same_before\ntarget\ndifferent_after";
        let anchor = make_desc_anchor(
            "target",
            Some(2),
            vec!["same_before"],
            vec!["original_after"],
        );
        assert_eq!(
            match_description_anchor(&anchor, description),
            AnchorOutcome::Stale(MismatchReason::ContextAfterChanged)
        );
    }

    #[test]
    fn desc_empty_description_returns_anchor_not_found() {
        let anchor = make_desc_anchor("target", Some(1), vec![], vec![]);
        assert_eq!(
            match_description_anchor(&anchor, ""),
            AnchorOutcome::Stale(MismatchReason::AnchorNotFound)
        );
    }

    #[test]
    fn desc_target_text_newlines_flattened_by_normalized() {
        let anchor = DescriptionAnchor {
            display_line: Some(1),
            target_text: "foo\nbar".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text, "foo bar");
    }
}
