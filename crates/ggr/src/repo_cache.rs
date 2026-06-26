//! Local repository clone cache for graph building.
//!
//! ggr can clone a PR's repository to `/tmp/ggr-repos/<host>/<owner>/<repo>/`
//! so `build_graph` can construct the full cross-file call graph — the same
//! graph that jjr builds from its local working copy. The clone is intentionally
//! left in `/tmp` (no cleanup); the OS evicts it on reboot and it is reused
//! across sessions in the meantime.
//!
//! ## Security
//!
//! Cloning a repository downloads code from the internet. If you are reviewing
//! a PR from an untrusted fork, the code lands in `/tmp`. `build_graph` is a
//! read-only tree-sitter parse — it does not execute the code — so the attack
//! surface is limited to tree-sitter parser vulnerabilities. Still, if you
//! prefer not to have the code cloned, pass `--no-graph` to `ggr` or set
//! `GGR_NO_GRAPH_CLONE=1`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable that disables the repo clone when set to any non-empty
/// value. Takes precedence over the `--no-graph` flag default.
pub(crate) const NO_CLONE_ENV_VAR: &str = "GGR_NO_GRAPH_CLONE";

/// Ensure a shallow clone of `owner_repo` exists under `/tmp/ggr-repos/` and
/// return its path, or `None` when cloning is disabled or fails.
///
/// If the directory already exists and contains a `.git` entry, the existing
/// clone is returned without network access. This makes subsequent calls
/// essentially free.
///
/// `hostname` is the GHE host (e.g. `"github.example.com"`); pass `None` for
/// github.com. `allow_clone` reflects the `--no-graph` CLI flag — pass `false`
/// to skip cloning even when the env var is not set.
pub(crate) fn ensure_clone(
    owner_repo: &str,
    hostname: Option<&str>,
    allow_clone: bool,
) -> Option<PathBuf> {
    if !allow_clone || std::env::var_os(NO_CLONE_ENV_VAR).is_some_and(|v| !v.is_empty()) {
        return None;
    }

    let repo_path = clone_path(owner_repo, hostname);

    // Reuse existing clone — a `.git` directory is enough to confirm it.
    if repo_path.join(".git").exists() {
        return Some(repo_path);
    }

    // Clone doesn't exist yet.  Parent must exist before `gh repo clone`.
    let parent = repo_path.parent()?;
    std::fs::create_dir_all(parent).ok()?;

    let mut cmd = Command::new("gh");
    cmd.args([
        "repo",
        "clone",
        owner_repo,
        &repo_path.to_string_lossy(),
        "--",
        "--depth=1",
        "--quiet",
    ]);
    if let Some(h) = hostname {
        cmd.env("GH_HOST", h);
    }
    let status = cmd.status().ok()?;
    if status.success() {
        Some(repo_path)
    } else {
        None
    }
}

/// List every tracked file in the cloned repo as repo-relative `PathBuf`s.
///
/// Uses `git ls-files`, which respects `.gitignore` and excludes untracked
/// files — the same scope `jj files` covers in jjr.
pub(crate) fn list_files(repo_path: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_path)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| PathBuf::from(local_review_core::util::strip_controls(l)))
        .collect()
}

/// `/tmp/ggr-repos/<host>/<owner>/<repo>`
fn clone_path(owner_repo: &str, hostname: Option<&str>) -> PathBuf {
    let host = hostname.unwrap_or("github.com");
    let (owner, repo) = owner_repo.split_once('/').unwrap_or((owner_repo, "repo"));
    std::env::temp_dir()
        .join("ggr-repos")
        .join(host)
        .join(owner)
        .join(repo)
}
