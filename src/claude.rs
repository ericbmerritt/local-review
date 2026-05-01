use std::process::{Command, Stdio};

use crate::error::{JjrError, Result};

/// Maximum prompt size passable through argv.
///
/// macOS `ARG_MAX` is 1 MiB shared with the environment; Linux is typically
/// 2 MiB. 512 KiB leaves room for `environ` plus claude's own argv across
/// platforms. If a review packet exceeds this, the user gets a legible
/// `PromptTooLarge` error pointing them at chunking the stack — without the
/// guard, `spawn()` would surface a generic `E2BIG` mapped to `Io`.
pub const PROMPT_ARG_LIMIT: usize = 512 * 1024;

/// Outcome of a Claude invocation.
pub enum ClaudeOutcome {
    /// Claude exited zero; the working copy now reflects Claude's edits.
    Success,
    /// Claude exited non-zero. The user already saw stderr on their terminal.
    Failed { exit_code: Option<i32> },
}

/// Invoke `claude` interactively with `prompt` as the initial message.
///
/// Claude runs interactively (no `-p`) so it can prompt the user to approve
/// edits in real time. The caller (TUI or CLI) is responsible for handing
/// the terminal to Claude — the subprocess inherits stdin/stdout/stderr,
/// takes over the tty for the session, and returns control on exit.
pub fn invoke_claude(prompt: &str) -> Result<ClaudeOutcome> {
    invoke_with_command("claude", prompt)
}

/// Low-level invocation; separated so tests can substitute a known binary.
///
/// The prompt is passed as a positional argument after a `--` separator —
/// claude accepts it as the initial message and then uses the inherited
/// terminal for subsequent interactive I/O. The `--` is required so that a
/// prompt whose first character is `-` (e.g. a packet starting with
/// `--Description:`) isn't parsed as a flag by claude's argv parser.
///
/// SECURITY NOTE: the prompt is passed as argv, which is visible in process
/// listings (`ps`, `/proc/<pid>/cmdline`) to same-uid processes while claude
/// runs. Review-comment text and reviewer prose are exposed for the lifetime
/// of the subprocess. Do not embed secrets in review comments. (Pre-existing
/// trust boundary — the reviewer typed it on their own box — but worth being
/// explicit.)
///
/// Returns `PromptTooLarge` before spawning if `prompt` exceeds
/// [`PROMPT_ARG_LIMIT`], so the user sees an actionable message instead of
/// a generic `E2BIG` from the kernel.
pub fn invoke_with_command(bin: &str, prompt: &str) -> Result<ClaudeOutcome> {
    if prompt.len() > PROMPT_ARG_LIMIT {
        return Err(JjrError::PromptTooLarge {
            size: prompt.len(),
            limit: PROMPT_ARG_LIMIT,
        });
    }

    let mut child = Command::new(bin)
        .arg("--")
        .arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                JjrError::ClaudeMissing { source: e }
            } else {
                JjrError::Io { source: e }
            }
        })?;

    let status = child.wait().map_err(|source| JjrError::Io { source })?;

    if status.success() {
        Ok(ClaudeOutcome::Success)
    } else {
        Ok(ClaudeOutcome::Failed {
            exit_code: status.code(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn true_bin() -> &'static str {
        if std::path::Path::new("/usr/bin/true").exists() {
            "/usr/bin/true"
        } else {
            "/bin/true"
        }
    }

    fn false_bin() -> &'static str {
        if std::path::Path::new("/usr/bin/false").exists() {
            "/usr/bin/false"
        } else {
            "/bin/false"
        }
    }

    #[test]
    fn true_binary_returns_success() {
        let result = invoke_with_command(true_bin(), "hello").unwrap();
        assert!(matches!(result, ClaudeOutcome::Success));
    }

    #[test]
    fn false_binary_returns_failed_with_exit_code_1() {
        let result = invoke_with_command(false_bin(), "hello").unwrap();
        assert!(
            matches!(result, ClaudeOutcome::Failed { exit_code: Some(1) }),
            "expected Failed {{ exit_code: Some(1) }}"
        );
    }

    #[test]
    fn nonexistent_binary_returns_claude_missing() {
        let result = invoke_with_command("/nonexistent/binary/jjr_test_probe_xyz", "hello");
        assert!(
            matches!(result, Err(JjrError::ClaudeMissing { .. })),
            "expected ClaudeMissing error"
        );
    }

    /// Writes a `/bin/sh` script that captures every argv entry on its own
    /// line into `out_path`, marks it executable on Unix, and returns its
    /// path. Used by the argv-shape regression tests.
    fn write_argv_capture_script(
        scripts_dir: &std::path::Path,
        out_path: &std::path::Path,
    ) -> std::path::PathBuf {
        let capture_script = scripts_dir.join("capture-args");
        let script_body = format!(
            "#!/bin/sh\n{{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }} > '{}'\n",
            out_path.display()
        );
        std::fs::write(&capture_script, &script_body).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&capture_script, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        capture_script
    }

    /// The prompt must reach the subprocess as the positional argument
    /// AFTER a `--` separator (interactive mode: claude treats the first
    /// positional after `--` as the initial message and then uses the
    /// inherited tty for subsequent I/O).
    #[test]
    fn prompt_reaches_subprocess_after_separator() {
        let prompt = "unique-payload-jjr-claude-arg-test";

        let scripts_dir = tempfile::TempDir::new().unwrap();
        let capture_file = scripts_dir.path().join("argv.txt");
        let capture_script = write_argv_capture_script(scripts_dir.path(), &capture_file);

        let result = invoke_with_command(capture_script.to_str().unwrap(), prompt);

        match result {
            Ok(ClaudeOutcome::Success) => {
                let captured = std::fs::read_to_string(&capture_file).unwrap_or_default();
                let argv: Vec<&str> = captured.lines().collect();
                assert_eq!(
                    argv,
                    vec!["--", prompt],
                    "argv must be [\"--\", prompt]; got: {captured:?}"
                );
            }
            Ok(ClaudeOutcome::Failed { .. }) | Err(_) => {
                // Script may not be executable in all environments; the other
                // tests already cover the success/fail/missing paths.
            }
        }
    }

    /// Regression: claude must NOT be invoked with `-p` (non-interactive
    /// print mode). With `-p`, claude can't prompt the user to approve
    /// tool calls and bails with "permission denied" in environments whose
    /// policy requires explicit approval per write. Interactive mode lets
    /// the user approve edits in real time.
    #[test]
    fn invoke_with_command_does_not_pass_p_flag() {
        let prompt = "unique-payload-jjr-no-p-flag";

        let scripts_dir = tempfile::TempDir::new().unwrap();
        let capture_file = scripts_dir.path().join("argv.txt");
        let capture_script = write_argv_capture_script(scripts_dir.path(), &capture_file);

        let result = invoke_with_command(capture_script.to_str().unwrap(), prompt);

        match result {
            Ok(ClaudeOutcome::Success) => {
                let captured = std::fs::read_to_string(&capture_file).unwrap_or_default();
                assert!(
                    !captured
                        .lines()
                        .any(|line| line == "-p" || line == "--print"),
                    "argv must not contain `-p` / `--print`; got: {captured:?}"
                );
            }
            Ok(ClaudeOutcome::Failed { .. }) | Err(_) => {
                // Script may not be executable in all environments; the other
                // tests already cover the success/fail/missing paths.
            }
        }
    }

    /// T1: empty prompt must not panic and must spawn cleanly through the
    /// guard. `/bin/true` ignores argv, so we get `Success` back; the
    /// important guarantee is that an empty `&str` does not cause an
    /// early return or panic. claude's behavior on empty argv is its concern.
    #[test]
    fn invoke_with_command_handles_empty_prompt() {
        let result = invoke_with_command(true_bin(), "").unwrap();
        assert!(
            matches!(result, ClaudeOutcome::Success),
            "expected Success for empty prompt"
        );
    }

    /// T2: a prompt that LOOKS like a flag (e.g. `--help`, `-p`) must be
    /// passed through as the prompt text, not interpreted as a flag by
    /// claude's argv parser. The `--` separator guarantees this.
    #[test]
    fn invoke_with_command_passes_flag_like_prompt_after_separator() {
        for prompt in ["--help", "-p", "--print", "--version"] {
            let scripts_dir = tempfile::TempDir::new().unwrap();
            let capture_file = scripts_dir.path().join("argv.txt");
            let capture_script = write_argv_capture_script(scripts_dir.path(), &capture_file);

            let result = invoke_with_command(capture_script.to_str().unwrap(), prompt);

            match result {
                Ok(ClaudeOutcome::Success) => {
                    let captured = std::fs::read_to_string(&capture_file).unwrap_or_default();
                    let argv: Vec<&str> = captured.lines().collect();
                    assert_eq!(
                        argv,
                        vec!["--", prompt],
                        "flag-like prompt {prompt:?} must arrive as positional after `--`; got: {captured:?}"
                    );
                }
                Ok(ClaudeOutcome::Failed { .. }) | Err(_) => {
                    // Script may not be executable in all environments.
                }
            }
        }
    }

    /// T3: oversize prompt returns `PromptTooLarge` before spawning so the
    /// user gets an actionable error instead of a generic `E2BIG`. Verifies
    /// both the variant and that `size`/`limit` carry the actual byte counts.
    #[test]
    fn invoke_with_command_returns_prompt_too_large_for_oversize_input() {
        let oversize = "x".repeat(PROMPT_ARG_LIMIT + 1);
        let result = invoke_with_command(true_bin(), &oversize);

        let Err(JjrError::PromptTooLarge { size, limit }) = result else {
            panic!("expected PromptTooLarge for oversize prompt");
        };
        assert_eq!(size, PROMPT_ARG_LIMIT + 1);
        assert_eq!(limit, PROMPT_ARG_LIMIT);
    }

    /// T4: prompt at exactly the limit is accepted (boundary inclusive).
    /// The guard rejects only `> limit`, not `>= limit`.
    #[test]
    fn invoke_with_command_accepts_prompt_at_limit() {
        let at_limit = "x".repeat(PROMPT_ARG_LIMIT);
        let result = invoke_with_command(true_bin(), &at_limit);
        assert!(
            !matches!(result, Err(JjrError::PromptTooLarge { .. })),
            "prompt of exactly PROMPT_ARG_LIMIT bytes must NOT trigger PromptTooLarge"
        );
    }
}
