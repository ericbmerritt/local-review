//! Structured entity identity that replaces the Phase 1 `PlaceholderEntityId`.
//!
//! `EntityId` is a tuple `(file_path, scope_chain, signature_key, ordinal)`
//! serialised as JSON for cache files and comment storage. String concatenation
//! is not used because file paths may contain `::` and Windows paths contain `:`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::strip_controls;

/// Structured entity identity.
///
/// Encoded as a JSON object for disk storage and comment schemas. The
/// `display_name` is derived from `scope_chain` + language convention at
/// render time and is never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId {
    /// Repo-relative file path; control characters stripped at construction.
    pub file_path: PathBuf,
    /// Container chain from outermost to innermost, ending with the entity
    /// name (e.g., `["AuthService", "authenticate"]`). Each segment is UTF-8
    /// with control characters stripped.
    pub scope_chain: Vec<String>,
    /// Language-specific parameter signature distinguishing overloads, e.g.,
    /// `"(int)"` for Java or `"(&self, &str) -> bool"` for Rust. `None` for
    /// languages that do not support overloads or for containers and config
    /// properties. `None` entities fall back to `ordinal` for disambiguation.
    pub signature_key: Option<String>,
    /// Zero-based index among entities sharing the same
    /// `(file_path, scope_chain, signature_key)`, ordered by source position.
    /// `0` for the common (non-duplicate) case.
    pub ordinal: u32,
}

impl EntityId {
    /// Construct an `EntityId` from pre-validated components.
    ///
    /// `strip_controls` is applied to all string fields at construction.
    pub fn new(
        file_path: impl Into<PathBuf>,
        scope_chain: Vec<String>,
        signature_key: Option<String>,
        ordinal: u32,
    ) -> Self {
        let raw = file_path.into();
        let file_path = PathBuf::from(strip_controls(&raw.to_string_lossy()));
        let scope_chain = scope_chain
            .into_iter()
            .map(|s| strip_controls(&s))
            .collect();
        let signature_key = signature_key.map(|s| strip_controls(&s));
        Self {
            file_path,
            scope_chain,
            signature_key,
            ordinal,
        }
    }

    /// The entity name — the last element of `scope_chain`.
    pub fn name(&self) -> &str {
        self.scope_chain.last().map(String::as_str).unwrap_or("")
    }

    /// Dot-joined display name without the file path.
    ///
    /// Language surfaces should override this with language-native syntax
    /// (e.g., `::` for Rust). This is a fallback used when no language
    /// context is available.
    pub fn display_name(&self) -> String {
        self.scope_chain.join(".")
    }
}

// ── Ordinal computation ───────────────────────────────────────────────────────

/// Assign ordinals to a set of raw `(scope_chain, signature_key, start_line)`
/// tuples from one file, mutating the `ordinals` output slice in place.
///
/// Entities that share `(scope_chain, signature_key)` are sorted by
/// `start_line` and assigned ordinals 0, 1, 2, … in source order. Entities
/// with unique identities always get ordinal 0.
/// Key type used when grouping entities for ordinal assignment.
type OrdinalGroupKey = (Vec<String>, Option<String>);

pub fn assign_ordinals(keys: &[(Vec<String>, Option<String>, u32)], ordinals: &mut [u32]) {
    // Group indices by (scope_chain, signature_key).
    let mut groups: std::collections::HashMap<OrdinalGroupKey, Vec<(usize, u32)>> =
        std::collections::HashMap::new();

    for (idx, (scope, sig, start)) in keys.iter().enumerate() {
        groups
            .entry((scope.clone(), sig.clone()))
            .or_default()
            .push((idx, *start));
    }

    for members in groups.values() {
        if members.len() == 1 {
            ordinals[members[0].0] = 0;
            continue;
        }
        // Sort by start_line to assign stable ordinals.
        let mut sorted = members.clone();
        sorted.sort_unstable_by_key(|&(_, line)| line);
        for (ordinal, &(orig_idx, _)) in sorted.iter().enumerate() {
            ordinals[orig_idx] = u32::try_from(ordinal).unwrap_or(u32::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinal_zero_for_unique_entity() {
        let keys = vec![(vec!["foo".to_owned()], None, 1u32)];
        let mut ordinals = vec![99u32];
        assign_ordinals(&keys, &mut ordinals);
        assert_eq!(ordinals[0], 0);
    }

    #[test]
    fn ordinals_assigned_by_source_order() {
        // Two impl Foo blocks at lines 10 and 5 — line 5 should be ordinal 0.
        let keys = vec![
            (vec!["Foo".to_owned()], None, 10u32),
            (vec!["Foo".to_owned()], None, 5u32),
        ];
        let mut ordinals = vec![99u32; 2];
        assign_ordinals(&keys, &mut ordinals);
        // index 0 (line 10) → ordinal 1; index 1 (line 5) → ordinal 0
        assert_eq!(ordinals[0], 1, "later entity gets ordinal 1");
        assert_eq!(ordinals[1], 0, "earlier entity gets ordinal 0");
    }

    #[test]
    fn signature_key_distinguishes_overloads() {
        let keys = vec![
            (vec!["foo".to_owned()], Some("(int)".to_owned()), 1u32),
            (vec!["foo".to_owned()], Some("(String)".to_owned()), 5u32),
        ];
        let mut ordinals = vec![99u32; 2];
        assign_ordinals(&keys, &mut ordinals);
        // Different signature_key → both ordinal 0.
        assert_eq!(ordinals[0], 0);
        assert_eq!(ordinals[1], 0);
    }

    #[test]
    fn strip_controls_applied() {
        let id = EntityId::new(
            PathBuf::from("src/\x1bfile.rs"),
            vec!["Foo\x1b".to_owned()],
            None,
            0,
        );
        assert!(!id.file_path.to_string_lossy().contains('\x1b'));
        assert!(!id.scope_chain[0].contains('\x1b'));
    }
}
