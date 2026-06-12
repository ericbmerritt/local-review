//! jjr comment model — flat reviewer notes anchored to diff lines,
//! change descriptions, whole changes, or the entire stack.
//!
//! The anchor *location* types ([`LineAnchor`], [`DescriptionAnchor`],
//! [`Side`], [`MismatchReason`]) are shared with `ggr` and live in
//! `local_review_core`. This module re-exports them and adds the
//! jjr-specific wrapper: a flat [`Comment`] with severity/status lifecycle
//! and a custom JSONL wire format.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::change_id::{ChangeId, CommitId};
use crate::error::{JjrError, Result};
use crate::stack::RevsetHash;

pub use local_review_core::comment::{
    DescriptionAnchor, LineAnchor, MismatchReason, Side, CONTEXT_MAX, TARGET_TEXT_MAX,
    TRUNCATION_SUFFIX,
};
pub use local_review_core::severity::Severity;

pub(crate) const SCHEMA_VERSION_VALUE: &str = "diff-comment/v2";
pub(crate) const BODY_MAX: usize = 64 * 1024;

/// Marker type that always serializes/deserializes as `"diff-comment/v2"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaVersion;

impl Serialize for SchemaVersion {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(SCHEMA_VERSION_VALUE)
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == SCHEMA_VERSION_VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "schema version mismatch: expected \"{SCHEMA_VERSION_VALUE}\", got \"{s}\""
            )))
        }
    }
}

/// Lifecycle status of a comment across review cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Stale,
    Orphaned,
}

impl Serialize for Status {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Pending => serializer.serialize_str("pending"),
            Self::Stale => serializer.serialize_str("stale"),
            Self::Orphaned => serializer.serialize_str("orphaned"),
        }
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "pending" => Ok(Self::Pending),
            "stale" => Ok(Self::Stale),
            "orphaned" => Ok(Self::Orphaned),
            other => Err(serde::de::Error::custom(format!(
                "unknown status \"{other}\", expected pending/stale/orphaned"
            ))),
        }
    }
}

/// Where a comment is attached: a specific diff line, a whole change, the
/// entire stack, or a specific line in the change description. Serialized to
/// the wire format with the `"scope"` discriminator.
///
/// Stack-scoped records carry the `revset_hash` so `_stack.jsonl` can host
/// comments from multiple stacks and be filtered per-session.
///
/// Description-scoped records live alongside line- and change-scoped records
/// in `<change-id>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    Line {
        change_id: ChangeId,
        location: LineAnchor,
    },
    Change {
        change_id: ChangeId,
    },
    Stack {
        revset_hash: RevsetHash,
    },
    Description {
        change_id: ChangeId,
        location: DescriptionAnchor,
    },
}

/// Format an `OffsetDateTime` as an RFC 3339 string, mapping any formatting
/// failure into `JjrError::Io`. Used as the canonical key for identifying
/// comments by `created_at` across save / update / delete paths.
pub(crate) fn format_rfc3339(t: OffsetDateTime) -> Result<String> {
    t.format(&Rfc3339).map_err(|e| JjrError::Io {
        source: std::io::Error::other(e),
    })
}

fn truncate_with_ellipsis(s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s;
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push_str(TRUNCATION_SUFFIX);
    truncated
}

fn truncate_body(s: String) -> String {
    truncate_with_ellipsis(s, BODY_MAX)
}

/// A reviewer comment at line, change, or stack scope.
///
/// The on-disk (wire) representation is flat JSON with `scope` as the
/// discriminant. See the `Serialize`/`Deserialize` impls on `CommentDto` for
/// the exact field layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub schema_version: SchemaVersion,
    pub anchor: Anchor,
    pub repo_root: PathBuf,
    pub revset: String,
    pub commit_id: Option<CommitId>,
    pub body: String,
    pub severity: Severity,
    pub created_at: OffsetDateTime,
    pub updated_at: Option<OffsetDateTime>,
    pub status: Option<Status>,
    pub mismatch_reason: Option<MismatchReason>,
    /// Semantic entity this comment belongs to; populated at comment-creation
    /// time when entity extraction has run.
    pub entity_id: Option<local_review_core::semantic::EntityId>,
    /// Three-point re-anchoring fingerprint; populated for line-scoped comments.
    pub anchor_fingerprint: Option<local_review_core::AnchorFingerprint>,
}

impl Serialize for Comment {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let dto = CommentDto::from_comment(self).map_err(serde::ser::Error::custom)?;
        dto.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Comment {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let dto = CommentDto::deserialize(deserializer)?;
        dto.into_comment().map_err(serde::de::Error::custom)
    }
}

/// Intermediate flat DTO matching the on-disk JSON layout.
///
/// `scope` defaults to `"line"` when absent (v1 backward compatibility).
#[derive(Debug, Serialize, Deserialize)]
struct CommentDto {
    schema_version: SchemaVersion,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_id: Option<ChangeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_id: Option<CommitId>,
    repo_root: String,
    revset: String,
    /// Hex-encoded `RevsetHash`; present only for `scope = "stack"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    revset_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    side: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hunk_header: Option<String>,
    /// 1-based display line for `scope = "description"`; absent for line/change/stack.
    #[serde(skip_serializing_if = "Option::is_none")]
    display_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_after: Option<Vec<String>>,
    #[serde(rename = "comment")]
    body: String,
    severity: Severity,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch_reason: Option<MismatchReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entity_id: Option<local_review_core::semantic::EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor_fingerprint: Option<local_review_core::AnchorFingerprint>,
}

fn default_scope() -> String {
    "line".to_owned()
}

impl CommentDto {
    fn from_comment(c: &Comment) -> Result<Self> {
        let created_at = format_rfc3339(c.created_at)?;
        let updated_at = c.updated_at.map(format_rfc3339).transpose()?;

        let repo_root = c
            .repo_root
            .to_str()
            .ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("repo_root is not valid UTF-8"),
            })?
            .to_owned();

        // Apply spec constraints at the write boundary so a Comment built by
        // direct struct literal can never serialize an oversized record.
        let normalized_anchor = normalize_anchor(c.anchor.clone());
        let fields = anchor_to_dto_fields(&normalized_anchor)?;

        Ok(Self {
            schema_version: SchemaVersion,
            scope: fields.scope,
            change_id: fields.change_id,
            commit_id: c.commit_id.clone(),
            repo_root,
            revset: c.revset.clone(),
            revset_hash: fields.revset_hash_hex,
            file: fields.line.as_ref().map(|f| f.file.clone()),
            side: fields.line.as_ref().map(|f| f.side),
            old_line: fields.line.as_ref().and_then(|f| f.old_line),
            new_line: fields.line.as_ref().and_then(|f| f.new_line),
            hunk_header: fields.line.as_ref().map(|f| f.hunk_header.clone()),
            display_line: fields.description.as_ref().and_then(|d| d.display_line),
            target_text: fields
                .line
                .as_ref()
                .map(|f| f.target_text.clone())
                .or_else(|| fields.description.as_ref().map(|d| d.target_text.clone())),
            context_before: fields
                .line
                .as_ref()
                .map(|f| f.context_before.clone())
                .or_else(|| {
                    fields
                        .description
                        .as_ref()
                        .map(|d| d.context_before.clone())
                }),
            context_after: fields
                .line
                .as_ref()
                .map(|f| f.context_after.clone())
                .or_else(|| fields.description.as_ref().map(|d| d.context_after.clone())),
            body: truncate_body(c.body.clone()),
            severity: c.severity,
            created_at,
            updated_at,
            status: c.status,
            mismatch_reason: c.mismatch_reason,
            entity_id: c.entity_id.clone(),
            anchor_fingerprint: c.anchor_fingerprint.clone(),
        })
    }

    fn into_comment(self) -> Result<Comment> {
        let created_at =
            OffsetDateTime::parse(&self.created_at, &Rfc3339).map_err(|e| JjrError::Io {
                source: std::io::Error::other(e),
            })?;
        let updated_at = self
            .updated_at
            .map(|s| OffsetDateTime::parse(&s, &Rfc3339))
            .transpose()
            .map_err(|e| JjrError::Io {
                source: std::io::Error::other(e),
            })?;

        let repo_root = PathBuf::from(&self.repo_root);
        let anchor = dto_fields_to_anchor(
            &self.scope,
            self.change_id,
            self.revset_hash,
            self.file,
            self.side,
            self.old_line,
            self.new_line,
            self.hunk_header,
            self.display_line,
            self.target_text,
            self.context_before,
            self.context_after,
        )?;
        // A hand-edited JSONL file must not be able to smuggle oversized
        // fields past the read boundary.
        let anchor = normalize_anchor(anchor);

        Ok(Comment {
            schema_version: SchemaVersion,
            anchor,
            repo_root,
            revset: self.revset,
            commit_id: self.commit_id,
            body: truncate_body(self.body),
            severity: self.severity,
            created_at,
            updated_at,
            status: self.status,
            mismatch_reason: self.mismatch_reason,
            entity_id: self.entity_id,
            anchor_fingerprint: self.anchor_fingerprint,
        })
    }
}

fn normalize_anchor(anchor: Anchor) -> Anchor {
    match anchor {
        Anchor::Line {
            change_id,
            location,
        } => Anchor::Line {
            change_id,
            location: location.normalized(),
        },
        Anchor::Description {
            change_id,
            location,
        } => Anchor::Description {
            change_id,
            location: location.normalized(),
        },
        a @ (Anchor::Change { .. } | Anchor::Stack { .. }) => a,
    }
}

struct LineFields {
    file: String,
    side: Side,
    old_line: Option<u32>,
    new_line: Option<u32>,
    hunk_header: String,
    target_text: String,
    context_before: Vec<String>,
    context_after: Vec<String>,
}

struct DescriptionFields {
    display_line: Option<u32>,
    target_text: String,
    context_before: Vec<String>,
    context_after: Vec<String>,
}

struct AnchorDtoFields {
    scope: String,
    change_id: Option<ChangeId>,
    revset_hash_hex: Option<String>,
    line: Option<LineFields>,
    description: Option<DescriptionFields>,
}

fn anchor_to_dto_fields(anchor: &Anchor) -> Result<AnchorDtoFields> {
    match anchor {
        Anchor::Line {
            change_id,
            location,
        } => {
            let file = location
                .file
                .to_str()
                .ok_or_else(|| JjrError::Io {
                    source: std::io::Error::other("file path is not valid UTF-8"),
                })?
                .to_owned();
            Ok(AnchorDtoFields {
                scope: "line".to_owned(),
                change_id: Some(change_id.clone()),
                revset_hash_hex: None,
                line: Some(LineFields {
                    file,
                    side: location.side,
                    old_line: location.old_line,
                    new_line: location.new_line,
                    hunk_header: location.hunk_header.clone(),
                    target_text: location.target_text.clone(),
                    context_before: location.context_before.clone(),
                    context_after: location.context_after.clone(),
                }),
                description: None,
            })
        }
        Anchor::Change { change_id } => Ok(AnchorDtoFields {
            scope: "change".to_owned(),
            change_id: Some(change_id.clone()),
            revset_hash_hex: None,
            line: None,
            description: None,
        }),
        Anchor::Stack { revset_hash } => Ok(AnchorDtoFields {
            scope: "stack".to_owned(),
            change_id: None,
            revset_hash_hex: Some(revset_hash.hex()),
            line: None,
            description: None,
        }),
        Anchor::Description {
            change_id,
            location,
        } => Ok(AnchorDtoFields {
            scope: "description".to_owned(),
            change_id: Some(change_id.clone()),
            revset_hash_hex: None,
            line: None,
            description: Some(DescriptionFields {
                display_line: location.display_line,
                target_text: location.target_text.clone(),
                context_before: location.context_before.clone(),
                context_after: location.context_after.clone(),
            }),
        }),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "all fields needed to reconstruct the Anchor from flat DTO; no logical grouping reduces coupling"
)]
fn dto_fields_to_anchor(
    scope: &str,
    change_id: Option<ChangeId>,
    revset_hash_hex: Option<String>,
    file: Option<String>,
    side: Option<Side>,
    old_line: Option<u32>,
    new_line: Option<u32>,
    hunk_header: Option<String>,
    display_line: Option<u32>,
    target_text: Option<String>,
    context_before: Option<Vec<String>>,
    context_after: Option<Vec<String>>,
) -> Result<Anchor> {
    match scope {
        "line" => {
            let change_id = change_id.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=line requires change_id"),
            })?;
            let file = PathBuf::from(file.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=line requires file"),
            })?);
            let side = side.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=line requires side"),
            })?;
            let hunk_header = hunk_header.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=line requires hunk_header"),
            })?;
            let target_text = target_text.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=line requires target_text"),
            })?;
            // Spec lists both line numbers as optional (one side of a diff
            // never has the other). At least one must be present, or the
            // anchor refers to no line at all.
            if old_line.is_none() && new_line.is_none() {
                return Err(JjrError::LineAnchorMissingLineNumber);
            }
            Ok(Anchor::Line {
                change_id,
                location: LineAnchor {
                    file,
                    side,
                    old_line,
                    new_line,
                    hunk_header,
                    target_text,
                    context_before: context_before.unwrap_or_default(),
                    context_after: context_after.unwrap_or_default(),
                },
            })
        }
        "change" => {
            let change_id = change_id.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=change requires change_id"),
            })?;
            Ok(Anchor::Change { change_id })
        }
        "stack" => {
            let hex = revset_hash_hex.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=stack requires revset_hash"),
            })?;
            let revset_hash = RevsetHash::from_hex_str(&hex).ok_or_else(|| JjrError::Io {
                source: std::io::Error::other(format!(
                    "scope=stack: revset_hash is malformed (expected 64 hex chars, got {hex:?})"
                )),
            })?;
            Ok(Anchor::Stack { revset_hash })
        }
        "description" => {
            let change_id = change_id.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=description requires change_id"),
            })?;
            let target_text = target_text.ok_or_else(|| JjrError::Io {
                source: std::io::Error::other("scope=description requires target_text"),
            })?;
            Ok(Anchor::Description {
                change_id,
                location: DescriptionAnchor {
                    display_line,
                    target_text,
                    context_before: context_before.unwrap_or_default(),
                    context_after: context_after.unwrap_or_default(),
                },
            })
        }
        other => Err(JjrError::Io {
            source: std::io::Error::other(format!("unknown scope \"{other}\"")),
        }),
    }
}

/// Test-only fixture: a JSONL line whose `schema_version` field is wrong.
/// Shared across `comment.rs` and `store.rs` tests to keep wire-format
/// expectations in lockstep.
#[cfg(test)]
pub(crate) const BAD_V1_FIXTURE: &str = r#"{"schema_version":"diff-comment/v1","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;

    use super::*;

    fn sample_change_id() -> ChangeId {
        ChangeId::parse("abc12345").unwrap()
    }

    fn sample_commit_id() -> CommitId {
        CommitId::parse("deadbeef").unwrap()
    }

    fn sample_line_anchor() -> LineAnchor {
        LineAnchor {
            file: PathBuf::from("src/foo.rs"),
            side: Side::New,
            old_line: None,
            new_line: Some(42),
            hunk_header: "@@ -40,7 +40,12 @@ impl Client".to_owned(),
            target_text: "let resp = self.inner.request(req).await?;".to_owned(),
            context_before: vec![
                "pub async fn send(...) {".to_owned(),
                "    let req = self.prepare(req)?;".to_owned(),
            ],
            context_after: vec!["    Ok(resp)".to_owned(), "}".to_owned()],
        }
    }

    fn sample_line_comment() -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: sample_change_id(),
                location: sample_line_anchor(),
            },
            repo_root: PathBuf::from("/workspace/project"),
            revset: "@".to_owned(),
            commit_id: Some(sample_commit_id()),
            body: "This bypasses the retry policy.".to_owned(),
            severity: Severity::Required,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    #[test]
    fn line_comment_roundtrips_through_serde() {
        let original = sample_line_comment();
        let json = serde_json::to_string(&original).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn change_comment_roundtrips_through_serde() {
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: sample_change_id(),
            },
            repo_root: PathBuf::from("/workspace/project"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "This change does too much.".to_owned(),
            severity: Severity::Suggestion,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, original);
    }

    fn sample_revset_hash() -> RevsetHash {
        RevsetHash::from_revset("main..@")
    }

    #[test]
    fn stack_comment_roundtrips_through_serde() {
        let original = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: sample_revset_hash(),
            },
            repo_root: PathBuf::from("/workspace/project"),
            revset: "main..@".to_owned(),
            commit_id: None,
            body: "Rename retry_wrapper to retry_policy throughout.".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn stack_comment_wire_shape_includes_revset_hash() {
        let hash = sample_revset_hash();
        let c = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack { revset_hash: hash },
            repo_root: PathBuf::from("/w"),
            revset: "main..@".to_owned(),
            commit_id: None,
            body: "stack comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["scope"], "stack");
        assert_eq!(v["revset_hash"], hash.hex());
        assert!(v.get("change_id").is_none());
        assert!(v.get("file").is_none());
    }

    #[test]
    fn stack_scope_missing_revset_hash_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"stack","repo_root":"/w","revset":"@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=stack requires revset_hash"),
            "got: {err}"
        );
    }

    #[test]
    fn stack_scope_malformed_revset_hash_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"stack","revset_hash":"notvalid","repo_root":"/w","revset":"@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("revset_hash is malformed"), "got: {err}");
    }

    #[test]
    fn v1_record_without_scope_deserializes_as_line() {
        let v1_json = r#"{
            "schema_version": "diff-comment/v2",
            "change_id": "abc12345",
            "commit_id": "deadbeef",
            "repo_root": "/workspace/project",
            "revset": "@",
            "file": "src/foo.rs",
            "side": "new",
            "old_line": null,
            "new_line": 42,
            "hunk_header": "@@ -40,7 +40,12 @@ impl Client",
            "target_text": "let resp = self.inner.request(req).await?;",
            "context_before": ["context line"],
            "context_after": [],
            "comment": "Note body",
            "severity": "note",
            "created_at": "2026-04-29T14:22:01Z",
            "status": "pending"
        }"#;
        let c: Comment = serde_json::from_str(v1_json).unwrap();
        assert!(matches!(c.anchor, Anchor::Line { .. }));
    }

    #[test]
    fn schema_version_mismatch_returns_error() {
        let result = serde_json::from_str::<Comment>(BAD_V1_FIXTURE);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("schema version mismatch"), "got: {msg}");
    }

    #[test]
    fn severity_wire_format_roundtrips() {
        for (sev, wire) in [
            (Severity::Note, "\"note\""),
            (Severity::Suggestion, "\"suggestion\""),
            (Severity::Required, "\"required\""),
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            assert_eq!(json, wire);
            let parsed: Severity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, sev);
        }
    }

    #[test]
    fn status_wire_format_roundtrips() {
        for (st, wire) in [
            (Status::Pending, "\"pending\""),
            (Status::Stale, "\"stale\""),
            (Status::Orphaned, "\"orphaned\""),
        ] {
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, wire);
            let parsed: Status = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, st);
        }
    }

    #[test]
    fn line_comment_wire_shape_has_expected_fields() {
        let c = sample_line_comment();
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["schema_version"], "diff-comment/v2");
        assert_eq!(v["scope"], "line");
        assert_eq!(v["change_id"], "abc12345");
        assert_eq!(v["file"], "src/foo.rs");
        assert_eq!(v["side"], "new");
        assert_eq!(v["new_line"], 42);
        assert_eq!(v["severity"], "required");
        assert_eq!(v["status"], "pending");
        assert!(v.get("mismatch_reason").is_none() || v["mismatch_reason"].is_null());
    }

    #[test]
    fn change_comment_wire_shape_omits_line_fields() {
        let c = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Change {
                change_id: sample_change_id(),
            },
            repo_root: PathBuf::from("/w"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "change comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["scope"], "change");
        assert_eq!(v["change_id"], "abc12345");
        assert!(v.get("file").is_none());
        assert!(v.get("side").is_none());
        assert!(v.get("hunk_header").is_none());
    }

    #[test]
    fn stack_comment_wire_shape_omits_change_and_line_fields() {
        let c = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Stack {
                revset_hash: sample_revset_hash(),
            },
            repo_root: PathBuf::from("/w"),
            revset: "main..@".to_owned(),
            commit_id: None,
            body: "stack comment".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["scope"], "stack");
        assert!(v.get("change_id").is_none());
        assert!(v.get("file").is_none());
        assert!(v.get("commit_id").is_none());
    }

    #[test]
    fn unicode_in_body_target_text_and_path_roundtrips() {
        let comment = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Line {
                change_id: sample_change_id(),
                location: LineAnchor {
                    file: PathBuf::from("src/café/módulo.rs"),
                    side: Side::New,
                    old_line: None,
                    new_line: Some(1),
                    hunk_header: "@@ -1 +1 @@".to_owned(),
                    target_text: "let π = 3.14; // 中文 🎉".to_owned(),
                    context_before: vec!["// ñoño".to_owned()],
                    context_after: vec!["✨".to_owned()],
                },
            },
            repo_root: PathBuf::from("/w"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "Comment with emoji 🦀 and ümlaut".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let json = serde_json::to_string(&comment).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, comment);
    }

    #[test]
    fn wire_format_uses_comment_field_not_body() {
        let c = sample_line_comment();
        let json = serde_json::to_string(&c).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["comment"], "This bypasses the retry policy.");
        assert!(
            parsed.get("body").is_none(),
            "wire format must not emit a `body` field; spec says `comment`"
        );
    }

    #[test]
    fn oversized_body_is_truncated_at_serialize_time() {
        let huge_body = "x".repeat(BODY_MAX + 5000);
        let mut c = sample_line_comment();
        c.body = huge_body;
        let json = serde_json::to_string(&c).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let body_str = parsed["comment"].as_str().unwrap();
        assert_eq!(body_str.chars().count(), BODY_MAX + 1);
        assert!(body_str.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn oversized_body_is_truncated_at_deserialize_time() {
        let huge = "x".repeat(BODY_MAX + 5000);
        let hash = RevsetHash::from_revset("@").hex();
        let json = format!(
            r#"{{"schema_version":"diff-comment/v2","scope":"stack","revset_hash":"{hash}","repo_root":"/w","revset":"@","comment":"{huge}","severity":"note","created_at":"2026-04-29T14:22:01Z"}}"#
        );
        let c: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(c.body.chars().count(), BODY_MAX + 1);
        assert!(c.body.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn oversized_target_text_in_jsonl_is_normalized_on_read() {
        let huge = "y".repeat(TARGET_TEXT_MAX + 500);
        let json = format!(
            r#"{{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","target_text":"{huge}","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}}"#
        );
        let c: Comment = serde_json::from_str(&json).unwrap();
        let Anchor::Line { location, .. } = c.anchor else {
            panic!("expected Line anchor");
        };
        assert_eq!(location.target_text.chars().count(), TARGET_TEXT_MAX + 1);
        assert!(location.target_text.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn oversized_context_in_jsonl_is_capped_on_read() {
        let lines: Vec<String> = (0..50).map(|i| format!("\"l{i}\"")).collect();
        let context = format!("[{}]", lines.join(","));
        let json = format!(
            r#"{{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","target_text":"x","context_before":{context},"comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}}"#
        );
        let c: Comment = serde_json::from_str(&json).unwrap();
        let Anchor::Line { location, .. } = c.anchor else {
            panic!("expected Line anchor");
        };
        assert_eq!(location.context_before.len(), CONTEXT_MAX);
    }

    #[test]
    fn line_scope_with_neither_line_number_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let result = serde_json::from_str::<Comment>(json);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("at least one of old_line or new_line"),
            "got: {msg}"
        );
    }

    #[test]
    fn line_scope_with_only_old_line_succeeds() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"old","old_line":5,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let c: Comment = serde_json::from_str(json).unwrap();
        assert!(matches!(c.anchor, Anchor::Line { .. }));
    }

    #[test]
    fn line_scope_missing_change_id_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope=line requires change_id"), "got: {err}");
    }

    #[test]
    fn line_scope_missing_file_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","side":"new","new_line":1,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope=line requires file"), "got: {err}");
    }

    #[test]
    fn line_scope_missing_side_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","new_line":1,"hunk_header":"@@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope=line requires side"), "got: {err}");
    }

    #[test]
    fn line_scope_missing_hunk_header_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=line requires hunk_header"),
            "got: {err}"
        );
    }

    #[test]
    fn line_scope_missing_target_text_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"line","change_id":"abc12345","repo_root":"/w","revset":"@","file":"f.rs","side":"new","new_line":1,"hunk_header":"@@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=line requires target_text"),
            "got: {err}"
        );
    }

    #[test]
    fn change_scope_missing_change_id_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"change","repo_root":"/w","revset":"@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=change requires change_id"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_scope_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"galaxy","repo_root":"/w","revset":"@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown scope"), "got: {err}");
    }

    #[test]
    fn unknown_severity_errors() {
        let err = serde_json::from_str::<Severity>(r#""critical""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown severity"), "got: {err}");
    }

    #[test]
    fn unknown_status_errors() {
        let err = serde_json::from_str::<Status>(r#""approved""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown status"), "got: {err}");
    }

    fn sample_description_anchor() -> DescriptionAnchor {
        DescriptionAnchor {
            display_line: Some(4),
            target_text: "Fix the off-by-one error in loop bounds".to_owned(),
            context_before: vec![
                "Refactor retry logic".to_owned(),
                String::new(),
                "This change extracts the retry helper into its own module.".to_owned(),
            ],
            context_after: vec![String::new(), "Co-authored-by: Alice".to_owned()],
        }
    }

    fn sample_description_comment() -> Comment {
        Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: sample_change_id(),
                location: sample_description_anchor(),
            },
            repo_root: PathBuf::from("/workspace/project"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "This line describes the wrong fix.".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: Some(Status::Pending),
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        }
    }

    #[test]
    fn description_comment_roundtrips_through_serde() {
        let original = sample_description_comment();
        let json = serde_json::to_string(&original).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn description_comment_wire_shape() {
        let c = sample_description_comment();
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["scope"], "description");
        assert_eq!(v["change_id"], "abc12345");
        assert_eq!(v["display_line"], 4);
        assert_eq!(v["target_text"], "Fix the off-by-one error in loop bounds");
        assert!(v.get("file").is_none());
        assert!(v.get("side").is_none());
        assert!(v.get("hunk_header").is_none());
        assert!(v.get("revset_hash").is_none());
    }

    #[test]
    fn description_comment_wire_shape_byte_stable() {
        let c = sample_description_comment();
        let json = serde_json::to_string(&c).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let json2 =
            serde_json::to_string(&serde_json::from_str::<Comment>(&json).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::from_str::<serde_json::Value>(&json2).unwrap(),
            "round-trip must be byte-stable"
        );
        assert_eq!(v["scope"], "description");
    }

    #[test]
    fn description_scope_missing_change_id_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"description","repo_root":"/w","revset":"@","target_text":"x","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=description requires change_id"),
            "got: {err}"
        );
    }

    #[test]
    fn description_scope_missing_target_text_errors() {
        let json = r#"{"schema_version":"diff-comment/v2","scope":"description","change_id":"abc12345","repo_root":"/w","revset":"@","comment":"b","severity":"note","created_at":"2026-04-29T14:22:01Z"}"#;
        let err = serde_json::from_str::<Comment>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope=description requires target_text"),
            "got: {err}"
        );
    }

    #[test]
    fn description_anchor_display_line_zero_roundtrips() {
        let c = Comment {
            schema_version: SchemaVersion,
            anchor: Anchor::Description {
                change_id: sample_change_id(),
                location: DescriptionAnchor {
                    display_line: Some(0),
                    target_text: "first line".to_owned(),
                    context_before: vec![],
                    context_after: vec![],
                },
            },
            repo_root: PathBuf::from("/w"),
            revset: "@".to_owned(),
            commit_id: None,
            body: "b".to_owned(),
            severity: Severity::Note,
            created_at: datetime!(2026-04-29 14:22:01 UTC),
            updated_at: None,
            status: None,
            mismatch_reason: None,
            entity_id: None,
            anchor_fingerprint: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        let restored: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, c);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["display_line"], 0);
    }
}
