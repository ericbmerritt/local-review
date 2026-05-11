//! Cursor-position persistence for `ggr`.
//!
//! Saves and restores the last-viewed commit, file, and line so the reviewer
//! can resume where they left off. The storage path follows the XDG Base
//! Directory Specification: `$XDG_DATA_HOME/ggr/…` or `~/.local/share/ggr/…`.
//! All failures are silent — cursor state is advisory, never load-bearing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pr::PrDetails;

/// Saved position within a PR review session.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CursorState {
    /// Stored as `String` so an unrecognised SHA silently discards the cursor
    /// without parse error.
    pub(crate) commit_sha: String,
    /// Repo-relative file path that was focused.
    pub(crate) file: String,
    /// Row index (0-based) within the rendered view.
    pub(crate) line: usize,
}

/// Compute the path where the cursor file for `pr` should be stored.
///
/// Returns `None` when neither `$XDG_DATA_HOME` nor `$HOME` is set, which is
/// vanishingly rare but must not panic.
pub(crate) fn cursor_path(pr: &PrDetails) -> Option<PathBuf> {
    let data_home = crate::util::data_home()?;
    Some(cursor_path_from_base(&data_home, pr))
}

/// Cursor path for `pr` under `data_home`.
///
/// Extracted so tests can supply `data_home` directly without manipulating
/// environment variables.
fn cursor_path_from_base(data_home: &Path, pr: &PrDetails) -> PathBuf {
    let host = pr.hostname.as_deref().unwrap_or("github.com");
    let repo = pr.repo_name.as_str();
    // RepoName guarantees owner/repo format at construction; the fallback is unreachable in practice.
    let (owner, repo_slug) = repo.split_once('/').unwrap_or(("", repo));
    crate::util::pr_data_dir(data_home, host, owner, repo_slug, pr.number).join("cursor.json")
}

/// Load a saved cursor from `path`. Returns `None` on any failure so the
/// caller never has to handle a missing or corrupt cursor file.
pub(crate) fn load(path: &Path) -> Option<CursorState> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Serializes `state` to JSON and writes it atomically to `path`.
///
/// Crash-safety (same-filesystem rename) provided by
/// [`local_review_core::util::atomic_write_bytes`].
pub(crate) fn save(path: &Path, state: &CursorState) -> Result<(), std::io::Error> {
    let data = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    local_review_core::util::atomic_write_bytes(path, &data)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{cursor_path_from_base, load, save, CursorState};
    use crate::pr::{PrDetails, RepoName};

    fn make_cursor_pr(repo: &str, hostname: Option<&str>, number: u64) -> PrDetails {
        PrDetails {
            number,
            title: "title".to_owned(),
            body: String::new(),
            comments: vec![],
            repo_name: RepoName::try_from(repo).unwrap(),
            hostname: hostname.map(str::to_owned),
            commits: vec![],
            review_threads: vec![],
        }
    }

    fn make_state() -> CursorState {
        CursorState {
            commit_sha: "a".repeat(40),
            file: "src/lib.rs".to_owned(),
            line: 7,
        }
    }

    #[test]
    fn cursor_path_from_base_invalid_hostname_falls_back_to_github_com() {
        let base = PathBuf::from("/data");
        let pr = make_cursor_pr("owner/repo", Some("../../etc"), 7);
        let path = cursor_path_from_base(&base, &pr);
        let path_str = path.to_string_lossy();
        assert!(
            !path_str.contains(".."),
            "path must not contain '..' when hostname is crafted: {path_str}"
        );
        assert!(
            path_str.contains("github.com"),
            "path must fall back to github.com for invalid hostname: {path_str}"
        );
    }

    #[test]
    fn cursor_path_from_base_default_host_when_no_hostname() {
        let base = PathBuf::from("/data");
        let pr = make_cursor_pr("owner/repo", None, 99);
        let path = cursor_path_from_base(&base, &pr);
        assert_eq!(
            path,
            PathBuf::from("/data/ggr/github.com/owner/repo/99/cursor.json")
        );
    }

    #[test]
    fn cursor_path_from_base_custom_host() {
        let base = PathBuf::from("/data");
        let pr = make_cursor_pr("owner/repo", Some("ghe.example.com"), 1);
        let path = cursor_path_from_base(&base, &pr);
        assert_eq!(
            path,
            PathBuf::from("/data/ggr/ghe.example.com/owner/repo/1/cursor.json")
        );
    }

    #[test]
    fn cursor_path_from_base_includes_owner_repo_and_pr_number() {
        let base = PathBuf::from("/home/user/.local/share");
        let pr = make_cursor_pr("myorg/myrepo", None, 42);
        let path = cursor_path_from_base(&base, &pr);
        let components: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert!(
            components.contains(&"myorg".to_owned()),
            "path must contain owner segment"
        );
        assert!(
            components.contains(&"myrepo".to_owned()),
            "path must contain repo segment"
        );
        assert!(
            components.contains(&"42".to_owned()),
            "path must contain pr number segment"
        );
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "cursor.json");
    }

    fn unique_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ggr_cursor_test_{}_{}", name, std::process::id()))
    }

    #[test]
    fn cursor_load_missing_file_returns_none() {
        let path = unique_test_path("missing");
        assert!(load(&path).is_none());
    }

    #[test]
    fn cursor_load_invalid_json_returns_none() {
        let path = unique_test_path("invalid_json");
        std::fs::write(&path, b"not valid json").unwrap();
        let result = load(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_none());
    }

    #[test]
    fn cursor_save_and_load_roundtrip() {
        let dir = unique_test_path("roundtrip_dir");
        let path = dir.join("cursor.json");
        let state = make_state();
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.commit_sha, state.commit_sha);
        assert_eq!(loaded.file, state.file);
        assert_eq!(loaded.line, state.line);
    }

    #[test]
    fn cursor_save_creates_parent_dirs() {
        let base = unique_test_path("parent_dirs");
        let path = base.join("nested").join("deep").join("cursor.json");
        let state = make_state();
        save(&path, &state).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cursor_load_sha_from_saved_state() {
        let dir = unique_test_path("sha_check_dir");
        let path = dir.join("cursor.json");
        let state = CursorState {
            commit_sha: "deadbeef".repeat(5),
            file: "a/b.rs".to_owned(),
            line: 3,
        };
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.commit_sha, "deadbeef".repeat(5));
        assert_eq!(loaded.file, "a/b.rs");
        assert_eq!(loaded.line, 3);
    }
}
