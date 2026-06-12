//! Pure anchor types for locating reviewer comments in diffs and descriptions.
//!
//! These types describe *where* a comment is attached — file, line, context
//! window — not what the comment contains or how it is stored. Both `jjr` and
//! `ggr` build their own comment models on top of these shared anchor
//! primitives.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const TARGET_TEXT_MAX: usize = 1024;
pub const CONTEXT_MAX: usize = 3;
pub const TRUNCATION_SUFFIX: &str = "…";

/// Which side of the diff a line comment targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl Serialize for Side {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Old => serializer.serialize_str("old"),
            Self::New => serializer.serialize_str("new"),
        }
    }
}

impl<'de> Deserialize<'de> for Side {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "old" => Ok(Self::Old),
            "new" => Ok(Self::New),
            other => Err(serde::de::Error::custom(format!(
                "unknown side \"{other}\", expected \"old\" or \"new\""
            ))),
        }
    }
}

/// Why a line anchor failed to re-match after the diff changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchReason {
    TargetTextChanged,
    ContextBeforeChanged,
    ContextAfterChanged,
    AnchorNotFound,
    FileNotInDiff,
}

impl Serialize for MismatchReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::TargetTextChanged => serializer.serialize_str("target_text changed"),
            Self::ContextBeforeChanged => serializer.serialize_str("context_before changed"),
            Self::ContextAfterChanged => serializer.serialize_str("context_after changed"),
            Self::AnchorNotFound => serializer.serialize_str("anchor not found"),
            Self::FileNotInDiff => serializer.serialize_str("file not in diff"),
        }
    }
}

impl<'de> Deserialize<'de> for MismatchReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "target_text changed" => Ok(Self::TargetTextChanged),
            "context_before changed" => Ok(Self::ContextBeforeChanged),
            "context_after changed" => Ok(Self::ContextAfterChanged),
            "anchor not found" => Ok(Self::AnchorNotFound),
            "file not in diff" => Ok(Self::FileNotInDiff),
            other => Err(serde::de::Error::custom(format!(
                "unknown mismatch_reason \"{other}\""
            ))),
        }
    }
}

/// Durable anchor for a line comment — survives small edits via text matching.
///
/// Construct via struct literal and call [`LineAnchor::normalized`] to
/// enforce the spec limits ([`TARGET_TEXT_MAX`]-char `target_text`,
/// ≤[`CONTEXT_MAX`] context lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAnchor {
    pub file: PathBuf,
    pub side: Side,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub hunk_header: String,
    pub target_text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

impl LineAnchor {
    /// Apply spec constraints, returning a normalized copy:
    /// - `target_text` is truncated to [`TARGET_TEXT_MAX`] chars with the
    ///   ellipsis suffix appended.
    /// - `context_before` and `context_after` are each capped at
    ///   [`CONTEXT_MAX`] entries.
    ///
    /// Called at every trust boundary (read-time deserialization, save-time
    /// serialization) so untrusted JSONL input cannot smuggle in oversized
    /// fields.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            target_text: truncate_target_text(&self.target_text),
            context_before: cap_context(self.context_before),
            context_after: cap_context(self.context_after),
            ..self
        }
    }
}

/// Durable anchor for a description-scoped comment — survives small edits via
/// text matching, the same as [`LineAnchor`].
///
/// Descriptions have only one version (no old/new diff sides) and are not
/// divided into hunks, so neither `side` nor `hunk_header` are carried.
/// Construct via struct literal and call [`DescriptionAnchor::normalized`] to
/// enforce the spec limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionAnchor {
    /// 1-based line number in the description at save time. Used as a
    /// tie-breaker when multiple identical lines match.
    pub display_line: Option<u32>,
    pub target_text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

impl DescriptionAnchor {
    /// Apply the same spec constraints as [`LineAnchor::normalized`].
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            target_text: truncate_target_text(&self.target_text),
            context_before: cap_context(self.context_before),
            context_after: cap_context(self.context_after),
            ..self
        }
    }
}

fn truncate_with_ellipsis(s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s;
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push_str(TRUNCATION_SUFFIX);
    truncated
}

fn truncate_target_text(s: &str) -> String {
    // `target_text` is rendered as a single line in the packet format; a
    // literal `\n` would break the byte-stable output downstream tooling
    // depends on. Strip at the trust boundary so the renderer never sees them.
    let oneline: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    truncate_with_ellipsis(oneline, TARGET_TEXT_MAX)
}

/// Three-point fingerprint for re-anchoring a line comment after edits.
///
/// Hashes are whitespace-normalized so reformatting alone does not break
/// anchors. `before_hash` and `after_hash` are 0 when the line is at the
/// top/bottom of the file respectively.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AnchorFingerprint {
    pub line_hash: u64,
    pub before_hash: u64,
    pub after_hash: u64,
}

impl AnchorFingerprint {
    /// Compute from a line and its immediate neighbours.
    pub fn compute(line: &str, before: &str, after: &str) -> Self {
        fn normalize_hash(s: &str) -> u64 {
            if s.is_empty() {
                return 0;
            }
            let normalized: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            crate::semantic::extractor::content_hash(&normalized)
        }
        Self {
            line_hash: normalize_hash(line),
            before_hash: normalize_hash(before),
            after_hash: normalize_hash(after),
        }
    }

    /// Score this fingerprint against a candidate.
    ///
    /// - 3 points: `line_hash` matches (mandatory for ≥3)
    /// - 1 point each: `before_hash`, `after_hash`
    pub fn score_against(&self, candidate: &Self) -> u8 {
        let mut score = 0u8;
        if self.line_hash == candidate.line_hash {
            score += 3;
        }
        if self.before_hash == candidate.before_hash {
            score += 1;
        }
        if self.after_hash == candidate.after_hash {
            score += 1;
        }
        score
    }
}

fn cap_context(lines: Vec<String>) -> Vec<String> {
    if lines.len() <= CONTEXT_MAX {
        lines
    } else {
        lines.into_iter().take(CONTEXT_MAX).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn target_text_truncates_at_1024_chars() {
        let long_text: String = "x".repeat(2000);
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: long_text,
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text.chars().count(), TARGET_TEXT_MAX + 1);
        assert!(anchor.target_text.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn target_text_not_truncated_when_short() {
        let short = "hello world".to_owned();
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: short.clone(),
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text, short);
    }

    #[test]
    fn target_text_exact_1024_chars_not_truncated() {
        let exact: String = "a".repeat(TARGET_TEXT_MAX);
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: exact.clone(),
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text, exact);
    }

    #[test]
    fn context_before_capped_at_3_entries() {
        let many: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: "target".to_owned(),
            context_before: many,
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.context_before.len(), CONTEXT_MAX);
    }

    #[test]
    fn context_after_capped_at_3_entries() {
        let many: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: "target".to_owned(),
            context_before: vec![],
            context_after: many,
        }
        .normalized();
        assert_eq!(anchor.context_after.len(), CONTEXT_MAX);
    }

    #[test]
    fn target_text_with_embedded_newline_is_flattened() {
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: "foo\nbar\r\nbaz".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text, "foo bar  baz");
    }

    #[test]
    fn target_text_one_over_limit_truncates_to_limit_plus_ellipsis() {
        let one_over: String = "a".repeat(TARGET_TEXT_MAX + 1);
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: one_over,
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text.chars().count(), TARGET_TEXT_MAX + 1);
        assert!(anchor.target_text.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn context_before_exactly_4_capped_at_3() {
        let four = vec![
            "1".to_owned(),
            "2".to_owned(),
            "3".to_owned(),
            "4".to_owned(),
        ];
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: "t".to_owned(),
            context_before: four,
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.context_before, vec!["1", "2", "3"]);
    }

    #[test]
    fn context_before_exactly_3_preserved() {
        let three = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(1),
            hunk_header: "@@".to_owned(),
            target_text: "t".to_owned(),
            context_before: three.clone(),
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.context_before, three);
    }

    #[test]
    fn normalized_preserves_short_context() {
        let before = vec!["a".to_owned(), "b".to_owned()];
        let after = vec!["c".to_owned()];
        let anchor = LineAnchor {
            file: PathBuf::from("f.rs"),
            side: Side::Old,
            old_line: Some(5),
            new_line: None,
            hunk_header: "@@".to_owned(),
            target_text: "removed".to_owned(),
            context_before: before.clone(),
            context_after: after.clone(),
        }
        .normalized();
        assert_eq!(anchor.context_before, before);
        assert_eq!(anchor.context_after, after);
    }

    #[test]
    fn side_wire_format_roundtrips() {
        for (side, wire) in [(Side::Old, "\"old\""), (Side::New, "\"new\"")] {
            let json = serde_json::to_string(&side).unwrap();
            assert_eq!(json, wire);
            let parsed: Side = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, side);
        }
    }

    #[test]
    fn mismatch_reason_wire_format_roundtrips() {
        let cases = [
            (MismatchReason::TargetTextChanged, "\"target_text changed\""),
            (
                MismatchReason::ContextBeforeChanged,
                "\"context_before changed\"",
            ),
            (
                MismatchReason::ContextAfterChanged,
                "\"context_after changed\"",
            ),
            (MismatchReason::AnchorNotFound, "\"anchor not found\""),
            (MismatchReason::FileNotInDiff, "\"file not in diff\""),
        ];
        for (reason, wire) in cases {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, wire);
            let parsed: MismatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[test]
    fn unknown_side_errors() {
        let err = serde_json::from_str::<Side>(r#""both""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown side"), "got: {err}");
    }

    #[test]
    fn unknown_mismatch_reason_errors() {
        let err = serde_json::from_str::<MismatchReason>(r#""whatever""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown mismatch_reason"), "got: {err}");
    }

    #[test]
    fn desc_anchor_target_text_newlines_flattened_by_normalized() {
        let anchor = DescriptionAnchor {
            display_line: Some(1),
            target_text: "foo\nbar\r\nbaz".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
        .normalized();
        assert_eq!(anchor.target_text, "foo bar  baz");
    }

    #[test]
    fn desc_anchor_context_capped_by_normalized() {
        let many: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let anchor = DescriptionAnchor {
            display_line: None,
            target_text: "target".to_owned(),
            context_before: many.clone(),
            context_after: many,
        }
        .normalized();
        assert_eq!(anchor.context_before.len(), CONTEXT_MAX);
        assert_eq!(anchor.context_after.len(), CONTEXT_MAX);
    }
}
