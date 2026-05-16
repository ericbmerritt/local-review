//! Local draft comment model and JSONL storage layer for `ggr`.
//!
//! Drafts live on disk under the XDG data directory until submitted to GitHub.
//! Storage layout per draft scope:
//!
//! - Line- and commit-scoped: `<data_home>/ggr/<host>/<owner>/<repo>/<pr>/drafts/<sha>.jsonl`
//! - PR-scoped: `<data_home>/ggr/<host>/<owner>/<repo>/<pr>/drafts/_pr.jsonl`
//!
//! All string fields sourced from external input are stripped of control
//! characters at construction to prevent ANSI/OSC injection.

use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{GgrError, Result};
use crate::pr::{valid_segment, CommitSha};
use local_review_core::comment::{Side, CONTEXT_MAX, TARGET_TEXT_MAX};
use local_review_core::util::{atomic_write_bytes, strip_controls};
use local_review_core::Severity;

const SCHEMA_VERSION: &str = "ggr-comment/v1";

/// Filename for PR-scoped drafts. Leading underscore avoids collision with
/// 40-hex commit SHAs, which never begin with `_`.
const PR_DRAFT_FILENAME: &str = "_pr.jsonl";

// ── anchor ────────────────────────────────────────────────────────────────────

/// Line-scoped anchor parameters. Passed to [`GgrDraft::new_line`].
///
/// Grouping into a struct avoids breaching the five-argument limit.
pub(crate) struct LineAnchorParams {
    pub(crate) commit_sha: CommitSha,
    pub(crate) file: String,
    pub(crate) side: Side,
    /// Line number on the old side; set when `side = Side::Old`.
    pub(crate) old_line: Option<u32>,
    /// Line number on the new side; set when `side = Side::New`.
    pub(crate) new_line: Option<u32>,
    /// Verbatim `@@ … @@` hunk header from the diff.
    pub(crate) hunk_header: String,
    /// Verbatim target text, max [`TARGET_TEXT_MAX`] chars.
    pub(crate) target_text: String,
    /// Up to [`CONTEXT_MAX`] lines before the target.
    pub(crate) context_before: Vec<String>,
    /// Up to [`CONTEXT_MAX`] lines after the target.
    pub(crate) context_after: Vec<String>,
}

/// Scope and anchor data for a draft comment.
///
/// The sum type makes illegal field combinations unrepresentable: a PR-scoped
/// draft cannot carry a `commit_sha`, a commit-scoped draft cannot carry a
/// `file`, and so on.
pub(crate) enum GgrAnchor {
    Line {
        commit_sha: CommitSha,
        file: String,
        side: Side,
        old_line: Option<u32>,
        new_line: Option<u32>,
        hunk_header: String,
        target_text: String,
        context_before: Vec<String>,
        context_after: Vec<String>,
    },
    Commit {
        commit_sha: CommitSha,
    },
    Pr,
}

// ── GgrDraft ──────────────────────────────────────────────────────────────────

/// A single local draft comment, parsed and validated.
///
/// Constructed via [`GgrDraft::new_line`], [`GgrDraft::new_commit`], or
/// [`GgrDraft::new_pr`]; deserialized via [`GgrDraft::from_wire`]. Construction
/// validates all invariants and strips control characters from external strings.
pub(crate) struct GgrDraft {
    /// GitHub hostname (`github.com` or a GHE hostname).
    pub(crate) host: String,
    pub(crate) owner: String,
    /// Repository name segment (without owner).
    pub(crate) repo: String,
    pub(crate) pr_number: u64,
    pub(crate) body: String,
    pub(crate) severity: Severity,
    /// RFC 3339 timestamp set at creation time.
    pub(crate) created_at: String,
    pub(crate) updated_at: Option<String>,
    pub(crate) anchor: GgrAnchor,
}

// ── construction ──────────────────────────────────────────────────────────────

pub(crate) struct CommonParams {
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) pr_number: u64,
    pub(crate) body: String,
    pub(crate) severity: Severity,
    pub(crate) created_at: String,
}

impl GgrDraft {
    pub(crate) fn new_line(common: &CommonParams, anchor: &LineAnchorParams) -> Result<Self> {
        validate_common(common)?;
        validate_line_anchor(anchor)?;
        Ok(Self {
            host: strip_controls(&common.host),
            owner: strip_controls(&common.owner),
            repo: strip_controls(&common.repo),
            pr_number: common.pr_number,
            body: strip_controls(&common.body),
            severity: common.severity,
            created_at: strip_controls(&common.created_at),
            updated_at: None,
            anchor: GgrAnchor::Line {
                commit_sha: anchor.commit_sha.clone(),
                file: strip_controls(&anchor.file),
                side: anchor.side,
                old_line: anchor.old_line,
                new_line: anchor.new_line,
                hunk_header: strip_controls(&anchor.hunk_header),
                target_text: strip_controls(&anchor.target_text),
                context_before: anchor
                    .context_before
                    .iter()
                    .map(|s| strip_controls(s))
                    .collect(),
                context_after: anchor
                    .context_after
                    .iter()
                    .map(|s| strip_controls(s))
                    .collect(),
            },
        })
    }

    pub(crate) fn new_commit(common: &CommonParams, commit_sha: &str) -> Result<Self> {
        validate_common(common)?;
        let sha = CommitSha::try_from(commit_sha).map_err(|e| GgrError::InvalidDraft {
            reason: e.to_string(),
        })?;
        Ok(Self {
            host: strip_controls(&common.host),
            owner: strip_controls(&common.owner),
            repo: strip_controls(&common.repo),
            pr_number: common.pr_number,
            body: strip_controls(&common.body),
            severity: common.severity,
            created_at: strip_controls(&common.created_at),
            updated_at: None,
            anchor: GgrAnchor::Commit { commit_sha: sha },
        })
    }

    pub(crate) fn new_pr(common: &CommonParams) -> Result<Self> {
        validate_common(common)?;
        Ok(Self {
            host: strip_controls(&common.host),
            owner: strip_controls(&common.owner),
            repo: strip_controls(&common.repo),
            pr_number: common.pr_number,
            body: strip_controls(&common.body),
            severity: common.severity,
            created_at: strip_controls(&common.created_at),
            updated_at: None,
            anchor: GgrAnchor::Pr,
        })
    }

    /// Convert a deserialized wire record into a validated [`GgrDraft`].
    ///
    /// Returns [`GgrError::InvalidDraft`] on any validation failure or
    /// unsupported field combination.
    fn from_wire(w: WireDraft) -> Result<Self> {
        if w.schema_version != SCHEMA_VERSION {
            return Err(GgrError::InvalidDraft {
                reason: format!(
                    "schema_version mismatch: expected {SCHEMA_VERSION:?}, found {:?}",
                    strip_controls(&w.schema_version),
                ),
            });
        }

        // Determine scope before consuming w, so routing does not cause a
        // partial-move error when we later build `common` from the same fields.
        let scope = w
            .scope
            .as_deref()
            .ok_or_else(|| GgrError::InvalidDraft {
                reason: "missing scope field".to_owned(),
            })?
            .to_owned();

        // Preserve updated_at before w is consumed by common/wire_anchor below.
        let updated_at = w.updated_at.as_deref().map(strip_controls);

        let common = CommonParams {
            host: w.host,
            owner: w.owner,
            repo: w.repo,
            pr_number: w.pr_number,
            body: w.body,
            severity: w.severity,
            created_at: w.created_at,
        };

        let wire_anchor = WireAnchorFields {
            commit_sha: w.commit_sha,
            file: w.file,
            side: w.side,
            old_line: w.old_line,
            new_line: w.new_line,
            hunk_header: w.hunk_header,
            target_text: w.target_text,
            context_before: w.context_before,
            context_after: w.context_after,
        };

        let mut draft = match scope.as_str() {
            "line" => {
                let anchor = line_anchor_from_wire(wire_anchor)?;
                Self::new_line(&common, &anchor)
            }
            "commit" => {
                let sha = wire_anchor
                    .commit_sha
                    .ok_or_else(|| GgrError::InvalidDraft {
                        reason: "commit-scoped draft missing commit_sha".to_owned(),
                    })?;
                Self::new_commit(&common, &sha)
            }
            "pr" => Self::new_pr(&common),
            other => Err(GgrError::InvalidDraft {
                reason: format!("unknown scope {:?}", strip_controls(other)),
            }),
        }?;
        draft.updated_at = updated_at;
        Ok(draft)
    }

    /// Serialize this draft to the flat JSONL wire format.
    fn to_wire(&self) -> WireDraft {
        match &self.anchor {
            GgrAnchor::Line {
                commit_sha,
                file,
                side,
                old_line,
                new_line,
                hunk_header,
                target_text,
                context_before,
                context_after,
            } => WireDraft {
                schema_version: SCHEMA_VERSION.to_owned(),
                scope: Some("line".to_owned()),
                host: self.host.clone(),
                owner: self.owner.clone(),
                repo: self.repo.clone(),
                pr_number: self.pr_number,
                body: self.body.clone(),
                severity: self.severity,
                created_at: self.created_at.clone(),
                updated_at: self.updated_at.clone(),
                commit_sha: Some(commit_sha.as_str().to_owned()),
                file: Some(file.clone()),
                side: Some(*side),
                old_line: *old_line,
                new_line: *new_line,
                hunk_header: Some(hunk_header.clone()),
                target_text: Some(target_text.clone()),
                context_before: Some(context_before.clone()),
                context_after: Some(context_after.clone()),
            },
            GgrAnchor::Commit { commit_sha } => WireDraft {
                schema_version: SCHEMA_VERSION.to_owned(),
                scope: Some("commit".to_owned()),
                host: self.host.clone(),
                owner: self.owner.clone(),
                repo: self.repo.clone(),
                pr_number: self.pr_number,
                body: self.body.clone(),
                severity: self.severity,
                created_at: self.created_at.clone(),
                updated_at: self.updated_at.clone(),
                commit_sha: Some(commit_sha.as_str().to_owned()),
                file: None,
                side: None,
                old_line: None,
                new_line: None,
                hunk_header: None,
                target_text: None,
                context_before: None,
                context_after: None,
            },
            GgrAnchor::Pr => WireDraft {
                schema_version: SCHEMA_VERSION.to_owned(),
                scope: Some("pr".to_owned()),
                host: self.host.clone(),
                owner: self.owner.clone(),
                repo: self.repo.clone(),
                pr_number: self.pr_number,
                body: self.body.clone(),
                severity: self.severity,
                created_at: self.created_at.clone(),
                updated_at: self.updated_at.clone(),
                commit_sha: None,
                file: None,
                side: None,
                old_line: None,
                new_line: None,
                hunk_header: None,
                target_text: None,
                context_before: None,
                context_after: None,
            },
        }
    }
}

// ── wire format ───────────────────────────────────────────────────────────────

/// Used internally to route wire anchor data to [`line_anchor_from_wire`]
/// without exceeding the five-argument function limit.
struct WireAnchorFields {
    commit_sha: Option<String>,
    file: Option<String>,
    side: Option<Side>,
    old_line: Option<u32>,
    new_line: Option<u32>,
    hunk_header: Option<String>,
    target_text: Option<String>,
    context_before: Option<Vec<String>>,
    context_after: Option<Vec<String>>,
}

fn line_anchor_from_wire(f: WireAnchorFields) -> Result<LineAnchorParams> {
    let sha_str = f.commit_sha.ok_or_else(|| GgrError::InvalidDraft {
        reason: "line-scoped draft missing commit_sha".to_owned(),
    })?;
    let commit_sha = CommitSha::try_from(sha_str.as_str()).map_err(|e| GgrError::InvalidDraft {
        reason: e.to_string(),
    })?;
    let file = f.file.ok_or_else(|| GgrError::InvalidDraft {
        reason: "line-scoped draft missing file".to_owned(),
    })?;
    let side = f.side.ok_or_else(|| GgrError::InvalidDraft {
        reason: "line-scoped draft missing side".to_owned(),
    })?;
    let hunk_header = f.hunk_header.ok_or_else(|| GgrError::InvalidDraft {
        reason: "line-scoped draft missing hunk_header".to_owned(),
    })?;
    Ok(LineAnchorParams {
        commit_sha,
        file,
        side,
        old_line: f.old_line,
        new_line: f.new_line,
        hunk_header,
        target_text: f.target_text.unwrap_or_default(),
        context_before: f.context_before.unwrap_or_default(),
        context_after: f.context_after.unwrap_or_default(),
    })
}

/// All anchor fields are `Option` to match the sparse JSONL format; validation
/// happens in [`GgrDraft::from_wire`] after deserialization, not here.
#[derive(Serialize, Deserialize)]
struct WireDraft {
    schema_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    host: String,
    owner: String,
    repo: String,
    pr_number: u64,
    body: String,
    severity: Severity,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    target_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_after: Option<Vec<String>>,
}

// ── validation ────────────────────────────────────────────────────────────────

fn validate_common(c: &CommonParams) -> Result<()> {
    if !valid_segment(&c.host) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid host {:?}", strip_controls(&c.host)),
        });
    }
    if !valid_segment(&c.owner) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid owner {:?}", strip_controls(&c.owner)),
        });
    }
    if !valid_segment(&c.repo) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid repo {:?}", strip_controls(&c.repo)),
        });
    }
    if c.body.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "body must not be empty".to_owned(),
        });
    }
    Ok(())
}

fn validate_line_anchor(a: &LineAnchorParams) -> Result<()> {
    if a.file.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "line-scoped draft: file must not be empty".to_owned(),
        });
    }

    match (a.side, a.old_line, a.new_line) {
        (Side::Old, Some(_), None) | (Side::New, None, Some(_)) => {}
        (Side::Old, None, _) => {
            return Err(GgrError::InvalidDraft {
                reason: "side=old requires old_line to be set".to_owned(),
            });
        }
        (Side::Old | Side::New, Some(_), Some(_)) => {
            return Err(GgrError::InvalidDraft {
                reason: "both old_line and new_line are set; set only one".to_owned(),
            });
        }
        (Side::New, _, None) => {
            return Err(GgrError::InvalidDraft {
                reason: "side=new requires new_line to be set".to_owned(),
            });
        }
    }

    if !a.hunk_header.starts_with("@@") {
        return Err(GgrError::InvalidDraft {
            reason: format!(
                "hunk_header must start with @@; got {:?}",
                strip_controls(&a.hunk_header),
            ),
        });
    }

    let target_len = a.target_text.chars().count();
    if target_len > TARGET_TEXT_MAX {
        return Err(GgrError::InvalidDraft {
            reason: format!("target_text is {target_len} chars; maximum is {TARGET_TEXT_MAX}"),
        });
    }

    if a.context_before.len() > CONTEXT_MAX {
        return Err(GgrError::InvalidDraft {
            reason: format!(
                "context_before has {} lines; maximum is {CONTEXT_MAX}",
                a.context_before.len(),
            ),
        });
    }

    if a.context_after.len() > CONTEXT_MAX {
        return Err(GgrError::InvalidDraft {
            reason: format!(
                "context_after has {} lines; maximum is {CONTEXT_MAX}",
                a.context_after.len(),
            ),
        });
    }

    Ok(())
}

// ── path construction ─────────────────────────────────────────────────────────

/// Resolve the drafts directory for a specific PR under `base`.
///
/// Extracted so tests can inject `base` without touching environment variables.
pub(crate) fn drafts_dir_from_base(
    base: &Path,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> PathBuf {
    crate::util::pr_data_dir(base, host, owner, repo, pr_number).join("drafts")
}

pub(crate) fn draft_path_from_base(base: &Path, draft: &GgrDraft) -> PathBuf {
    let dir = drafts_dir_from_base(
        base,
        &draft.host,
        &draft.owner,
        &draft.repo,
        draft.pr_number,
    );
    let filename = match &draft.anchor {
        GgrAnchor::Line { commit_sha, .. } | GgrAnchor::Commit { commit_sha } => {
            format!("{}.jsonl", commit_sha.as_str())
        }
        GgrAnchor::Pr => PR_DRAFT_FILENAME.to_owned(),
    };
    dir.join(filename)
}

// ── storage operations ────────────────────────────────────────────────────────

/// Append-mode write is safe: a crash at worst loses the new draft but cannot
/// corrupt lines already in the file.
pub(crate) fn append_draft(path: &Path, draft: &GgrDraft) -> Result<()> {
    let parent = path.parent().ok_or_else(|| GgrError::DraftIo {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "draft path has no parent directory",
        ),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| GgrError::DraftIo { source })?;

    let wire = draft.to_wire();
    let line = serde_json::to_string(&wire).map_err(|e| GgrError::DraftIo {
        source: std::io::Error::other(e),
    })?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| GgrError::DraftIo { source })?;

    writeln!(file, "{line}").map_err(|source| GgrError::DraftIo { source })?;
    Ok(())
}

/// Returns `Err` on any malformed line; schema-version mismatch returns
/// [`GgrError::InvalidDraft`], syntactic JSON errors return [`GgrError::DraftIo`].
pub(crate) fn list_drafts(path: &Path) -> Result<Vec<GgrDraft>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).map_err(|source| GgrError::DraftIo { source })?;
    let reader = std::io::BufReader::new(file);

    let mut drafts = Vec::new();
    for (idx, result) in reader.lines().enumerate() {
        let line = result.map_err(|source| GgrError::DraftIo { source })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let wire: WireDraft = serde_json::from_str(trimmed).map_err(|e| GgrError::DraftIo {
            source: std::io::Error::other(format!(
                "JSON parse error at line {} in {}: {e}",
                idx + 1,
                path.display()
            )),
        })?;
        drafts.push(GgrDraft::from_wire(wire)?);
    }
    Ok(drafts)
}

pub(crate) fn update_draft(
    path: &Path,
    created_at: &str,
    new_body: &str,
    new_severity: Severity,
    new_updated_at: &str,
) -> Result<bool> {
    if new_body.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "body must not be empty".to_owned(),
        });
    }
    let mut drafts = list_drafts(path)?;
    let pos = drafts.iter().position(|d| d.created_at == created_at);
    let Some(idx) = pos else {
        return Ok(false);
    };
    drafts[idx].body = strip_controls(new_body);
    drafts[idx].severity = new_severity;
    drafts[idx].updated_at = Some(strip_controls(new_updated_at));
    write_all(path, &drafts)?;
    Ok(true)
}

/// Rewrites `path` atomically, removing drafts that match `pred`.
pub(crate) fn delete_draft(path: &Path, pred: impl Fn(&GgrDraft) -> bool) -> Result<bool> {
    let drafts = list_drafts(path)?;
    let before = drafts.len();
    let kept: Vec<GgrDraft> = drafts.into_iter().filter(|d| !pred(d)).collect();
    if kept.len() == before {
        return Ok(false);
    }
    write_all(path, &kept)?;
    Ok(true)
}

/// Truncate `path` to empty content, preserving the file on disk.
///
/// Uses the same atomic-rename path as [`write_all`] so a crash mid-write
/// cannot corrupt a file that had valid content before the call.
pub(crate) fn clear_drafts(path: &Path) -> Result<()> {
    write_all(path, &[])
}

fn write_all(path: &Path, drafts: &[GgrDraft]) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    for draft in drafts {
        let line = serde_json::to_string(&draft.to_wire()).map_err(|e| GgrError::DraftIo {
            source: std::io::Error::other(e),
        })?;
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    atomic_write_bytes(path, &buf).map_err(|source| GgrError::DraftIo { source })
}

// ── GgrReply ──────────────────────────────────────────────────────────────────

/// Filename for reply drafts. Leading underscore avoids collision with commit
/// SHAs; distinct from `_pr.jsonl` so each file has a single purpose.
const REPLY_DRAFT_FILENAME: &str = "_replies.jsonl";

/// A pending reply to an existing GitHub review comment.
///
/// Constructed via [`GgrReply::new`]; deserialized via [`GgrReply::from_wire`].
/// All string fields from external input are stripped of control characters at
/// construction.
pub(crate) struct GgrReply {
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) pr_number: u64,
    /// GitHub review comment ID of the comment being replied to. Stored as a
    /// string because the spec treats it as an opaque identifier from the API.
    pub(crate) parent_comment_id: String,
    pub(crate) body: String,
    pub(crate) severity: Severity,
    pub(crate) created_at: String,
    pub(crate) updated_at: Option<String>,
}

/// Construction parameters for [`GgrReply::new`].
pub(crate) struct ReplyParams {
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) pr_number: u64,
    pub(crate) parent_comment_id: String,
    pub(crate) body: String,
    pub(crate) severity: Severity,
    pub(crate) created_at: String,
}

impl GgrReply {
    pub(crate) fn new(params: &ReplyParams) -> Result<Self> {
        validate_reply_params(params)?;
        Ok(Self {
            host: strip_controls(&params.host),
            owner: strip_controls(&params.owner),
            repo: strip_controls(&params.repo),
            pr_number: params.pr_number,
            parent_comment_id: strip_controls(&params.parent_comment_id),
            body: strip_controls(&params.body),
            severity: params.severity,
            created_at: strip_controls(&params.created_at),
            updated_at: None,
        })
    }

    fn from_wire(w: WireReply) -> Result<Self> {
        if w.schema_version != SCHEMA_VERSION {
            return Err(GgrError::InvalidDraft {
                reason: format!(
                    "schema_version mismatch: expected {SCHEMA_VERSION:?}, found {:?}",
                    strip_controls(&w.schema_version),
                ),
            });
        }
        let params = ReplyParams {
            host: w.host,
            owner: w.owner,
            repo: w.repo,
            pr_number: w.pr_number,
            parent_comment_id: w.parent_comment_id,
            body: w.body,
            severity: w.severity,
            created_at: w.created_at,
        };
        let mut reply = Self::new(&params)?;
        reply.updated_at = w.updated_at.as_deref().map(strip_controls);
        Ok(reply)
    }

    fn to_wire(&self) -> WireReply {
        WireReply {
            schema_version: SCHEMA_VERSION.to_owned(),
            kind: "reply".to_owned(),
            host: self.host.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            pr_number: self.pr_number,
            parent_comment_id: self.parent_comment_id.clone(),
            body: self.body.clone(),
            severity: self.severity,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WireReply {
    schema_version: String,
    kind: String,
    host: String,
    owner: String,
    repo: String,
    pr_number: u64,
    parent_comment_id: String,
    body: String,
    severity: Severity,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

fn validate_reply_params(p: &ReplyParams) -> Result<()> {
    if !valid_segment(&p.host) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid host {:?}", strip_controls(&p.host)),
        });
    }
    if !valid_segment(&p.owner) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid owner {:?}", strip_controls(&p.owner)),
        });
    }
    if !valid_segment(&p.repo) {
        return Err(GgrError::InvalidDraft {
            reason: format!("invalid repo {:?}", strip_controls(&p.repo)),
        });
    }
    if p.body.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "body must not be empty".to_owned(),
        });
    }
    if p.parent_comment_id.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "parent_comment_id must not be empty".to_owned(),
        });
    }
    Ok(())
}

// ── reply path construction ───────────────────────────────────────────────────

/// Path of the `_replies.jsonl` file for a PR under `base`.
pub(crate) fn replies_file_from_base(
    base: &Path,
    host: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> PathBuf {
    drafts_dir_from_base(base, host, owner, repo, pr_number).join(REPLY_DRAFT_FILENAME)
}

// ── reply storage operations ──────────────────────────────────────────────────

/// Append a single reply to `path` (`O_APPEND`; crash-safe).
pub(crate) fn append_reply(path: &Path, reply: &GgrReply) -> Result<()> {
    let parent = path.parent().ok_or_else(|| GgrError::DraftIo {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "reply path has no parent directory",
        ),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| GgrError::DraftIo { source })?;

    let line = serde_json::to_string(&reply.to_wire()).map_err(|e| GgrError::DraftIo {
        source: std::io::Error::other(e),
    })?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| GgrError::DraftIo { source })?;
    writeln!(file, "{line}").map_err(|source| GgrError::DraftIo { source })?;
    Ok(())
}

/// Read all replies from `path`. Returns empty vec if the file does not exist.
pub(crate) fn list_replies(path: &Path) -> Result<Vec<GgrReply>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).map_err(|source| GgrError::DraftIo { source })?;
    let reader = std::io::BufReader::new(file);
    let mut replies = Vec::new();
    for (idx, result) in reader.lines().enumerate() {
        let line = result.map_err(|source| GgrError::DraftIo { source })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let wire: WireReply = serde_json::from_str(trimmed).map_err(|e| GgrError::DraftIo {
            source: std::io::Error::other(format!(
                "JSON parse error at line {} in {}: {e}",
                idx + 1,
                path.display()
            )),
        })?;
        replies.push(GgrReply::from_wire(wire)?);
    }
    Ok(replies)
}

/// Atomically rewrite `path`, updating the reply identified by `created_at`.
pub(crate) fn update_reply(
    path: &Path,
    created_at: &str,
    new_body: &str,
    new_severity: Severity,
    new_updated_at: &str,
) -> Result<bool> {
    if new_body.is_empty() {
        return Err(GgrError::InvalidDraft {
            reason: "body must not be empty".to_owned(),
        });
    }
    let mut replies = list_replies(path)?;
    let pos = replies.iter().position(|r| r.created_at == created_at);
    let Some(idx) = pos else {
        return Ok(false);
    };
    replies[idx].body = strip_controls(new_body);
    replies[idx].severity = new_severity;
    replies[idx].updated_at = Some(strip_controls(new_updated_at));
    write_all_replies(path, &replies)?;
    Ok(true)
}

/// Atomically rewrite `path`, removing replies that match `pred`.
pub(crate) fn delete_reply(path: &Path, pred: impl Fn(&GgrReply) -> bool) -> Result<bool> {
    let replies = list_replies(path)?;
    let before = replies.len();
    let kept: Vec<GgrReply> = replies.into_iter().filter(|r| !pred(r)).collect();
    if kept.len() == before {
        return Ok(false);
    }
    write_all_replies(path, &kept)?;
    Ok(true)
}

/// Truncate `path` to empty, preserving the file on disk.
pub(crate) fn clear_replies(path: &Path) -> Result<()> {
    write_all_replies(path, &[])
}

fn write_all_replies(path: &Path, replies: &[GgrReply]) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    for reply in replies {
        let line = serde_json::to_string(&reply.to_wire()).map_err(|e| GgrError::DraftIo {
            source: std::io::Error::other(e),
        })?;
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    atomic_write_bytes(path, &buf).map_err(|source| GgrError::DraftIo { source })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn common(body: &str) -> CommonParams {
        CommonParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            body: body.to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn line_anchor() -> LineAnchorParams {
        LineAnchorParams {
            commit_sha: CommitSha::try_from("a".repeat(40).as_str()).expect("valid sha"),
            file: "src/lib.rs".to_owned(),
            side: Side::New,
            old_line: None,
            new_line: Some(10),
            hunk_header: "@@ -1,3 +1,4 @@".to_owned(),
            target_text: "fn foo() {}".to_owned(),
            context_before: vec!["use std::io;".to_owned()],
            context_after: vec!["fn bar() {}".to_owned()],
        }
    }

    fn unique_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ggr_draft_test_{}_{}.jsonl",
            tag,
            std::process::id()
        ))
    }

    // ── construction: happy paths ─────────────────────────────────────────────

    #[test]
    fn new_line_draft_constructs_ok() {
        let draft = GgrDraft::new_line(&common("fix null check"), &line_anchor()).unwrap();
        assert_eq!(draft.host, "github.com");
        assert_eq!(draft.pr_number, 42);
        assert!(matches!(draft.anchor, GgrAnchor::Line { .. }));
    }

    #[test]
    fn new_commit_draft_constructs_ok() {
        let sha = "b".repeat(40);
        let draft = GgrDraft::new_commit(&common("split this commit"), &sha).unwrap();
        assert_eq!(draft.body, "split this commit");
        match draft.anchor {
            GgrAnchor::Commit { commit_sha } => assert_eq!(commit_sha.as_str(), sha),
            GgrAnchor::Line { .. } | GgrAnchor::Pr => panic!("expected Commit anchor"),
        }
    }

    #[test]
    fn new_pr_draft_constructs_ok() {
        let draft = GgrDraft::new_pr(&common("rename throughout")).unwrap();
        assert!(matches!(draft.anchor, GgrAnchor::Pr));
    }

    // ── construction: line anchor validation ──────────────────────────────────

    #[test]
    fn line_draft_missing_commit_sha_errors() {
        // CommitSha enforces the 40-hex constraint at construction; empty string
        // is rejected before it can reach LineAnchorParams.
        assert!(CommitSha::try_from("").is_err());
    }

    #[test]
    fn line_draft_non_hex_commit_sha_errors() {
        // CommitSha enforces the 40-hex constraint at construction; non-hex
        // characters are rejected before they can reach LineAnchorParams.
        assert!(CommitSha::try_from("g".repeat(40).as_str()).is_err());
    }

    #[test]
    fn line_draft_both_old_and_new_line_errors() {
        let mut a = line_anchor();
        a.old_line = Some(5);
        a.new_line = Some(6);
        let result = GgrDraft::new_line(&common("body"), &a);
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("both"), "expected 'both' in error: {msg}");
    }

    #[test]
    fn line_draft_side_old_but_only_new_line_set_errors() {
        let mut a = line_anchor();
        a.side = Side::Old;
        a.old_line = None;
        a.new_line = Some(5);
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_side_new_but_only_old_line_set_errors() {
        let mut a = line_anchor();
        a.side = Side::New;
        a.old_line = Some(5);
        a.new_line = None;
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_target_text_over_1024_errors() {
        let mut a = line_anchor();
        a.target_text = "x".repeat(TARGET_TEXT_MAX + 1);
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_context_before_over_3_errors() {
        let mut a = line_anchor();
        a.context_before = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_context_after_over_3_errors() {
        let mut a = line_anchor();
        a.context_after = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_empty_body_errors() {
        assert!(GgrDraft::new_line(&common(""), &line_anchor()).is_err());
    }

    #[test]
    fn line_draft_empty_file_errors() {
        let mut a = line_anchor();
        a.file = String::new();
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    #[test]
    fn line_draft_hunk_header_not_starting_with_at_errors() {
        let mut a = line_anchor();
        a.hunk_header = "diff --git a/src/lib.rs".to_owned();
        assert!(GgrDraft::new_line(&common("body"), &a).is_err());
    }

    // ── construction: commit SHA validation ───────────────────────────────────

    #[test]
    fn commit_draft_39_char_sha_errors() {
        let sha = "a".repeat(39);
        assert!(GgrDraft::new_commit(&common("body"), &sha).is_err());
    }

    #[test]
    fn commit_draft_41_char_sha_errors() {
        let sha = "a".repeat(41);
        assert!(GgrDraft::new_commit(&common("body"), &sha).is_err());
    }

    #[test]
    fn commit_draft_uppercase_hex_sha_errors() {
        let sha = "A".repeat(40);
        assert!(GgrDraft::new_commit(&common("body"), &sha).is_err());
    }

    // ── construction: common validation ──────────────────────────────────────

    #[test]
    fn pr_draft_invalid_host_errors() {
        let mut c = common("body");
        c.host = "../../etc".to_owned();
        assert!(GgrDraft::new_pr(&c).is_err());
    }

    #[test]
    fn pr_draft_invalid_owner_errors() {
        let mut c = common("body");
        c.owner = "bad/owner".to_owned();
        assert!(GgrDraft::new_pr(&c).is_err());
    }

    #[test]
    fn pr_draft_invalid_repo_errors() {
        let mut c = common("body");
        c.repo = "..".to_owned();
        assert!(GgrDraft::new_pr(&c).is_err());
    }

    // ── serialization roundtrip ───────────────────────────────────────────────

    #[test]
    fn line_draft_json_roundtrip() {
        let draft = GgrDraft::new_line(&common("fix null check"), &line_anchor()).unwrap();
        let wire = draft.to_wire();
        let json = serde_json::to_string(&wire).unwrap();
        let wire2: WireDraft = serde_json::from_str(&json).unwrap();
        let draft2 = GgrDraft::from_wire(wire2).unwrap();

        assert_eq!(draft2.body, draft.body);
        assert_eq!(draft2.host, draft.host);
        assert_eq!(draft2.pr_number, draft.pr_number);
        assert_eq!(draft2.created_at, draft.created_at);
        match (draft.anchor, draft2.anchor) {
            (
                GgrAnchor::Line {
                    commit_sha: sha1,
                    file: file1,
                    ..
                },
                GgrAnchor::Line {
                    commit_sha: sha2,
                    file: file2,
                    ..
                },
            ) => {
                assert_eq!(sha1.as_str(), sha2.as_str());
                assert_eq!(file1, file2);
            }
            _ => panic!("expected matching Line anchors"),
        }
    }

    #[test]
    fn commit_draft_json_roundtrip() {
        let sha = "c".repeat(40);
        let draft = GgrDraft::new_commit(&common("fix commit"), &sha).unwrap();
        let json = serde_json::to_string(&draft.to_wire()).unwrap();
        let wire: WireDraft = serde_json::from_str(&json).unwrap();
        let draft2 = GgrDraft::from_wire(wire).unwrap();
        assert_eq!(draft2.body, draft.body);
        assert!(matches!(draft2.anchor, GgrAnchor::Commit { .. }));
    }

    #[test]
    fn pr_draft_json_roundtrip() {
        let draft = GgrDraft::new_pr(&common("rename throughout")).unwrap();
        let json = serde_json::to_string(&draft.to_wire()).unwrap();
        let wire: WireDraft = serde_json::from_str(&json).unwrap();
        let draft2 = GgrDraft::from_wire(wire).unwrap();
        assert_eq!(draft2.body, draft.body);
        assert!(matches!(draft2.anchor, GgrAnchor::Pr));
    }

    #[test]
    fn wire_schema_version_written_as_expected() {
        let draft = GgrDraft::new_pr(&common("body")).unwrap();
        let json = serde_json::to_string(&draft.to_wire()).unwrap();
        assert!(
            json.contains(SCHEMA_VERSION),
            "expected schema_version in JSON: {json}"
        );
    }

    #[test]
    fn wrong_schema_version_on_deserialize_returns_invalid_draft() {
        let json = r#"{"schema_version":"ggr-comment/v0","scope":"pr","host":"github.com","owner":"acme","repo":"widget","pr_number":1,"body":"x","severity":"note","created_at":"2026-01-01T00:00:00Z"}"#;
        let wire: WireDraft = serde_json::from_str(json).unwrap();
        let result = GgrDraft::from_wire(wire);
        assert!(matches!(result, Err(GgrError::InvalidDraft { .. })));
    }

    // ── storage: append + list roundtrip ─────────────────────────────────────

    #[test]
    fn append_then_list_returns_all_drafts() {
        let path = unique_path("append_list");
        let d1 = GgrDraft::new_pr(&common("first")).unwrap();
        let mut c2 = common("second");
        c2.created_at = "2026-01-01T00:00:01Z".to_owned();
        let d2 = GgrDraft::new_pr(&c2).unwrap();
        let mut c3 = common("third");
        c3.created_at = "2026-01-01T00:00:02Z".to_owned();
        let d3 = GgrDraft::new_pr(&c3).unwrap();

        append_draft(&path, &d1).unwrap();
        append_draft(&path, &d2).unwrap();
        append_draft(&path, &d3).unwrap();

        let loaded = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].body, "first");
        assert_eq!(loaded[1].body, "second");
        assert_eq!(loaded[2].body, "third");
    }

    #[test]
    fn list_drafts_nonexistent_file_returns_empty() {
        let path = unique_path("nonexistent");
        let loaded = list_drafts(&path).unwrap();
        assert!(loaded.is_empty());
    }

    // ── storage: delete ───────────────────────────────────────────────────────

    #[test]
    fn delete_draft_removes_matching_preserves_others() {
        let path = unique_path("delete");
        let d1 = GgrDraft::new_pr(&common("keep")).unwrap();
        let mut c2 = common("remove");
        c2.created_at = "2026-01-01T00:00:01Z".to_owned();
        let d2 = GgrDraft::new_pr(&c2).unwrap();

        append_draft(&path, &d1).unwrap();
        append_draft(&path, &d2).unwrap();

        let removed = delete_draft(&path, |d| d.body == "remove").unwrap();
        let remaining = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(removed);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].body, "keep");
    }

    #[test]
    fn delete_draft_no_match_returns_false() {
        let path = unique_path("delete_no_match");
        let d = GgrDraft::new_pr(&common("keep")).unwrap();
        append_draft(&path, &d).unwrap();

        let removed = delete_draft(&path, |d| d.body == "missing").unwrap();
        let remaining = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!removed);
        assert_eq!(remaining.len(), 1);
    }

    // ── storage: update ───────────────────────────────────────────────────────

    #[test]
    fn update_draft_changes_body_and_severity() {
        let path = unique_path("update");
        let draft = GgrDraft::new_pr(&common("original")).unwrap();
        let ts = draft.created_at.clone();
        append_draft(&path, &draft).unwrap();

        let found = update_draft(
            &path,
            &ts,
            "updated body",
            Severity::Required,
            "2024-01-15T10:31:00Z",
        )
        .unwrap();
        let loaded = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(found);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "updated body");
        assert_eq!(loaded[0].severity, Severity::Required);
        assert_eq!(loaded[0].created_at, ts);
    }

    #[test]
    fn update_draft_no_match_returns_false() {
        let path = unique_path("update_no_match");
        let draft = GgrDraft::new_pr(&common("body")).unwrap();
        append_draft(&path, &draft).unwrap();

        let found = update_draft(
            &path,
            "1999-01-01T00:00:00Z",
            "new body",
            Severity::Note,
            "2024-01-15T10:31:00Z",
        )
        .unwrap();
        let remaining = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!found);
        assert_eq!(remaining[0].body, "body");
    }

    // ── control character stripping ───────────────────────────────────────────

    #[test]
    fn control_characters_stripped_from_body() {
        let mut c = common("placeholder");
        c.body = "\x1b[31mevil\x1b[0m".to_owned();
        let draft = GgrDraft::new_pr(&c).unwrap();
        assert!(
            !draft.body.chars().any(char::is_control),
            "control chars must be stripped from body: {:?}",
            draft.body
        );
        assert_eq!(draft.body, "[31mevil[0m");
    }

    #[test]
    fn control_characters_stripped_from_line_anchor_fields() {
        let mut a = line_anchor();
        a.target_text = "\x1b[32mgreen\x1b[0m".to_owned();
        a.hunk_header = "@@ -1,3 +1,4 @@\x01".to_owned();
        let draft = GgrDraft::new_line(&common("body"), &a).unwrap();
        match draft.anchor {
            GgrAnchor::Line {
                target_text,
                hunk_header,
                ..
            } => {
                assert!(!target_text.chars().any(char::is_control));
                assert!(!hunk_header.chars().any(char::is_control));
            }
            GgrAnchor::Commit { .. } | GgrAnchor::Pr => panic!("expected Line anchor"),
        }
    }

    // ── path construction ─────────────────────────────────────────────────────

    #[test]
    fn draft_path_for_line_scope_ends_with_sha_jsonl() {
        let base = PathBuf::from("/data");
        let sha = "d".repeat(40);
        let draft = GgrDraft::new_line(
            &CommonParams {
                host: "github.com".to_owned(),
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                pr_number: 7,
                body: "body".to_owned(),
                severity: Severity::Note,
                created_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            &LineAnchorParams {
                commit_sha: CommitSha::try_from(sha.as_str()).expect("valid sha"),
                ..line_anchor()
            },
        )
        .unwrap();

        let path = draft_path_from_base(&base, &draft);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with(&format!("{sha}.jsonl")),
            "expected path ending with sha.jsonl: {path_str}"
        );
        assert!(
            path_str.contains("github.com/owner/repo/7/drafts"),
            "expected host/owner/repo/pr/drafts in path: {path_str}"
        );
    }

    #[test]
    fn draft_path_for_pr_scope_ends_with_pr_jsonl() {
        let base = PathBuf::from("/data");
        let draft = GgrDraft::new_pr(&common("pr comment")).unwrap();
        let path = draft_path_from_base(&base, &draft);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with(PR_DRAFT_FILENAME),
            "expected path ending with _pr.jsonl: {path_str}"
        );
    }

    #[test]
    fn draft_path_for_commit_scope_ends_with_sha_jsonl() {
        let base = PathBuf::from("/data");
        let sha = "e".repeat(40);
        let draft = GgrDraft::new_commit(&common("commit note"), &sha).unwrap();
        let path = draft_path_from_base(&base, &draft);
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with(&format!("{sha}.jsonl")),
            "expected path ending with sha.jsonl: {path_str}"
        );
    }

    #[test]
    fn drafts_dir_invalid_hostname_falls_back_to_github_com() {
        let base = PathBuf::from("/data");
        let dir = drafts_dir_from_base(&base, "../../etc", "owner", "repo", 1);
        let dir_str = dir.to_string_lossy();
        assert!(
            !dir_str.contains(".."),
            "path must not contain '..' for crafted host: {dir_str}"
        );
        assert!(
            dir_str.contains("github.com"),
            "path must fall back to github.com: {dir_str}"
        );
    }

    // ── error variants ────────────────────────────────────────────────────────

    #[test]
    fn invalid_draft_error_displays_reason() {
        let err = GgrError::InvalidDraft {
            reason: "body must not be empty".to_owned(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("body must not be empty"), "got: {msg}");
    }

    #[test]
    fn draft_io_error_is_produced_on_bad_path() {
        // Point at a path whose parent is a regular file — create_dir_all fails.
        let tmp = std::env::temp_dir().join(format!("ggr_io_blocker_{}", std::process::id()));
        std::fs::write(&tmp, b"file not dir").unwrap();
        let bad_path = tmp.join("drafts").join("out.jsonl");
        let draft = GgrDraft::new_pr(&common("body")).unwrap();
        let result = append_draft(&bad_path, &draft);
        let _ = std::fs::remove_file(&tmp);
        assert!(
            matches!(result, Err(GgrError::DraftIo { .. })),
            "expected DraftIo error; got: {result:?}"
        );
    }

    // ── g3: absent scope field ────────────────────────────────────────────────

    #[test]
    fn missing_scope_in_wire_returns_invalid_draft() {
        // A JSONL record without a `scope` field must be rejected; absent scope
        // cannot be silently defaulted because the caller's intent is ambiguous.
        let json = r#"{"schema_version":"ggr-comment/v1","host":"github.com","owner":"acme","repo":"widget","pr_number":1,"body":"x","severity":"note","created_at":"2026-01-01T00:00:00Z"}"#;
        let wire: WireDraft = serde_json::from_str(json).unwrap();
        let result = GgrDraft::from_wire(wire);
        assert!(
            matches!(result, Err(GgrError::InvalidDraft { .. })),
            "expected InvalidDraft for missing scope"
        );
    }

    // ── w1: update_draft with distinct created_at timestamps ─────────────────

    #[test]
    fn update_draft_duplicate_created_at() {
        // `created_at` is the unique key for update_draft. When two drafts have
        // different timestamps, the targeted draft is updated while the other is
        // left unchanged.
        let path = unique_path("update_duplicate_ts");
        let d1 = GgrDraft::new_pr(&common("first")).unwrap();
        let ts1 = d1.created_at.clone();
        let mut c2 = common("second");
        c2.created_at = "2026-01-01T00:00:01Z".to_owned();
        let d2 = GgrDraft::new_pr(&c2).unwrap();
        let ts2 = d2.created_at.clone();

        append_draft(&path, &d1).unwrap();
        append_draft(&path, &d2).unwrap();

        let found = update_draft(
            &path,
            &ts2,
            "second-updated",
            Severity::Required,
            "2024-01-15T10:31:00Z",
        )
        .unwrap();
        let loaded = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(
            found,
            "update_draft must return true when the record is found"
        );
        assert_eq!(loaded.len(), 2);
        // First draft is unchanged.
        assert_eq!(loaded[0].body, "first");
        assert_eq!(loaded[0].created_at, ts1);
        // Second draft is updated.
        assert_eq!(loaded[1].body, "second-updated");
        assert_eq!(loaded[1].severity, Severity::Required);
        assert_eq!(loaded[1].created_at, ts2);
    }

    // ── w2: list_drafts with a malformed JSON line ────────────────────────────

    #[test]
    fn list_drafts_malformed_json_line() {
        // A malformed JSON line causes list_drafts to return Err(DraftIo).
        // The file contains one valid record followed by one invalid line; the
        // invalid line is not skipped — it is a hard error.
        let path = unique_path("malformed_json");
        let draft = GgrDraft::new_pr(&common("valid")).unwrap();
        append_draft(&path, &draft).unwrap();
        // Append a malformed JSONL line directly.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{{invalid json}}").unwrap();
        drop(file);

        let result = list_drafts(&path);
        let _ = std::fs::remove_file(&path);

        assert!(
            matches!(result, Err(GgrError::DraftIo { .. })),
            "expected DraftIo error for malformed JSON line"
        );
    }

    // ── m1/w1: update_draft empty body ────────────────────────────────────────

    #[test]
    fn update_draft_empty_body_returns_error() {
        let path = unique_path("update_empty_body");
        let draft = GgrDraft::new_pr(&common("original")).unwrap();
        let ts = draft.created_at.clone();
        append_draft(&path, &draft).unwrap();

        let result = update_draft(&path, &ts, "", Severity::Note, "2024-01-15T10:31:00Z");
        let _ = std::fs::remove_file(&path);

        assert!(
            matches!(result, Err(GgrError::InvalidDraft { .. })),
            "expected InvalidDraft for empty body"
        );
    }

    // ── p2/m2: update_draft sets updated_at ──────────────────────────────────

    #[test]
    fn update_draft_sets_updated_at() {
        let path = unique_path("update_sets_updated_at");
        let draft = GgrDraft::new_pr(&common("original")).unwrap();
        let ts = draft.created_at.clone();
        append_draft(&path, &draft).unwrap();

        update_draft(
            &path,
            &ts,
            "new body",
            Severity::Note,
            "2024-01-15T10:31:00Z",
        )
        .unwrap();
        let loaded = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            loaded[0].updated_at.as_deref(),
            Some("2024-01-15T10:31:00Z"),
            "updated_at must be set after update_draft"
        );
    }

    // ── p1: missing hunk_header in wire ──────────────────────────────────────

    #[test]
    fn missing_hunk_header_in_wire_returns_invalid_draft() {
        // A line-scoped record with no hunk_header field must be rejected at
        // the parse boundary rather than silently defaulting to empty string,
        // which would fail hunk_header validation anyway but with a misleading
        // error message.
        let json = r#"{"schema_version":"ggr-comment/v1","scope":"line","host":"github.com","owner":"acme","repo":"widget","pr_number":1,"body":"x","severity":"note","created_at":"2026-01-01T00:00:00Z","commit_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","file":"src/lib.rs","side":"new","new_line":1}"#;
        let wire: WireDraft = serde_json::from_str(json).unwrap();
        let result = GgrDraft::from_wire(wire);
        assert!(
            matches!(result, Err(GgrError::InvalidDraft { .. })),
            "expected InvalidDraft for missing hunk_header"
        );
    }

    // ── w2 (integration): schema-version mismatch ────────────────────────────

    #[test]
    fn list_drafts_schema_version_mismatch_in_file() {
        let path = unique_path("schema_mismatch");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"schema_version":"ggr-comment/v0","scope":"pr","host":"github.com","owner":"a","repo":"b","pr_number":1,"body":"x","severity":"note","created_at":"2024-01-15T10:30:00Z"}}"#
        )
        .unwrap();
        drop(file);

        let result = list_drafts(&path);
        let _ = std::fs::remove_file(&path);

        assert!(
            matches!(result, Err(GgrError::InvalidDraft { .. })),
            "expected InvalidDraft for schema-version mismatch"
        );
    }

    // ── w3: target_text exactly at limit ─────────────────────────────────────

    #[test]
    fn new_line_target_text_exactly_at_limit_ok() {
        let mut a = line_anchor();
        a.target_text = "x".repeat(TARGET_TEXT_MAX);
        assert!(
            GgrDraft::new_line(&common("body"), &a).is_ok(),
            "exactly TARGET_TEXT_MAX chars must be accepted"
        );
    }

    // ── w4: context exactly at limit ─────────────────────────────────────────

    #[test]
    fn new_line_context_before_exactly_at_limit_ok() {
        let mut a = line_anchor();
        a.context_before = vec!["l".to_owned(); CONTEXT_MAX];
        assert!(
            GgrDraft::new_line(&common("body"), &a).is_ok(),
            "exactly CONTEXT_MAX context_before lines must be accepted"
        );
    }

    #[test]
    fn new_line_context_after_exactly_at_limit_ok() {
        let mut a = line_anchor();
        a.context_after = vec!["l".to_owned(); CONTEXT_MAX];
        assert!(
            GgrDraft::new_line(&common("body"), &a).is_ok(),
            "exactly CONTEXT_MAX context_after lines must be accepted"
        );
    }

    // ── w1: from_wire unknown scope ───────────────────────────────────────────

    #[test]
    fn from_wire_rejects_unknown_scope() {
        let path = unique_path("from_wire_unknown_scope");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"schema_version":"ggr-comment/v1","scope":"unknown_value","host":"github.com","owner":"acme","repo":"widget","pr_number":1,"body":"x","severity":"note","created_at":"2026-01-01T00:00:00Z"}}"#
        )
        .unwrap();
        drop(file);

        let result = list_drafts(&path);
        let _ = std::fs::remove_file(&path);

        match result {
            Err(GgrError::InvalidDraft { reason }) => {
                assert!(
                    reason.contains("unknown scope"),
                    "expected 'unknown scope' in reason: {reason}"
                );
            }
            Err(e) => panic!("expected InvalidDraft; got different error: {e}"),
            Ok(_) => panic!("expected InvalidDraft; got Ok"),
        }
    }

    // ── w2: from_wire commit-scope missing commit_sha ─────────────────────────

    #[test]
    fn from_wire_rejects_commit_scope_missing_sha() {
        let path = unique_path("from_wire_commit_no_sha");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"schema_version":"ggr-comment/v1","scope":"commit","host":"github.com","owner":"acme","repo":"widget","pr_number":1,"body":"x","severity":"note","created_at":"2026-01-01T00:00:00Z"}}"#
        )
        .unwrap();
        drop(file);

        let result = list_drafts(&path);
        let _ = std::fs::remove_file(&path);

        match result {
            Err(GgrError::InvalidDraft { reason }) => {
                assert!(
                    reason.contains("commit-scoped draft missing commit_sha"),
                    "expected commit_sha error in reason: {reason}"
                );
            }
            Err(e) => panic!("expected InvalidDraft; got different error: {e}"),
            Ok(_) => panic!("expected InvalidDraft; got Ok"),
        }
    }

    // ── w3: validate_line_anchor Side::Old happy path ─────────────────────────

    #[test]
    fn new_line_side_old_with_old_line_ok() {
        let mut a = line_anchor();
        a.side = Side::Old;
        a.old_line = Some(5);
        a.new_line = None;
        assert!(
            GgrDraft::new_line(&common("body"), &a).is_ok(),
            "side=Old with old_line=Some(5) and new_line=None must be accepted"
        );
    }

    // ── w4: update_draft strips control chars from new_body ───────────────────

    #[test]
    fn update_draft_strips_controls_from_new_body() {
        let path = unique_path("update_strip_controls");
        let draft = GgrDraft::new_pr(&common("original")).unwrap();
        let ts = draft.created_at.clone();
        append_draft(&path, &draft).unwrap();

        update_draft(
            &path,
            &ts,
            "\x1b[31mevil\x1b[0m",
            Severity::Note,
            "2024-01-15T10:31:00Z",
        )
        .unwrap();
        let loaded = list_drafts(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.len(), 1);
        assert!(
            !loaded[0].body.chars().any(char::is_control),
            "control chars must be stripped from new_body: {:?}",
            loaded[0].body
        );
    }

    // ── GgrReply tests ────────────────────────────────────────────────────────

    fn reply_params(body: &str) -> ReplyParams {
        ReplyParams {
            host: "github.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "widget".to_owned(),
            pr_number: 42,
            parent_comment_id: "123456789".to_owned(),
            body: body.to_owned(),
            severity: Severity::Note,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn unique_reply_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ggr_reply_test_{}_{}.jsonl",
            tag,
            std::process::id()
        ))
    }

    #[test]
    fn new_reply_constructs_ok() {
        let reply = GgrReply::new(&reply_params("looks good")).unwrap();
        assert_eq!(reply.host, "github.com");
        assert_eq!(reply.parent_comment_id, "123456789");
        assert_eq!(reply.body, "looks good");
        assert_eq!(reply.severity, Severity::Note);
        assert!(reply.updated_at.is_none());
    }

    #[test]
    fn new_reply_strips_controls_from_body() {
        let p = reply_params("\x1b[31mevil\x1b[0m");
        let reply = GgrReply::new(&p).unwrap();
        assert!(!reply.body.chars().any(char::is_control));
    }

    #[test]
    fn new_reply_strips_controls_from_parent_id() {
        let mut p = reply_params("ok");
        p.parent_comment_id = "123\x1b[31m".to_owned();
        let reply = GgrReply::new(&p).unwrap();
        assert!(!reply.parent_comment_id.chars().any(char::is_control));
    }

    #[test]
    fn new_reply_rejects_empty_body() {
        let p = reply_params("");
        assert!(GgrReply::new(&p).is_err());
    }

    #[test]
    fn new_reply_rejects_empty_parent_comment_id() {
        let mut p = reply_params("ok");
        p.parent_comment_id = String::new();
        assert!(GgrReply::new(&p).is_err());
    }

    #[test]
    fn new_reply_rejects_invalid_host() {
        let mut p = reply_params("ok");
        p.host = "../evil".to_owned();
        assert!(GgrReply::new(&p).is_err());
    }

    #[test]
    fn reply_wire_roundtrip() {
        let reply = GgrReply::new(&reply_params("roundtrip body")).unwrap();
        let wire = reply.to_wire();
        assert_eq!(wire.kind, "reply");
        assert_eq!(wire.schema_version, SCHEMA_VERSION);
        let restored = GgrReply::from_wire(wire).unwrap();
        assert_eq!(restored.body, "roundtrip body");
        assert_eq!(restored.parent_comment_id, "123456789");
    }

    #[test]
    fn reply_from_wire_rejects_wrong_schema_version() {
        let reply = GgrReply::new(&reply_params("ok")).unwrap();
        let mut wire = reply.to_wire();
        wire.schema_version = "wrong/v99".to_owned();
        assert!(GgrReply::from_wire(wire).is_err());
    }

    #[test]
    fn replies_file_from_base_has_correct_structure() {
        let base = PathBuf::from("/data");
        let path = replies_file_from_base(&base, "github.com", "acme", "widget", 42);
        assert_eq!(
            path,
            PathBuf::from("/data/ggr/github.com/acme/widget/42/drafts/_replies.jsonl")
        );
    }

    #[test]
    fn list_replies_returns_empty_for_nonexistent_file() {
        let path = unique_reply_path("nonexistent");
        let result = list_replies(&path).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn append_and_list_replies_roundtrip() {
        let path = unique_reply_path("append_list");
        let _ = std::fs::remove_file(&path);

        let reply = GgrReply::new(&reply_params("first")).unwrap();
        append_reply(&path, &reply).unwrap();

        let mut p2 = reply_params("second");
        p2.parent_comment_id = "987654321".to_owned();
        p2.severity = Severity::Required;
        let reply2 = GgrReply::new(&p2).unwrap();
        append_reply(&path, &reply2).unwrap();

        let loaded = list_replies(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].body, "first");
        assert_eq!(loaded[1].body, "second");
        assert_eq!(loaded[1].parent_comment_id, "987654321");
        assert_eq!(loaded[1].severity, Severity::Required);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_reply_changes_body_and_severity() {
        let path = unique_reply_path("update");
        let _ = std::fs::remove_file(&path);

        let reply = GgrReply::new(&reply_params("original")).unwrap();
        let created_at = reply.created_at.clone();
        append_reply(&path, &reply).unwrap();

        let updated = update_reply(
            &path,
            &created_at,
            "updated body",
            Severity::Required,
            "2026-02-01T00:00:00Z",
        )
        .unwrap();
        assert!(updated);

        let loaded = list_replies(&path).unwrap();
        assert_eq!(loaded[0].body, "updated body");
        assert_eq!(loaded[0].severity, Severity::Required);
        assert_eq!(
            loaded[0].updated_at.as_deref(),
            Some("2026-02-01T00:00:00Z")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_reply_returns_false_when_not_found() {
        let path = unique_reply_path("update_notfound");
        let _ = std::fs::remove_file(&path);

        let reply = GgrReply::new(&reply_params("body")).unwrap();
        append_reply(&path, &reply).unwrap();

        let updated = update_reply(
            &path,
            "1999-01-01T00:00:00Z",
            "new body",
            Severity::Note,
            "2026-02-01T00:00:00Z",
        )
        .unwrap();
        assert!(!updated);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_reply_rejects_empty_body() {
        let path = unique_reply_path("update_empty");
        let reply = GgrReply::new(&reply_params("body")).unwrap();
        let created_at = reply.created_at.clone();
        append_reply(&path, &reply).unwrap();

        assert!(update_reply(
            &path,
            &created_at,
            "",
            Severity::Note,
            "2026-01-01T00:00:00Z"
        )
        .is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_reply_removes_matching_entry() {
        let path = unique_reply_path("delete");
        let _ = std::fs::remove_file(&path);

        let r1 = GgrReply::new(&reply_params("keep")).unwrap();
        let mut p2 = reply_params("remove");
        p2.parent_comment_id = "999".to_owned();
        p2.created_at = "2026-06-01T00:00:00Z".to_owned();
        let r2 = GgrReply::new(&p2).unwrap();
        append_reply(&path, &r1).unwrap();
        append_reply(&path, &r2).unwrap();

        let deleted = delete_reply(&path, |r| r.parent_comment_id == "999").unwrap();
        assert!(deleted);

        let remaining = list_replies(&path).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].body, "keep");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_reply_returns_false_when_not_found() {
        let path = unique_reply_path("delete_notfound");
        let _ = std::fs::remove_file(&path);

        let reply = GgrReply::new(&reply_params("body")).unwrap();
        append_reply(&path, &reply).unwrap();

        let deleted = delete_reply(&path, |r| r.parent_comment_id == "nonexistent").unwrap();
        assert!(!deleted);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_replies_truncates_to_empty() {
        let path = unique_reply_path("clear");
        let _ = std::fs::remove_file(&path);

        let reply = GgrReply::new(&reply_params("body")).unwrap();
        append_reply(&path, &reply).unwrap();
        assert_eq!(list_replies(&path).unwrap().len(), 1);

        clear_replies(&path).unwrap();
        assert_eq!(list_replies(&path).unwrap().len(), 0);
        assert!(path.exists(), "file must be preserved after clear");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_replies_malformed_json_returns_error() {
        let path = unique_reply_path("malformed");
        std::fs::write(&path, b"not valid json\n").unwrap();
        assert!(list_replies(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
