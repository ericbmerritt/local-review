//! Local repository clone cache for graph building.
//!
//! ggr clones a PR's repository to `/tmp/ggr-repos/<host>/<owner>/<repo>/`
//! and checks it out at the **PR head SHA** so `build_graph` constructs the
//! call graph from the exact state under review — the same guarantee jjr
//! gets from its local working copy. A clone that cannot reach the head SHA
//! (fork PRs the remote won't serve, fetch limits) is treated as failure:
//! degraded is acceptable, a silently wrong-state graph is not. The clone is
//! intentionally left in `/tmp` (no cleanup); the OS evicts it on reboot and
//! it is reused across sessions and PRs in the meantime (each session
//! re-checks-out its own head).
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

/// Outcome of [`ensure_clone_at`]. `Disabled` and `Failed` both leave the
/// graph unavailable; they are distinct because the reviewer chose the
/// former and should not be told something went wrong.
#[derive(Debug)]
pub(crate) enum CloneStatus {
    /// Clone exists and is checked out at the requested head SHA.
    Ready(PathBuf),
    /// The reviewer opted out; the string names the opt-out mechanism.
    Disabled(String),
    /// Clone, fetch, or checkout failed; the string is a short reason safe
    /// to render (no captured subprocess output — stderr from a hostile
    /// remote is an injection vector).
    Failed(String),
}

/// Everything [`ensure_clone_at`] needs. The two `Option` fields are test
/// seams: production callers pass `None` and get the `gh`-CLI clone into
/// the shared `/tmp` cache; tests point at a `file://` fixture remote and
/// a tempdir root, exercising the identical fetch/checkout path without
/// network or global state.
pub(crate) struct CloneRequest<'a> {
    pub owner_repo: &'a str,
    pub hostname: Option<&'a str>,
    /// PR number, used to fetch `pull/<n>/head` when the head SHA is not
    /// directly fetchable (fork PRs whose commits live outside any branch).
    pub pr_number: u64,
    /// Commit the clone must end up checked out at.
    pub head_sha: &'a str,
    /// `false` when `--no-graph` was passed.
    pub allow_clone: bool,
    /// Test seam: clone this URL with plain `git` instead of `gh repo clone`.
    pub remote_override: Option<&'a str>,
    /// Test seam: root directory replacing `$TMPDIR/ggr-repos`.
    pub cache_root: Option<&'a Path>,
}

/// Why cloning is disabled, or `None` when it may proceed. Pure over its
/// inputs so both opt-out mechanisms are unit-testable without touching
/// process environment.
pub(crate) fn opt_out_reason(
    allow_clone: bool,
    no_clone_env: Option<&std::ffi::OsStr>,
) -> Option<String> {
    if !allow_clone {
        return Some("--no-graph".to_owned());
    }
    if no_clone_env.is_some_and(|v| !v.is_empty()) {
        return Some(format!("{NO_CLONE_ENV_VAR}=1"));
    }
    None
}

/// Ensure a clone of the PR's repository exists and is checked out at
/// `req.head_sha`. Reuses an existing clone when present (fetching the head
/// if it is not yet local), so repeat opens of the same PR cost one
/// `rev-parse`.
pub(crate) fn ensure_clone_at(req: &CloneRequest<'_>) -> CloneStatus {
    let env = std::env::var_os(NO_CLONE_ENV_VAR);
    if let Some(reason) = opt_out_reason(req.allow_clone, env.as_deref()) {
        return CloneStatus::Disabled(reason);
    }
    if !valid_sha(req.head_sha) {
        // The SHA reaches `git` argv; a malformed one from a hostile API
        // response must not get that far.
        return CloneStatus::Failed("malformed PR head SHA".to_owned());
    }

    let repo_path = clone_path(req.owner_repo, req.hostname, req.cache_root);
    if !repo_path.join(".git").exists() {
        if let Err(reason) = clone_repo(req, &repo_path) {
            return CloneStatus::Failed(reason);
        }
    }
    match checkout_head(&repo_path, req.head_sha, req.pr_number) {
        Ok(()) => CloneStatus::Ready(repo_path),
        Err(reason) => CloneStatus::Failed(reason),
    }
}

fn clone_repo(req: &CloneRequest<'_>, repo_path: &Path) -> Result<(), String> {
    let Some(parent) = repo_path.parent() else {
        return Err("clone path has no parent directory".to_owned());
    };
    if std::fs::create_dir_all(parent).is_err() {
        return Err("cannot create clone cache directory".to_owned());
    }

    let mut cmd = if let Some(url) = req.remote_override {
        let mut c = Command::new("git");
        c.args(["clone", "--depth=1", "--quiet", url])
            .arg(repo_path);
        c
    } else {
        let mut c = Command::new("gh");
        c.args([
            "repo",
            "clone",
            req.owner_repo,
            &repo_path.to_string_lossy(),
            "--",
            "--depth=1",
            "--quiet",
        ]);
        if let Some(h) = req.hostname {
            c.env("GH_HOST", h);
        }
        c
    };
    // Point git's OpenSSL backend at the system certificate bundle so GHE
    // certificates are trusted without disabling verification. Homebrew git
    // uses its own OpenSSL which doesn't read the macOS Keychain; this
    // bridges the gap. Priority:
    //   1. GIT_SSL_CAINFO already set by the caller — respect it.
    //   2. macOS system root bundle at /etc/ssl/cert.pem (symlink to Keychain).
    //   3. Linux CA bundle paths.
    if std::env::var_os("GIT_SSL_CAINFO").is_none() {
        if let Some(bundle) = system_ca_bundle() {
            cmd.env("GIT_SSL_CAINFO", bundle);
        }
    }
    // Suppress output so a failed clone doesn't bleed remote-controlled
    // error text into the reviewer's terminal.
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err("clone failed".to_owned()),
    }
}

/// Bring `sha` into the clone (if absent) and check it out detached.
///
/// Fetch ladder, cheapest first — each rung re-checks presence:
/// 1. the SHA directly (GitHub serves reachable SHAs by hash);
/// 2. `pull/<n>/head` (fork PRs whose commits live outside any branch);
/// 3. all branch tips (`file://` fixtures and remotes that only serve refs).
fn checkout_head(repo: &Path, sha: &str, pr_number: u64) -> Result<(), String> {
    if head_commit(repo).as_deref() == Some(sha) {
        return Ok(());
    }
    if !commit_present(repo, sha) {
        let pull_ref = format!("pull/{pr_number}/head");
        let attempts: [&[&str]; 3] = [
            &["fetch", "--depth=1", "--quiet", "origin", sha],
            &["fetch", "--depth=1", "--quiet", "origin", &pull_ref],
            &["fetch", "--depth=1", "--quiet", "origin"],
        ];
        for args in attempts {
            let _ = quiet_git(repo, args);
            if commit_present(repo, sha) {
                break;
            }
        }
        if !commit_present(repo, sha) {
            let short: String = sha.chars().take(8).collect();
            return Err(format!("cannot reach PR head {short}"));
        }
    }
    if quiet_git(repo, &["checkout", "--detach", "--quiet", sha]) {
        Ok(())
    } else {
        Err("checkout of PR head failed".to_owned())
    }
}

/// Run `git <args>` in `repo` with all output suppressed; `true` on success.
fn quiet_git(repo: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn head_commit(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_owned())
}

fn commit_present(repo: &Path, sha: &str) -> bool {
    quiet_git(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
}

/// Hex, 7–40 chars — the only shape allowed into `git` argv.
fn valid_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
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

/// Return the path to the system CA certificate bundle, or `None` if none of
/// the standard locations are present.
///
/// On macOS, `/etc/ssl/cert.pem` is a symlink maintained by the OS that
/// reflects the system root keychain — it is the same bundle that the macOS
/// `SecureTransport` and `curl` backends use. On Linux, the bundle is typically
/// at `/etc/ssl/certs/ca-certificates.crt` (Debian/Ubuntu) or
/// `/etc/pki/tls/certs/ca-bundle.crt` (RHEL/Fedora).
fn system_ca_bundle() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/etc/ssl/cert.pem",                  // macOS + some BSDs
        "/etc/ssl/certs/ca-certificates.crt", // Debian / Ubuntu
        "/etc/pki/tls/certs/ca-bundle.crt",   // RHEL / Fedora / CentOS
        "/etc/ssl/ca-bundle.pem",             // openSUSE
        "/usr/share/ssl/certs/ca-bundle.crt", // older RHEL
    ];
    CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(Path::to_path_buf)
}

/// `<root>/<host>/<owner>/<repo>` where `<root>` defaults to
/// `$TMPDIR/ggr-repos`.
fn clone_path(owner_repo: &str, hostname: Option<&str>, cache_root: Option<&Path>) -> PathBuf {
    let host = hostname.unwrap_or("github.com");
    let (owner, repo) = owner_repo.split_once('/').unwrap_or((owner_repo, "repo"));
    let root = cache_root.map_or_else(|| std::env::temp_dir().join("ggr-repos"), Path::to_path_buf);
    root.join(host).join(owner).join(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn rev_head(dir: &Path) -> String {
        head_commit(dir).expect("fixture HEAD")
    }

    /// A file:// fixture remote with a cross-file call (`caller` → `callee`)
    /// so the resulting clone exercises the same graph build ggr runs.
    fn make_fixture(dir: &Path) -> String {
        git_in(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("callee.rs"), "pub fn callee() -> i32 { 1 }\n").expect("write");
        std::fs::write(
            dir.join("caller.rs"),
            "pub fn caller() -> i32 { callee() + 1 }\n",
        )
        .expect("write");
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-q", "-m", "init"]);
        rev_head(dir)
    }

    fn request<'a>(remote: &'a str, head_sha: &'a str, cache_root: &'a Path) -> CloneRequest<'a> {
        CloneRequest {
            owner_repo: "fixture/repo",
            hostname: None,
            pr_number: 1,
            head_sha,
            allow_clone: true,
            remote_override: Some(remote),
            cache_root: Some(cache_root),
        }
    }

    #[test]
    fn clone_lands_at_head_sha_and_graph_sees_cross_file_call() {
        let fixture = tempfile::tempdir().expect("fixture dir");
        let head = make_fixture(fixture.path());
        let cache = tempfile::tempdir().expect("cache dir");
        let remote = format!("file://{}", fixture.path().display());

        let status = ensure_clone_at(&request(&remote, &head, cache.path()));
        let CloneStatus::Ready(path) = status else {
            panic!("expected Ready, got {status:?}");
        };
        assert_eq!(rev_head(&path), head, "clone must sit at the PR head SHA");

        // The done-when substance: the clone feeds build_graph and yields
        // caller data without any entry having been visited.
        let registry = local_review_core::semantic::create_default_registry();
        let files = list_files(&path);
        let graph = local_review_core::semantic::build_graph(&registry, &path, &files);
        let callee_edges = graph
            .edges
            .iter()
            .filter(|e| e.to.name() == "callee")
            .count();
        assert_eq!(callee_edges, 1, "fixture caller→callee edge must resolve");
    }

    #[test]
    fn existing_clone_is_fetched_forward_to_a_new_head() {
        let fixture = tempfile::tempdir().expect("fixture dir");
        let first = make_fixture(fixture.path());
        let cache = tempfile::tempdir().expect("cache dir");
        let remote = format!("file://{}", fixture.path().display());

        let status = ensure_clone_at(&request(&remote, &first, cache.path()));
        assert!(matches!(status, CloneStatus::Ready(_)), "got {status:?}");

        // Force-push simulation: fixture advances; the reused clone must
        // fetch and land on the new head, not silently stay on the old one.
        std::fs::write(fixture.path().join("extra.rs"), "pub fn extra() {}\n").expect("write");
        git_in(fixture.path(), &["add", "."]);
        git_in(fixture.path(), &["commit", "-q", "-m", "more"]);
        let second = rev_head(fixture.path());
        assert_ne!(first, second);

        let status = ensure_clone_at(&request(&remote, &second, cache.path()));
        let CloneStatus::Ready(path) = status else {
            panic!("expected Ready after head moved, got {status:?}");
        };
        assert_eq!(rev_head(&path), second);
    }

    #[test]
    fn unreachable_remote_fails_visibly() {
        let cache = tempfile::tempdir().expect("cache dir");
        let status = ensure_clone_at(&request(
            "file:///nonexistent/ggr-fixture-void",
            "0123456789abcdef0123456789abcdef01234567",
            cache.path(),
        ));
        let CloneStatus::Failed(reason) = status else {
            panic!("expected Failed, got {status:?}");
        };
        assert_eq!(reason, "clone failed");
    }

    #[test]
    fn unreachable_head_sha_fails_visibly() {
        let fixture = tempfile::tempdir().expect("fixture dir");
        let _head = make_fixture(fixture.path());
        let cache = tempfile::tempdir().expect("cache dir");
        let remote = format!("file://{}", fixture.path().display());

        let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let status = ensure_clone_at(&request(&remote, bogus, cache.path()));
        let CloneStatus::Failed(reason) = status else {
            panic!("expected Failed, got {status:?}");
        };
        assert_eq!(reason, "cannot reach PR head deadbeef");
    }

    #[test]
    fn malformed_sha_is_rejected_before_reaching_git() {
        let cache = tempfile::tempdir().expect("cache dir");
        let status = ensure_clone_at(&request(
            "file:///unused",
            "--upload-pack=evil",
            cache.path(),
        ));
        let CloneStatus::Failed(reason) = status else {
            panic!("expected Failed, got {status:?}");
        };
        assert_eq!(reason, "malformed PR head SHA");
    }

    #[test]
    fn opt_outs_are_reported_as_disabled_not_failed() {
        assert_eq!(opt_out_reason(false, None).as_deref(), Some("--no-graph"));
        assert_eq!(
            opt_out_reason(true, Some(std::ffi::OsStr::new("1"))).as_deref(),
            Some("GGR_NO_GRAPH_CLONE=1")
        );
        // Empty value = unset, mirroring the historical behavior.
        assert_eq!(opt_out_reason(true, Some(std::ffi::OsStr::new(""))), None);
        assert_eq!(opt_out_reason(true, None), None);

        let cache = tempfile::tempdir().expect("cache dir");
        let mut req = request("file:///unused", "0123456789abcdef012345", cache.path());
        req.allow_clone = false;
        let status = ensure_clone_at(&req);
        assert!(
            matches!(status, CloneStatus::Disabled(ref r) if r == "--no-graph"),
            "got {status:?}"
        );
    }

    #[test]
    fn valid_sha_bounds() {
        assert!(valid_sha("abcdef0"));
        assert!(valid_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_sha("abcdef")); // 6 chars
        assert!(!valid_sha("xyzxyzx")); // non-hex
        assert!(!valid_sha(""));
    }
}
