use std::process::Command;

use crate::change_id::{ChangeId, CommitId};
use crate::diff::{self, Diff};
use crate::error::{JjrError, Result};

#[derive(Debug, Clone)]
pub struct ChangeDetails {
    pub change_id: ChangeId,
    pub commit_id: CommitId,
    pub description: String,
    pub diff: Diff,
}

/// Resolve a revset expression (including `@`, `@-`, branch names, etc.) to a
/// `ChangeId`. The revset must resolve to exactly one change.
pub fn resolve_revset(revset: &str) -> Result<ChangeId> {
    // Template emits one ID per line followed by `\n` so multi-match revsets
    // are detectable. Without the trailing newline jj concatenates all matching
    // IDs on a single line and a "is there a second line?" check would silently
    // pass for multi-match revsets.
    let output = run_jj(&[
        "log",
        "-r",
        revset,
        "--no-graph",
        "--color=never",
        "-T",
        r#"change_id ++ "\n""#,
    ])?;
    let non_empty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    match non_empty.as_slice() {
        [single] => ChangeId::parse(single.trim()),
        [] => Err(JjrError::RevsetNoMatch {
            revset: revset.to_owned(),
        }),
        _ => Err(JjrError::RevsetAmbiguous {
            revset: revset.to_owned(),
            raw: output,
        }),
    }
}

pub fn show(change_id: &ChangeId) -> Result<ChangeDetails> {
    let metadata = log_metadata(change_id)?;
    let diff = show_diff(change_id)?;
    Ok(ChangeDetails {
        change_id: metadata.change_id,
        commit_id: metadata.commit_id,
        description: metadata.description,
        diff,
    })
}

struct LogMetadata {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
}

fn log_metadata(change_id: &ChangeId) -> Result<LogMetadata> {
    let template = r#"change_id ++ "\n" ++ commit_id ++ "\n" ++ description.first_line()"#;
    let output = run_jj(&[
        "log",
        "-r",
        change_id.as_str(),
        "--no-graph",
        "--color=never",
        "-T",
        template,
    ])?;

    let mut lines = output.lines();
    let raw_change_id = lines
        .next()
        .ok_or_else(|| JjrError::JjUnexpectedOutput {
            raw: output.clone(),
        })?
        .trim()
        .to_owned();
    let raw_commit_id = lines
        .next()
        .ok_or_else(|| JjrError::JjUnexpectedOutput {
            raw: output.clone(),
        })?
        .trim()
        .to_owned();
    // An empty description (jj returned the line, but it was empty) is valid;
    // a missing line is not.
    let description = lines
        .next()
        .ok_or_else(|| JjrError::JjUnexpectedOutput {
            raw: output.clone(),
        })?
        .trim()
        .to_owned();

    Ok(LogMetadata {
        change_id: ChangeId::parse(&raw_change_id)?,
        commit_id: CommitId::parse(&raw_commit_id)?,
        description,
    })
}

fn show_diff(change_id: &ChangeId) -> Result<Diff> {
    let output = run_jj(&["show", change_id.as_str(), "--git", "--color=never"])?;
    diff::parse(&output)
}

fn run_jj(args: &[&str]) -> Result<String> {
    let output = Command::new("jj").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            JjrError::JjMissing { source: e }
        } else {
            JjrError::Io { source: e }
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(JjrError::JjFailed {
            message: stderr,
            exit_code: output.status.code(),
        });
    }

    String::from_utf8(output.stdout).map_err(|source| JjrError::JjOutputEncoding { source })
}
