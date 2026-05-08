use std::path::{Path, PathBuf};

use crate::change_id::ChangeId;
use crate::error::Result;
use crate::jj;
use crate::util::log_warning;

/// RAII guard that restores the jj working copy to its prior position on drop.
///
/// Construction captures `@`, then switches the working copy to `target`. On
/// drop, it runs `jj edit <prior>` to restore the original position.
///
/// Restore failures are reported to stderr but never panic.
pub struct WorkingCopyGuard {
    prior_change: ChangeId,
    repo_root: PathBuf,
}

impl WorkingCopyGuard {
    /// Capture `@`, move the working copy to `target`, and return a guard that
    /// restores `@` on drop.
    ///
    /// If `jj edit <target>` fails, this returns an error and the working copy
    /// remains at its original position.
    pub fn enter(repo_root: &Path, target: &ChangeId) -> Result<Self> {
        let prior = jj::current_change(repo_root)?;

        jj::edit(repo_root, target)?;

        Ok(Self {
            prior_change: prior,
            repo_root: repo_root.to_owned(),
        })
    }
}

impl Drop for WorkingCopyGuard {
    fn drop(&mut self) {
        if let Err(e) = jj::edit(&self.repo_root, &self.prior_change) {
            // Concatenate the warning + remediation hint into a single
            // `\n`-separated message so the whole crash diagnostic stays in
            // one log entry. The remediation line is indented to mirror the
            // prior two-writeln visual layout.
            let prior = self.prior_change.as_str();
            log_warning(&format!(
                "failed to restore working copy to {prior}: {e}\n\
                 \x20        run `jj edit {prior}` manually to restore your position"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use super::*;

    fn jj_on_path() -> bool {
        StdCommand::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn build_fixture(tmp: &tempfile::TempDir) -> PathBuf {
        build_fixture_named(tmp, "single_change.sh")
    }

    fn build_fixture_named(tmp: &tempfile::TempDir, fixture: &str) -> PathBuf {
        let repo = tmp.path().join("repo");
        let script = std::env::current_dir()
            .unwrap()
            .join("tests")
            .join("fixtures")
            .join(fixture);
        let status = StdCommand::new("bash")
            .arg(&script)
            .arg(&repo)
            .status()
            .unwrap();
        assert!(status.success(), "fixture script failed");
        repo
    }

    /// Return change IDs in `trunk()..@` order (oldest first).
    fn stack_change_ids(repo: &Path) -> Vec<String> {
        let out = StdCommand::new("jj")
            .args([
                "--repository",
                repo.to_str().unwrap_or("."),
                "log",
                "-r",
                "trunk()..@",
                "--reversed",
                "--no-graph",
                "--color=never",
                "-T",
                r#"change_id ++ "\n""#,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "jj log failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn current_change_in(repo: &Path) -> String {
        jj::current_change(repo).unwrap().as_str().to_owned()
    }

    #[test]
    fn guard_enter_and_drop_restores_at() {
        if !jj_on_path() {
            eprintln!("jj not on PATH; skipping guard_enter_and_drop_restores_at");
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = build_fixture(&tmp);

        let original_id = current_change_in(&repo);
        let target = ChangeId::parse(&original_id).unwrap();

        {
            let _guard = WorkingCopyGuard::enter(&repo, &target).unwrap();
            let during = current_change_in(&repo);
            assert_eq!(during, original_id, "guard should be at target");
        }

        let after = current_change_in(&repo);
        assert_eq!(after, original_id, "drop must restore to original @");
    }

    #[test]
    fn guard_enter_same_change_still_works() {
        if !jj_on_path() {
            eprintln!("jj not on PATH; skipping guard_enter_same_change_still_works");
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = build_fixture(&tmp);

        let original_id = current_change_in(&repo);
        let target = ChangeId::parse(&original_id).unwrap();

        let guard = WorkingCopyGuard::enter(&repo, &target).unwrap();
        drop(guard);

        let after = current_change_in(&repo);
        assert_eq!(after, original_id, "@ must be unchanged after no-op guard");
    }

    #[test]
    fn guard_enter_invalid_target_returns_error_at_unchanged() {
        if !jj_on_path() {
            eprintln!(
                "jj not on PATH; skipping guard_enter_invalid_target_returns_error_at_unchanged"
            );
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = build_fixture(&tmp);

        let original_id = current_change_in(&repo);
        let bad_target = ChangeId::parse("aaaabbbbcccc1234").unwrap();

        let result = WorkingCopyGuard::enter(&repo, &bad_target);
        assert!(result.is_err(), "expected error for nonexistent target");

        let after = current_change_in(&repo);
        assert_eq!(after, original_id, "@ must be unchanged after failed enter");
    }

    #[test]
    fn guard_drop_on_panic_restores_at() {
        if !jj_on_path() {
            eprintln!("jj not on PATH; skipping guard_drop_on_panic_restores_at");
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = build_fixture(&tmp);

        let original_id = current_change_in(&repo);
        let target = ChangeId::parse(&original_id).unwrap();

        let repo_clone = repo.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = WorkingCopyGuard::enter(&repo_clone, &target).unwrap();
            panic!("intentional panic to test Drop");
        });

        assert!(result.is_err(), "panic should have been caught");

        let after = current_change_in(&repo);
        assert_eq!(after, original_id, "@ must be restored after panic unwind");
    }

    /// Pin the actual move-and-restore cycle: with @ at one change and the
    /// guard targeting a different change, @ moves on enter and returns on
    /// drop. The same-change tests above can't catch a regression where
    /// `enter` silently no-ops.
    #[test]
    fn guard_enter_moves_at_to_target_and_drop_restores() {
        if !jj_on_path() {
            eprintln!("jj not on PATH; skipping guard_enter_moves_at_to_target_and_drop_restores");
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = build_fixture_named(&tmp, "three_change_stack.sh");

        let ids = stack_change_ids(&repo);
        assert!(
            ids.len() >= 2,
            "fixture must produce >= 2 changes; got: {ids:?}"
        );

        // @ ends up on the youngest change (last in the reversed log).
        let original_id = current_change_in(&repo);
        let target_id = ids.first().expect("at least one entry").clone();
        assert_ne!(
            target_id, original_id,
            "test requires distinct prior/target changes"
        );

        let target = ChangeId::parse(&target_id).unwrap();

        {
            let _guard = WorkingCopyGuard::enter(&repo, &target).unwrap();
            let during = current_change_in(&repo);
            assert_eq!(during, target_id, "guard must move @ to target");
        }

        let after = current_change_in(&repo);
        assert_eq!(after, original_id, "drop must restore @ to prior change");
    }
}
