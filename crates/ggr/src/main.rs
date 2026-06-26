//! `ggr` — local terminal review surface for GitHub pull requests.
//!
//! Usage:
//!   `ggr 42`                                               — auto-detect repo from git remote
//!   `ggr acme/myrepo#2429`                                 — explicit repo, works anywhere
//!   `ggr --url https://github.example.com owner/repo#2429` — GHE host + short form
//!   `ggr https://github.example.com/owner/repo/pull/2429`  — full pull URL
//!   `ggr drafts <pr-ref>`                                  — list local draft comments
//!   `ggr clear <pr-ref>`                                   — clear local draft comments

mod cursor;
mod draft;
mod error;
mod gh;
mod pr;
mod pr_ref;
mod reanchor;
mod repo_cache;
mod submit;
mod tui;
mod util;

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use error::GgrError;

#[derive(Parser)]
#[command(
    name = "ggr",
    about = "Local terminal review surface for GitHub pull requests"
)]
struct Cli {
    /// PR to review: number, owner/repo#number, or full pull URL.
    #[arg(value_name = "PR", required = false)]
    pr: Option<String>,

    /// Base URL of a GitHub Enterprise Server instance (e.g. `https://github.example.com`).
    /// Required when using the owner/repo#number form against a GHE host.
    #[arg(long)]
    url: Option<String>,

    /// Disable cloning the repository to /tmp for dependency-graph building.
    ///
    /// By default ggr clones the PR's repository to /tmp/ggr-repos/ so it can
    /// compute a cross-file call graph (used for topo-sorted entity lists and
    /// the Claude context bundle). The clone is shallow, read-only, and reused
    /// across sessions. Pass this flag — or set `GGR_NO_GRAPH_CLONE=1` — if you
    /// prefer not to download code from the internet.
    #[arg(long = "no-graph")]
    no_graph: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List local draft comments for a PR in human-readable form.
    Drafts {
        /// PR reference: number, owner/repo#number, or full pull URL.
        pr: String,
        /// Base URL of a GitHub Enterprise Server instance.
        #[arg(long)]
        url: Option<String>,
    },
    /// Clear local draft comments for a PR (preserves the drafts directory).
    Clear {
        /// PR reference: number, owner/repo#number, or full pull URL.
        pr: String,
        /// Base URL of a GitHub Enterprise Server instance.
        #[arg(long)]
        url: Option<String>,
        /// Clear only drafts that are stale (anchor no longer matches current diff).
        #[arg(long)]
        stale: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let stderr = std::io::stderr();
            let mut handle = stderr.lock();
            let _ = writeln!(handle, "ggr: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> error::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Drafts { pr, url }) => cmd_drafts(&pr, url.as_deref()),
        Some(Commands::Clear { pr, url, stale }) => cmd_clear(&pr, url.as_deref(), stale),
        None => {
            let pr_str = cli.pr.ok_or_else(|| GgrError::InvalidPrRef {
                raw: "no PR reference given; run `ggr --help` for usage".to_owned(),
            })?;
            cmd_review(&pr_str, cli.url.as_deref(), !cli.no_graph)
        }
    }
}

fn cmd_review(pr_str: &str, url: Option<&str>, allow_graph_clone: bool) -> error::Result<()> {
    let mut parsed = pr_ref::parse(pr_str, url)?;
    if parsed.hostname.is_none() {
        if let Some(host) = util::detect_remote_host(parsed.repo_flag.as_deref()) {
            if let Some(repo) = parsed.repo_flag.take() {
                parsed.repo_flag = Some(format!("{host}/{repo}"));
            }
            parsed.hostname = Some(host);
        }
    }
    let spinner = local_review_core::startup_spinner::StartupSpinner::start(format!(
        "Loading PR #{}…",
        parsed.number
    ));
    let pr = gh::fetch_pr_details(
        parsed.number,
        parsed.repo_flag.as_deref(),
        parsed.hostname.as_deref(),
    )?;
    if pr.commits.is_empty() {
        spinner.stop();
        return Err(GgrError::PrNotFound { pr: parsed.number });
    }
    // Re-anchor local drafts against the freshly fetched PR state.
    let stale_count = util::data_home()
        .map(|base| reanchor::reanchor_all(&pr, &base))
        .unwrap_or(0);
    spinner.stop();
    tui::run(pr, stale_count, allow_graph_clone)
}

/// Resolve a PR reference string to `(host, owner, repo, pr_number)` for
/// storage path construction.
///
/// For bare PR numbers the git remote is queried. If the remote can't be
/// detected the caller should use the `owner/repo#N` or full-URL form.
fn resolve_pr_coords(
    pr_str: &str,
    url: Option<&str>,
) -> error::Result<(String, String, String, u64)> {
    let parsed = pr_ref::parse(pr_str, url)?;

    let Some(repo_flag) = parsed.repo_flag else {
        // Bare number — derive coords from the local git remote.
        let Some((host, owner, repo)) = util::detect_remote_coords() else {
            return Err(GgrError::InvalidPrRef {
                raw: "cannot locate drafts for a bare PR number without a detectable git \
                      remote; use owner/repo#N or a full URL"
                    .to_owned(),
            });
        };
        return Ok((host, owner, repo, parsed.number));
    };

    let host = parsed
        .hostname
        .as_deref()
        .unwrap_or("github.com")
        .to_owned();
    // For GHE, repo_flag is "HOST/owner/repo"; strip the leading host segment.
    let slug = if parsed.hostname.is_some() {
        repo_flag
            .strip_prefix(&format!("{host}/"))
            .unwrap_or(repo_flag.as_str())
            .to_owned()
    } else {
        repo_flag.clone()
    };
    let (owner, repo) = slug.split_once('/').ok_or_else(|| GgrError::InvalidPrRef {
        raw: format!("expected owner/repo, got: {slug}"),
    })?;
    Ok((host, owner.to_owned(), repo.to_owned(), parsed.number))
}

fn cmd_drafts(pr_str: &str, url: Option<&str>) -> error::Result<()> {
    let (host, owner, repo, pr_number) = resolve_pr_coords(pr_str, url)?;
    let Some(base) = util::data_home() else {
        return Err(GgrError::Io {
            source: std::io::Error::other("could not determine data directory"),
        });
    };
    let drafts_dir = draft::drafts_dir_from_base(&base, &host, &owner, &repo, pr_number);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if !drafts_dir.exists() {
        writeln!(out, "no drafts for {owner}/{repo}#{pr_number}")
            .map_err(|source| GgrError::Io { source })?;
        return Ok(());
    }

    let files = collect_draft_files(&drafts_dir)?;
    let total: usize = files
        .iter()
        .map(|p| draft::list_drafts(p).map(|v| v.len()).unwrap_or(0))
        .sum();

    if total == 0 {
        writeln!(out, "no drafts for {owner}/{repo}#{pr_number}")
            .map_err(|source| GgrError::Io { source })?;
        return Ok(());
    }

    writeln!(out, "{owner}/{repo}#{pr_number} — {total} draft(s)")
        .map_err(|source| GgrError::Io { source })?;

    for path in &files {
        let drafts = draft::list_drafts(path)?;
        if drafts.is_empty() {
            continue;
        }
        print_draft_group(&mut out, path, &drafts)?;
    }

    Ok(())
}

fn cmd_clear(pr_str: &str, url: Option<&str>, stale_only: bool) -> error::Result<()> {
    let (host, owner, repo, pr_number) = resolve_pr_coords(pr_str, url)?;
    let Some(base) = util::data_home() else {
        return Err(GgrError::Io {
            source: std::io::Error::other("could not determine data directory"),
        });
    };
    let drafts_dir = draft::drafts_dir_from_base(&base, &host, &owner, &repo, pr_number);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if !drafts_dir.exists() {
        writeln!(out, "no drafts for {owner}/{repo}#{pr_number}")
            .map_err(|source| GgrError::Io { source })?;
        return Ok(());
    }

    let files = collect_draft_files(&drafts_dir)?;
    let mut cleared = 0usize;
    for path in &files {
        let all = draft::list_drafts(path)?;
        if stale_only {
            let kept: Vec<_> = all
                .iter()
                .filter(|d| d.status != Some(draft::DraftStatus::Stale))
                .collect();
            let removed = all.len() - kept.len();
            if removed > 0 {
                let kept_owned: Vec<_> = kept.into_iter().cloned().collect();
                draft::write_drafts_to_path(path, &kept_owned)?;
                cleared += removed;
            }
        } else if !all.is_empty() {
            draft::clear_drafts(path)?;
            cleared += all.len();
        }
    }
    // Handle replies.
    let replies_file = draft::replies_file_from_base(&base, &host, &owner, &repo, pr_number);
    if replies_file.exists() {
        let all_replies = draft::list_replies(&replies_file)?;
        if stale_only {
            let kept: Vec<_> = all_replies
                .iter()
                .filter(|r| r.status != Some(draft::DraftStatus::Stale))
                .cloned()
                .collect();
            let removed = all_replies.len() - kept.len();
            if removed > 0 {
                draft::write_replies_to_path(&replies_file, &kept)?;
                cleared += removed;
            }
        } else if !all_replies.is_empty() {
            draft::clear_replies(&replies_file)?;
            cleared += all_replies.len();
        }
    }

    if cleared == 0 {
        writeln!(out, "no drafts to clear for {owner}/{repo}#{pr_number}")
            .map_err(|source| GgrError::Io { source })?;
    } else {
        writeln!(
            out,
            "cleared {cleared} draft(s) for {owner}/{repo}#{pr_number}"
        )
        .map_err(|source| GgrError::Io { source })?;
    }

    Ok(())
}

/// Collect draft files from `drafts_dir`: `_pr.jsonl` first, then commit
/// files sorted by name.
fn collect_draft_files(drafts_dir: &Path) -> error::Result<Vec<std::path::PathBuf>> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let pr_file = drafts_dir.join("_pr.jsonl");
    if pr_file.exists() {
        files.push(pr_file);
    }
    let mut commit_files: Vec<std::path::PathBuf> = std::fs::read_dir(drafts_dir)
        .map_err(|source| GgrError::DraftIo { source })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && p.file_name().and_then(|n| n.to_str()) != Some("_pr.jsonl")
        })
        .collect();
    commit_files.sort();
    files.extend(commit_files);
    Ok(files)
}

fn print_draft_group(
    out: &mut impl std::io::Write,
    path: &Path,
    drafts: &[draft::GgrDraft],
) -> error::Result<()> {
    let label = match path.file_name().and_then(|n| n.to_str()) {
        Some("_pr.jsonl") => "PR".to_owned(),
        Some(name) => format!("Commit {}", name.trim_end_matches(".jsonl")),
        None => "Unknown".to_owned(),
    };
    writeln!(out, "\n── {label} ──").map_err(|source| GgrError::Io { source })?;

    for d in drafts {
        let severity = match d.severity {
            local_review_core::Severity::Required => "[REQUIRED]",
            local_review_core::Severity::Suggestion => "[SUGGESTION]",
            local_review_core::Severity::Note => "[NOTE]",
        };
        let anchor = match &d.anchor {
            draft::GgrAnchor::Line {
                file,
                new_line,
                old_line,
                ..
            } => {
                let line = new_line
                    .map(|l| (l, "new"))
                    .or_else(|| old_line.map(|l| (l, "old")));
                match line {
                    Some((n, side)) => format!("{file}:{n} ({side})"),
                    None => file.clone(),
                }
            }
            draft::GgrAnchor::Commit { .. } => "(commit-scope)".to_owned(),
            draft::GgrAnchor::Pr => "(PR-scope)".to_owned(),
        };
        writeln!(out, "{severity} {anchor}").map_err(|source| GgrError::Io { source })?;
        for line in local_review_core::util::strip_controls(&d.body).lines() {
            writeln!(out, "  {line}").map_err(|source| GgrError::Io { source })?;
        }
        writeln!(out).map_err(|source| GgrError::Io { source })?;
    }

    Ok(())
}
