//! Reviewer-assigned severity weight for comments.
//!
//! Shared between `jjr` (local jj stack review) and `ggr` (GitHub PR review).
//! Serialization follows the same lowercase kebab-case strings used by the
//! on-disk JSONL format so both tools can round-trip the value identically.

use serde::{Deserialize, Serialize};

/// Reviewer-assigned weight: how much attention the comment demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Note,
    Suggestion,
    Required,
}

impl Serialize for Severity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Note => serializer.serialize_str("note"),
            Self::Suggestion => serializer.serialize_str("suggestion"),
            Self::Required => serializer.serialize_str("required"),
        }
    }
}

impl<'de> Deserialize<'de> for Severity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "note" => Ok(Self::Note),
            "suggestion" => Ok(Self::Suggestion),
            "required" => Ok(Self::Required),
            other => Err(serde::de::Error::custom(format!(
                "unknown severity \"{other}\", expected note/suggestion/required"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Severity;

    fn serialize(s: Severity) -> String {
        serde_json::to_string(&s).expect("serialize")
    }

    fn deserialize(s: &str) -> Result<Severity, serde_json::Error> {
        serde_json::from_str(s)
    }

    #[test]
    fn serialize_note_produces_lowercase_string() {
        assert_eq!(serialize(Severity::Note), r#""note""#);
    }

    #[test]
    fn serialize_suggestion_produces_lowercase_string() {
        assert_eq!(serialize(Severity::Suggestion), r#""suggestion""#);
    }

    #[test]
    fn serialize_required_produces_lowercase_string() {
        assert_eq!(serialize(Severity::Required), r#""required""#);
    }

    #[test]
    fn deserialize_round_trips_all_variants() {
        assert_eq!(deserialize(r#""note""#).unwrap(), Severity::Note);
        assert_eq!(
            deserialize(r#""suggestion""#).unwrap(),
            Severity::Suggestion
        );
        assert_eq!(deserialize(r#""required""#).unwrap(), Severity::Required);
    }

    #[test]
    fn deserialize_unknown_string_returns_err() {
        assert!(
            deserialize(r#""critical""#).is_err(),
            "unknown variant must deserialize to Err"
        );
    }

    /// The serde impl is case-sensitive: capital-R "Required" must not match.
    #[test]
    fn deserialize_is_case_sensitive() {
        assert!(
            deserialize(r#""Required""#).is_err(),
            "capitalized variant must not deserialize"
        );
        assert!(
            deserialize(r#""Note""#).is_err(),
            "capitalized variant must not deserialize"
        );
        assert!(
            deserialize(r#""Suggestion""#).is_err(),
            "capitalized variant must not deserialize"
        );
    }
}
