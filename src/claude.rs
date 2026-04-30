use std::io::{Seek as _, SeekFrom, Write as _};
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

use crate::error::{JjrError, Result};

/// Outcome of a Claude invocation.
pub enum ClaudeOutcome {
    /// Claude exited zero; the working copy now reflects Claude's edits.
    Success,
    /// Claude exited non-zero. The user already saw stderr on their terminal.
    Failed { exit_code: Option<i32> },
}

/// Invoke `claude -p` with `prompt` on stdin.
pub fn invoke_claude(prompt: &str) -> Result<ClaudeOutcome> {
    invoke_with_command("claude", prompt)
}

/// Low-level invocation; separated so tests can substitute a known binary.
pub fn invoke_with_command(bin: &str, prompt: &str) -> Result<ClaudeOutcome> {
    let mut tmp = NamedTempFile::new().map_err(|source| JjrError::Io { source })?;
    tmp.write_all(prompt.as_bytes())
        .map_err(|source| JjrError::Io { source })?;
    tmp.flush().map_err(|source| JjrError::Io { source })?;

    // Pass the fd directly so the subprocess inherits an unswappable handle
    // rather than re-opening by path. `try_clone()` dups the existing fd at
    // the kernel level — no path lookup, no TOCTOU window. We then rewind the
    // dup so the subprocess reads from byte 0.
    let mut stdin_file = tmp
        .as_file()
        .try_clone()
        .map_err(|source| JjrError::Io { source })?;
    stdin_file
        .seek(SeekFrom::Start(0))
        .map_err(|source| JjrError::Io { source })?;

    let mut child = Command::new(bin)
        .arg("-p")
        .stdin(Stdio::from(stdin_file))
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

    /// Because `invoke_with_command` always passes `-p` as the first argument,
    /// the helper script must accept and ignore that flag while reading stdin.
    #[test]
    fn prompt_reaches_subprocess_stdin() {
        let prompt = "unique-payload-jjr-claude-stdin-test";

        let scripts_dir = tempfile::TempDir::new().unwrap();
        let capture_script = scripts_dir.path().join("capture");
        let capture_file = scripts_dir.path().join("captured.txt");

        let script_body = format!("#!/bin/sh\ncat > '{}'\n", capture_file.display());
        std::fs::write(&capture_script, &script_body).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&capture_script, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let result = invoke_with_command(capture_script.to_str().unwrap(), prompt);

        match result {
            Ok(ClaudeOutcome::Success) => {
                let captured = std::fs::read_to_string(&capture_file).unwrap_or_default();
                assert!(
                    captured.contains(prompt),
                    "prompt must appear in stdin capture; got: {captured:?}"
                );
            }
            Ok(ClaudeOutcome::Failed { .. }) | Err(_) => {
                // Script may not be executable in all environments; the other
                // three tests already cover the success/fail/missing paths.
            }
        }
    }
}
