use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{JjrError, Result};

/// A jj change ID, e.g. `abc12345` or `abc12345/1` for divergent changes.
///
/// Stored as separate `head` and `divergence` fields so callers can reason
/// about each part independently. The combined canonical form (`head` plus
/// optional `/<divergence>`) is also kept so [`as_str`] and the transparent
/// serde wire format remain a borrowed `&str`.
///
/// Parsed and validated at the boundary; downstream code can trust the value.
/// Deserialization is validated — constructing an invalid `ChangeId` via JSON
/// is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct ChangeId {
    head: String,
    divergence: Option<u32>,
    canonical: String,
}

impl ChangeId {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let (head_str, div_str) = match trimmed.split_once('/') {
            None => (trimmed, None),
            Some((h, d)) => (h, Some(d)),
        };

        if !is_valid_head(head_str) {
            return Err(JjrError::InvalidChangeId {
                raw: raw.to_owned(),
            });
        }

        let divergence = match div_str {
            None => None,
            Some(d) => match d.parse::<u32>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return Err(JjrError::InvalidChangeId {
                        raw: raw.to_owned(),
                    });
                }
            },
        };

        let canonical = match divergence {
            None => head_str.to_owned(),
            Some(n) => format!("{head_str}/{n}"),
        };

        Ok(Self {
            head: head_str.to_owned(),
            divergence,
            canonical,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn divergence(&self) -> Option<u32> {
        self.divergence
    }

    /// Filename-safe encoding for storage on disk.
    ///
    /// Divergent change IDs from jj carry a `/<index>` disambiguator (e.g. `abc/1`).
    /// The slash would create directory hierarchy on disk, so it is replaced with
    /// an underscore for the filename only. The canonical in-memory form is preserved.
    pub fn to_filename(&self) -> String {
        match self.divergence {
            None => self.head.clone(),
            Some(n) => format!("{}_{}", self.head, n),
        }
    }
}

impl From<ChangeId> for String {
    fn from(id: ChangeId) -> Self {
        id.canonical
    }
}

impl<'de> Deserialize<'de> for ChangeId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

/// A jj commit ID (hex SHA).
///
/// Deserialization is validated — constructing an invalid `CommitId` via JSON
/// is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CommitId(String);

impl CommitId {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if !is_valid_commit_id(trimmed) {
            return Err(JjrError::InvalidCommitId {
                raw: raw.to_owned(),
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CommitId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates the head of a jj change ID. Must be at least 8 ASCII-alphanumeric
/// characters. The optional `/<digits>` divergence disambiguator is parsed
/// separately by [`ChangeId::parse`].
fn is_valid_head(head: &str) -> bool {
    head.len() >= 8 && head.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Validates a jj commit ID.
///
/// Must be at least 8 ASCII-hex characters (jj abbreviates commit hashes to
/// at least 8 chars in all output).
fn is_valid_commit_id(raw: &str) -> bool {
    raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_change_id() {
        let id = ChangeId::parse("abc33333").unwrap();
        assert_eq!(id.as_str(), "abc33333");
        assert_eq!(id.to_filename(), "abc33333");
    }

    #[test]
    fn parse_divergent_change_id() {
        let id = ChangeId::parse("abc11111/1").unwrap();
        assert_eq!(id.as_str(), "abc11111/1");
        assert_eq!(id.to_filename(), "abc11111_1");
    }

    #[test]
    fn rejects_empty_change_id() {
        assert!(matches!(
            ChangeId::parse(""),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn rejects_change_id_with_whitespace_inside() {
        assert!(matches!(
            ChangeId::parse("abc 33333"),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn rejects_change_id_with_non_digit_disambiguator() {
        assert!(matches!(
            ChangeId::parse("abc12345/foo"),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn rejects_change_id_with_empty_disambiguator() {
        assert!(matches!(
            ChangeId::parse("abc12345/"),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn trims_whitespace_around_change_id() {
        let id = ChangeId::parse("  abc33333\n").unwrap();
        assert_eq!(id.as_str(), "abc33333");
    }

    #[test]
    fn rejects_whitespace_only_change_id() {
        assert!(matches!(
            ChangeId::parse("   "),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn rejects_non_ascii_change_id() {
        assert!(matches!(
            ChangeId::parse("αβγδεζηθ"),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn rejects_too_short_change_id() {
        assert!(matches!(
            ChangeId::parse("a"),
            Err(JjrError::InvalidChangeId { .. })
        ));
    }

    #[test]
    fn accepts_exactly_eight_char_change_id() {
        let id = ChangeId::parse("a1b2c3d4").unwrap();
        assert_eq!(id.as_str(), "a1b2c3d4");
    }

    #[test]
    fn parse_commit_id() {
        let id = CommitId::parse("a1b2c3d4").unwrap();
        assert_eq!(id.as_str(), "a1b2c3d4");
    }

    #[test]
    fn rejects_non_hex_commit_id() {
        assert!(matches!(
            CommitId::parse("xyz"),
            Err(JjrError::InvalidCommitId { .. })
        ));
    }

    #[test]
    fn rejects_empty_commit_id() {
        assert!(matches!(
            CommitId::parse(""),
            Err(JjrError::InvalidCommitId { .. })
        ));
    }

    #[test]
    fn rejects_too_short_commit_id() {
        assert!(matches!(
            CommitId::parse("a1b2c3d"),
            Err(JjrError::InvalidCommitId { .. })
        ));
    }

    #[test]
    fn accepts_exactly_eight_char_commit_id() {
        let id = CommitId::parse("0123abcd").unwrap();
        assert_eq!(id.as_str(), "0123abcd");
    }

    #[test]
    fn change_id_serde_roundtrip() {
        let id = ChangeId::parse("abc11111/1").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""abc11111/1""#);
        let parsed: ChangeId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn change_id_serde_rejects_invalid_on_deserialize() {
        let result = serde_json::from_str::<ChangeId>(r#""invalid id""#);
        assert!(
            result.is_err(),
            "expected deserialization to fail for invalid change ID"
        );
    }

    #[test]
    fn commit_id_serde_rejects_invalid_on_deserialize() {
        let result = serde_json::from_str::<CommitId>(r#""xyz!!""#);
        assert!(
            result.is_err(),
            "expected deserialization to fail for invalid commit ID"
        );
    }
}
