use std::process::{Command, Stdio};

use crate::agent_config::{load_agent_config, AgentConfig};
use crate::error::{JjrError, Result};

/// Maximum combined size (`prompt` + `extra_args`) passable through argv.
///
/// macOS `ARG_MAX` is 1 MiB shared with the environment; Linux is typically
/// 2 MiB. 512 KiB leaves room for `environ` plus the agent's own argv across
/// platforms.
pub const PROMPT_ARG_LIMIT: usize = 512 * 1024;

pub enum ClaudeOutcome {
    Success {
        tool: String,
    },
    Failed {
        tool: String,
        exit_code: Option<i32>,
    },
}

/// Invoke the configured agent CLI interactively with `prompt` as the initial
/// message. Loads `[agent]` from the global `jjr` config file (see
/// [`crate::util::global_config_path`]); falls back to `claude` with no extra
/// args when the config is missing or malformed.
///
/// The subprocess inherits stdin/stdout/stderr; the caller is responsible for
/// suspending any TUI before the call.
pub fn invoke_claude(prompt: &str) -> Result<ClaudeOutcome> {
    let config = load_agent_config();
    invoke_with_config(&config, prompt)
}

/// Invoke `config.tool` with `config.extra_args` followed by `--` and the
/// `prompt`. Crate-internal so the loader stays the only public surface.
pub(crate) fn invoke_with_config(config: &AgentConfig, prompt: &str) -> Result<ClaudeOutcome> {
    invoke_with_command(&config.tool, &config.extra_args, prompt)
}

/// Spawn `bin` with `extra_args` followed by `--` and `prompt`.
///
/// `extra_args` go BEFORE the `--` separator so the agent CLI parses them as
/// flags. The `--` is required so a prompt whose first character is `-`
/// isn't parsed as a flag itself.
///
/// Returns `PromptTooLarge` before spawning if the combined argv (`prompt` +
/// `extra_args` + per-arg overhead) exceeds [`PROMPT_ARG_LIMIT`], so the user
/// gets an actionable error instead of a generic kernel `E2BIG`.
///
/// SECURITY: argv is visible in process listings to same-uid processes;
/// review-comment text is exposed for the lifetime of the subprocess.
pub(crate) fn invoke_with_command(
    bin: &str,
    extra_args: &[String],
    prompt: &str,
) -> Result<ClaudeOutcome> {
    let combined = combined_argv_size(extra_args, prompt);
    if combined > PROMPT_ARG_LIMIT {
        return Err(JjrError::PromptTooLarge {
            size: combined,
            limit: PROMPT_ARG_LIMIT,
        });
    }

    let mut child = Command::new(bin)
        .args(extra_args)
        .arg("--")
        .arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                JjrError::AgentMissing {
                    tool: bin.to_owned(),
                    source: e,
                }
            } else {
                JjrError::Io { source: e }
            }
        })?;

    let status = child.wait().map_err(|source| JjrError::Io { source })?;

    if status.success() {
        Ok(ClaudeOutcome::Success {
            tool: bin.to_owned(),
        })
    } else {
        Ok(ClaudeOutcome::Failed {
            tool: bin.to_owned(),
            exit_code: status.code(),
        })
    }
}

/// Conservative argv-size estimate (bytes + per-arg NUL overhead) used to
/// gate `ARG_MAX` before spawn.
fn combined_argv_size(extra_args: &[String], prompt: &str) -> usize {
    let extras: usize = extra_args.iter().map(String::len).sum();
    prompt.len() + extras + extra_args.len()
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

    /// Run the capture script and return the captured argv. Returns `None`
    /// when the script can't execute (some CI sandboxes); callers treat that
    /// as skip.
    fn run_capture(extra_args: &[String], prompt: &str) -> Option<Vec<String>> {
        let scripts_dir = tempfile::TempDir::new().unwrap();
        let capture_file = scripts_dir.path().join("argv.txt");
        let capture_script = write_argv_capture_script(scripts_dir.path(), &capture_file);

        let result = invoke_with_command(capture_script.to_str().unwrap(), extra_args, prompt);
        match result {
            Ok(ClaudeOutcome::Success { .. }) => {
                let captured = std::fs::read_to_string(&capture_file).unwrap_or_default();
                Some(captured.lines().map(str::to_owned).collect())
            }
            Ok(ClaudeOutcome::Failed { .. }) | Err(_) => None,
        }
    }

    #[test]
    fn true_binary_returns_success() {
        let result = invoke_with_command(true_bin(), &[], "hello").unwrap();
        assert!(matches!(result, ClaudeOutcome::Success { .. }));
    }

    #[test]
    fn false_binary_returns_failed() {
        let result = invoke_with_command(false_bin(), &[], "hello").unwrap();
        let ClaudeOutcome::Failed { tool, exit_code } = result else {
            panic!("expected Failed");
        };
        assert_eq!(tool, false_bin());
        assert_eq!(exit_code, Some(1));
    }

    #[test]
    fn nonexistent_binary_returns_agent_missing() {
        let result = invoke_with_command("/nonexistent/binary/jjr_test_probe_xyz", &[], "hello");
        assert!(matches!(result, Err(JjrError::AgentMissing { .. })));
    }

    #[test]
    fn missing_configured_tool_reports_configured_name_not_claude() {
        let result =
            invoke_with_command("/nonexistent/binary/jjr_test_probe_opencode", &[], "hello");
        let Err(JjrError::AgentMissing { tool, .. }) = result else {
            panic!("expected AgentMissing");
        };
        assert_eq!(tool, "/nonexistent/binary/jjr_test_probe_opencode");
        // Display string must mention the configured tool, not "claude".
        let displayed = format!(
            "{}",
            JjrError::AgentMissing {
                tool: "opencode".to_owned(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }
        );
        assert!(displayed.contains("opencode"), "display: {displayed:?}");
        assert!(
            !displayed.contains("claude"),
            "display leaked claude: {displayed:?}"
        );
    }

    #[test]
    fn prompt_reaches_subprocess_after_separator() {
        let prompt = "unique-payload-jjr-claude-arg-test";
        let Some(argv) = run_capture(&[], prompt) else {
            return;
        };
        assert_eq!(argv, vec!["--".to_owned(), prompt.to_owned()]);
    }

    /// Regression: the agent must NOT be invoked with `-p` (non-interactive
    /// print mode). With `-p`, claude can't prompt the user to approve tool
    /// calls and bails with "permission denied" in environments whose policy
    /// requires explicit approval per write.
    #[test]
    fn invoke_with_command_does_not_pass_p_flag() {
        let Some(argv) = run_capture(&[], "unique-payload-jjr-no-p-flag") else {
            return;
        };
        assert!(!argv.iter().any(|a| a == "-p" || a == "--print"));
    }

    #[test]
    fn handles_empty_prompt() {
        let result = invoke_with_command(true_bin(), &[], "").unwrap();
        assert!(matches!(result, ClaudeOutcome::Success { .. }));
    }

    #[test]
    fn flag_like_prompt_passes_through_after_separator() {
        for prompt in ["--help", "-p", "--print", "--version"] {
            let Some(argv) = run_capture(&[], prompt) else {
                continue;
            };
            assert_eq!(argv, vec!["--".to_owned(), prompt.to_owned()]);
        }
    }

    #[test]
    fn oversize_prompt_returns_prompt_too_large() {
        let oversize = "x".repeat(PROMPT_ARG_LIMIT + 1);
        let result = invoke_with_command(true_bin(), &[], &oversize);
        let Err(JjrError::PromptTooLarge { size, limit }) = result else {
            panic!("expected PromptTooLarge");
        };
        assert_eq!(size, PROMPT_ARG_LIMIT + 1);
        assert_eq!(limit, PROMPT_ARG_LIMIT);
    }

    #[test]
    fn prompt_at_limit_is_accepted() {
        let at_limit = "x".repeat(PROMPT_ARG_LIMIT);
        let result = invoke_with_command(true_bin(), &[], &at_limit);
        assert!(!matches!(result, Err(JjrError::PromptTooLarge { .. })));
    }

    /// Long `extra_args` must count toward the argv-size guard. Without this,
    /// a multi-megabyte `extra_args` payload would skip the guard and spawn
    /// would surface a generic `E2BIG`.
    #[test]
    fn extra_args_overflow_returns_prompt_too_large() {
        let big_arg = "x".repeat(PROMPT_ARG_LIMIT);
        let extras = vec![big_arg];
        let result = invoke_with_command(true_bin(), &extras, "hi");
        let Err(JjrError::PromptTooLarge { size, limit }) = result else {
            panic!("expected PromptTooLarge for oversize extra_args");
        };
        assert!(size > PROMPT_ARG_LIMIT);
        assert_eq!(limit, PROMPT_ARG_LIMIT);
    }

    #[test]
    fn extra_args_precede_separator() {
        let extras = vec!["--dangerously-skip-permissions".to_owned()];
        let Some(argv) = run_capture(&extras, "p") else {
            return;
        };
        assert_eq!(
            argv,
            vec![
                "--dangerously-skip-permissions".to_owned(),
                "--".to_owned(),
                "p".to_owned(),
            ]
        );
    }

    #[test]
    fn multiple_extra_args_pass_through_in_order() {
        let extras = vec!["--flag-one".to_owned(), "--flag-two=value".to_owned()];
        let Some(argv) = run_capture(&extras, "p") else {
            return;
        };
        assert_eq!(
            argv,
            vec![
                "--flag-one".to_owned(),
                "--flag-two=value".to_owned(),
                "--".to_owned(),
                "p".to_owned(),
            ]
        );
    }
}
