/// Parse a confirmation response from the user.
///
/// Returns `true` for any casing of `y` or `yes`. Anything else — including
/// an empty string — is treated as rejection and returns `false`.
#[must_use]
pub fn confirm_response(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

pub use local_review_core::util::pluralize;
pub(crate) use local_review_core::util::truncate;
#[cfg(test)]
pub(crate) use local_review_core::util::{clamp_with_delta, page_size};

/// Emit a `warning: <msg>` line to stderr, locked for the duration of the
/// write so concurrent calls do not interleave. Mirrors `store.rs`'s prior
/// in-place helper; centralizing here keeps the wire format ("warning: …")
/// in one place and gives reviewed-state load failures the same surface.
pub(crate) fn log_warning(msg: &str) {
    use std::io::Write as _;
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "warning: {msg}");
}

/// Atomically write `bytes` to `path`, creating `dir` if needed.
///
/// Three call sites (cursor, comment store, reviewed-state) used to inline
/// the same `create_dir_all + tempfile + write_all + flush + persist`
/// sequence; centralizing the idiom here keeps the crash-safety contract in
/// one named place. `dir` must be the parent directory of `path` (passed
/// explicitly so the caller can hold an owned `PathBuf` for both without an
/// extra `parent()` round-trip).
///
/// Crash safety: writes go to a randomized sibling temp file, which `persist`
/// renames into place; same-directory placement keeps the rename on a single
/// filesystem so the OS can guarantee atomicity.
pub(crate) fn atomic_write_bytes(
    dir: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> crate::error::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir).map_err(|source| crate::error::JjrError::Io { source })?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.write_all(bytes)
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.flush()
        .map_err(|source| crate::error::JjrError::Io { source })?;
    tmp.persist(path).map_err(|e| crate::error::JjrError::Io {
        source: std::io::Error::other(e),
    })?;
    Ok(())
}

/// Resolve the path to the global `jjr` config file.
///
/// Resolution order:
/// 1. `JJR_CONFIG_PATH` if set and non-empty — used verbatim. Test-only
///    override; not advertised as a user-facing knob.
/// 2. `XDG_CONFIG_HOME/jjr/config.toml` if `XDG_CONFIG_HOME` is set and
///    non-empty.
/// 3. `HOME/.config/jjr/config.toml` if `HOME` is set and non-empty.
/// 4. `None` — caller falls back to defaults.
///
/// On macOS we deliberately use the XDG layout (`~/.config/jjr/`) rather than
/// `~/Library/Application Support/`; this matches `jj` itself and other CLI
/// tools.
pub(crate) fn global_config_path() -> Option<std::path::PathBuf> {
    if let Some(override_path) = std::env::var_os("JJR_CONFIG_PATH").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(override_path));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(
            std::path::PathBuf::from(xdg)
                .join("jjr")
                .join("config.toml"),
        );
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config")
            .join("jjr")
            .join("config.toml"),
    )
}

/// Load and parse the global `jjr` config file as a TOML table.
///
/// Resolves the path via [`global_config_path`]. Returns `None` if the path
/// can't be resolved (no `HOME`/`XDG_CONFIG_HOME`), the file is missing,
/// unreadable, or malformed. Section- and field-level fallbacks are the
/// caller's responsibility.
pub(crate) fn load_global_config_table() -> Option<toml::Table> {
    let path = global_config_path()?;
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    raw.parse::<toml::Table>().ok()
}

/// Locate the jj repo root by walking up from the process's current directory.
///
/// Returns the first ancestor that contains a `.jj/` directory. If no
/// `.jj/` is found before reaching the filesystem root, returns
/// `JjrError::NotInJjRepo` carrying the original cwd for diagnostics.
///
/// This wraps `find_repo_root_from(&current_dir())` so the cwd-touching part
/// stays separate from the pure walk-up logic that the tests exercise.
pub fn find_repo_root() -> crate::error::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().map_err(|source| crate::error::JjrError::Io { source })?;
    find_repo_root_from(&cwd)
}

/// Walk up from `start` looking for a directory containing a `.jj/`
/// subdirectory. Pure on its inputs (no cwd or env access) so tests can
/// drive it with arbitrary fixture paths.
///
/// A `.jj` entry that is a regular file (not a directory) is ignored; jj
/// itself only treats `.jj/` as a repo marker.
fn find_repo_root_from(start: &std::path::Path) -> crate::error::Result<std::path::PathBuf> {
    let mut current = start;
    loop {
        if current.join(".jj").is_dir() {
            return Ok(current.to_owned());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => {
                return Err(crate::error::JjrError::NotInJjRepo {
                    cwd: start.to_owned(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_with_delta_moves_forward() {
        assert_eq!(clamp_with_delta(3, 2, 10), 5);
    }

    #[test]
    fn clamp_with_delta_clamps_at_max() {
        assert_eq!(clamp_with_delta(8, 5, 10), 10);
    }

    #[test]
    fn clamp_with_delta_clamps_at_zero() {
        assert_eq!(clamp_with_delta(2, -5, 10), 0);
    }

    #[test]
    fn clamp_with_delta_stays_at_zero() {
        assert_eq!(clamp_with_delta(0, -1, 10), 0);
    }

    #[test]
    fn clamp_with_delta_exact_zero() {
        assert_eq!(clamp_with_delta(0, 0, 10), 0);
    }

    #[test]
    fn page_size_normal() {
        assert_eq!(page_size(20), 19);
    }

    #[test]
    fn page_size_minimum_one() {
        assert_eq!(page_size(0), 1);
        assert_eq!(page_size(1), 1);
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate("hello world", 8);
        assert_eq!(result, "hello w…");
        assert_eq!(result.chars().count(), 8);
    }

    #[test]
    fn truncate_empty_string_unchanged() {
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn truncate_max_zero_returns_empty() {
        // At max==0 even the `…` indicator would overflow the budget; the
        // overview's column-fitting depends on this precise behavior.
        assert_eq!(truncate("hi", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn pluralize_count_one_is_singular() {
        assert_eq!(pluralize("note", 1), "note");
        assert_eq!(pluralize("suggestion", 1), "suggestion");
    }

    #[test]
    fn pluralize_count_zero_is_plural() {
        // We only ever call pluralize with count > 0 in practice (we skip the
        // span when the count is zero), but the rule "anything other than 1
        // is plural" is the safer default.
        assert_eq!(pluralize("note", 0), "notes");
    }

    #[test]
    fn pluralize_count_two_is_plural() {
        assert_eq!(pluralize("note", 2), "notes");
        assert_eq!(pluralize("suggestion", 3), "suggestions");
    }

    #[test]
    fn confirm_response_accepts_y() {
        assert!(confirm_response("y"));
        assert!(confirm_response("Y"));
    }

    #[test]
    fn confirm_response_accepts_yes() {
        assert!(confirm_response("yes"));
        assert!(confirm_response("YES"));
        assert!(confirm_response("Yes"));
    }

    #[test]
    fn confirm_response_rejects_empty() {
        assert!(!confirm_response(""));
    }

    #[test]
    fn confirm_response_rejects_no() {
        assert!(!confirm_response("n"));
        assert!(!confirm_response("no"));
    }

    #[test]
    fn confirm_response_rejects_anything_else() {
        assert!(!confirm_response("nope"));
        assert!(!confirm_response("sure"));
        assert!(!confirm_response("1"));
    }

    #[test]
    fn atomic_write_bytes_writes_payload_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("subdir");
        let target = nested.join("out.txt");
        atomic_write_bytes(&nested, &target, b"hello").unwrap();
        let read = std::fs::read(&target).unwrap();
        assert_eq!(read, b"hello");
    }

    #[test]
    fn atomic_write_bytes_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        std::fs::write(&target, b"old").unwrap();
        atomic_write_bytes(dir.path(), &target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_bytes_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        atomic_write_bytes(dir.path(), &target, b"x").unwrap();
        let extras: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "out.txt")
            .collect();
        assert!(extras.is_empty(), "stray files: {extras:?}");
    }

    #[test]
    fn confirm_response_trims_surrounding_whitespace() {
        assert!(confirm_response("  y  "));
        assert!(confirm_response("  yes\n"));
        assert!(!confirm_response("  n  "));
    }

    #[test]
    fn find_repo_root_returns_dir_with_dot_jj() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".jj")).unwrap();
        let resolved = find_repo_root_from(dir.path()).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_repo_root_walks_up_from_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".jj")).unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let resolved = find_repo_root_from(&sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_repo_root_walks_up_through_multiple_levels() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".jj")).unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        let resolved = find_repo_root_from(&deep).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_repo_root_returns_error_when_no_jj_found() {
        // Anchor the search at a tempdir whose ancestors (the system temp
        // root) do not contain a `.jj/`. If the test host happened to have a
        // `.jj` somewhere above /tmp the walk-up would still terminate at
        // `/`, but in any sane CI/dev environment this exits with the
        // expected error.
        let dir = tempfile::tempdir().unwrap();
        let result = find_repo_root_from(dir.path());
        match result {
            Err(crate::error::JjrError::NotInJjRepo { cwd }) => {
                assert_eq!(
                    std::fs::canonicalize(&cwd).unwrap(),
                    std::fs::canonicalize(dir.path()).unwrap()
                );
            }
            other => panic!("expected NotInJjRepo, got {other:?}"),
        }
    }

    #[test]
    fn global_config_path_uses_jjr_config_path_override_first() {
        let _lock = crate::test_helpers::env_lock();
        let custom = std::path::PathBuf::from("/explicit/jjr-config.toml");
        let _override = crate::test_helpers::EnvGuard::set_path("JJR_CONFIG_PATH", &custom);
        // XDG and HOME should be ignored when JJR_CONFIG_PATH is set.
        let _xdg = crate::test_helpers::EnvGuard::set_path(
            "XDG_CONFIG_HOME",
            std::path::Path::new("/some/xdg"),
        );
        let _home =
            crate::test_helpers::EnvGuard::set_path("HOME", std::path::Path::new("/some/home"));
        assert_eq!(global_config_path(), Some(custom));
    }

    #[test]
    fn global_config_path_uses_xdg_when_set() {
        let _lock = crate::test_helpers::env_lock();
        let _override = crate::test_helpers::EnvGuard::unset("JJR_CONFIG_PATH");
        let _xdg = crate::test_helpers::EnvGuard::set_path(
            "XDG_CONFIG_HOME",
            std::path::Path::new("/x/config"),
        );
        let _home =
            crate::test_helpers::EnvGuard::set_path("HOME", std::path::Path::new("/should/ignore"));
        assert_eq!(
            global_config_path(),
            Some(std::path::PathBuf::from("/x/config/jjr/config.toml"))
        );
    }

    #[test]
    fn global_config_path_falls_back_to_dot_config_when_xdg_unset() {
        let _lock = crate::test_helpers::env_lock();
        let _override = crate::test_helpers::EnvGuard::unset("JJR_CONFIG_PATH");
        let _xdg = crate::test_helpers::EnvGuard::unset("XDG_CONFIG_HOME");
        let _home = crate::test_helpers::EnvGuard::set_path("HOME", std::path::Path::new("/u/me"));
        assert_eq!(
            global_config_path(),
            Some(std::path::PathBuf::from("/u/me/.config/jjr/config.toml"))
        );
    }

    #[test]
    fn global_config_path_returns_none_when_no_home_or_xdg() {
        let _lock = crate::test_helpers::env_lock();
        let _override = crate::test_helpers::EnvGuard::unset("JJR_CONFIG_PATH");
        let _xdg = crate::test_helpers::EnvGuard::unset("XDG_CONFIG_HOME");
        let _home = crate::test_helpers::EnvGuard::unset("HOME");
        assert_eq!(global_config_path(), None);
    }

    #[test]
    fn global_config_path_treats_empty_xdg_as_unset() {
        let _lock = crate::test_helpers::env_lock();
        let _override = crate::test_helpers::EnvGuard::unset("JJR_CONFIG_PATH");
        let _xdg = crate::test_helpers::EnvGuard::set_str("XDG_CONFIG_HOME", "");
        let _home = crate::test_helpers::EnvGuard::set_path("HOME", std::path::Path::new("/h"));
        assert_eq!(
            global_config_path(),
            Some(std::path::PathBuf::from("/h/.config/jjr/config.toml"))
        );
    }

    #[test]
    fn find_repo_root_uses_dot_jj_directory_not_file() {
        // jj itself only treats `.jj/` as a repo marker — a regular file
        // named `.jj` should NOT match. We pin that behavior here so a
        // regression to `.exists()` (which would match files too) is caught.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".jj"), b"not a directory").unwrap();
        let result = find_repo_root_from(dir.path());
        assert!(
            matches!(result, Err(crate::error::JjrError::NotInJjRepo { .. })),
            "expected NotInJjRepo when .jj is a file, got {result:?}"
        );
    }
}
