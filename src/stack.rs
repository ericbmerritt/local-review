use std::fmt::Write as _;

use crate::change_id::{ChangeId, CommitId};

/// A resolved stack of jj changes for the given revset.
#[derive(Debug, Clone)]
pub struct ResolvedStack {
    /// The revset string used for the resolution. After a fallback (e.g. when
    /// `trunk()` is unresolvable) this holds the *fallback* revset, not the
    /// original — keeping `revset_hash` and `entries` in agreement.
    pub revset: String,
    /// BLAKE3 hash of the canonicalized revset. Stable key for cursor storage.
    pub revset_hash: RevsetHash,
    /// Ordered entries, oldest-first (as `jj log` returns them for typical
    /// ancestry revsets).
    pub entries: Vec<StackEntry>,
}

/// One change in a resolved stack.
#[derive(Debug, Clone)]
pub struct StackEntry {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    /// First line of the change description.
    pub description: String,
}

/// A 32-byte BLAKE3 hash of a canonicalized revset string.
///
/// Wrapping the raw bytes in a newtype makes the cursor key distinct from
/// other 32-byte hashes the codebase may grow later (file-content hashes,
/// commit hashes, etc.) — the compiler refuses to mix them.
///
/// Canonicalization is intentionally permissive: lowercase + collapse
/// whitespace runs to single spaces. This means `Feature` and `feature`
/// produce the same hash. Revsets containing case-sensitive identifiers
/// (a future jj could grow them) would collide; the trade-off is accepted
/// for now because the cursor file is local-user state, not a content
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RevsetHash([u8; 32]);

impl RevsetHash {
    /// Compute the BLAKE3 hash of a revset string after canonicalization.
    #[must_use]
    pub fn from_revset(revset: &str) -> Self {
        let canonical = canonicalize_revset(revset);
        Self(*blake3::hash(canonical.as_bytes()).as_bytes())
    }

    /// Borrow the raw bytes (rarely needed; prefer `hex`).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding (64 chars). Used as the cursor.json key.
    #[must_use]
    pub fn hex(&self) -> String {
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// Parse a lowercase hex string produced by [`RevsetHash::hex`].
    ///
    /// Returns `None` if the input is not exactly 64 lowercase hex characters.
    #[must_use]
    pub fn from_hex_str(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn canonicalize_revset(revset: &str) -> String {
    revset
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revset_hash_is_deterministic() {
        let a = RevsetHash::from_revset("trunk()..@");
        let b = RevsetHash::from_revset("trunk()..@");
        assert_eq!(a, b);
    }

    #[test]
    fn revset_hash_normalizes_case() {
        let lower = RevsetHash::from_revset("trunk()..@");
        let upper = RevsetHash::from_revset("TRUNK()..@");
        assert_eq!(lower, upper);
    }

    #[test]
    fn revset_hash_normalizes_whitespace() {
        let single = RevsetHash::from_revset("a b c");
        let multi = RevsetHash::from_revset("a   b   c");
        assert_eq!(single, multi);
    }

    #[test]
    fn revset_hash_normalizes_tabs_and_newlines() {
        let normal = RevsetHash::from_revset("a b c");
        let tabbed = RevsetHash::from_revset("a\tb\tc");
        let newlined = RevsetHash::from_revset("a\nb\nc");
        assert_eq!(normal, tabbed);
        assert_eq!(normal, newlined);
    }

    #[test]
    fn different_revsets_produce_different_hashes() {
        let a = RevsetHash::from_revset("@");
        let b = RevsetHash::from_revset("@-");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_32_bytes() {
        let h = RevsetHash::from_revset("@");
        assert_eq!(h.as_bytes().len(), 32);
    }

    #[test]
    fn hex_is_64_chars() {
        let h = RevsetHash::from_revset("@");
        assert_eq!(h.hex().len(), 64);
    }

    #[test]
    fn canonicalize_trims_edges() {
        let a = RevsetHash::from_revset("  @  ");
        let b = RevsetHash::from_revset("@");
        assert_eq!(a, b);
    }

    /// Document the deliberate case-insensitivity decision (see `RevsetHash` docs).
    #[test]
    fn case_insensitive_hash_is_documented_behavior() {
        let mixed = RevsetHash::from_revset("Feature");
        let lower = RevsetHash::from_revset("feature");
        assert_eq!(
            mixed, lower,
            "canonicalization lowercases identifier-like portions of revsets"
        );
    }

    #[test]
    fn from_hex_str_roundtrips_with_hex() {
        let original = RevsetHash::from_revset("trunk()..@");
        let hex = original.hex();
        let restored = RevsetHash::from_hex_str(&hex).expect("valid hex must parse");
        assert_eq!(original, restored);
    }

    #[test]
    fn from_hex_str_rejects_wrong_length() {
        assert!(RevsetHash::from_hex_str("abc").is_none());
        assert!(RevsetHash::from_hex_str("").is_none());
        let too_long = "a".repeat(65);
        assert!(RevsetHash::from_hex_str(&too_long).is_none());
    }

    #[test]
    fn from_hex_str_rejects_uppercase() {
        let h = RevsetHash::from_revset("@");
        let upper = h.hex().to_uppercase();
        assert!(
            RevsetHash::from_hex_str(&upper).is_none(),
            "from_hex_str only accepts lowercase hex"
        );
    }

    #[test]
    fn from_hex_str_rejects_non_hex_chars() {
        let bad: String = "g".repeat(64);
        assert!(RevsetHash::from_hex_str(&bad).is_none());
    }
}
