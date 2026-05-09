//! `ggr` — local terminal review surface for GitHub pull requests.
//!
//! Usage: `ggr <pr-number>`
//!
//! Fetches the PR commit list via `gh`, displays each commit's diff in a TUI,
//! and lets the reviewer navigate commits and files with the keyboard.

mod error;
mod gh;
mod pr;
mod tui;
mod util;

use std::io::Write as _;
use std::process::ExitCode;

use clap::Parser;

use error::GgrError;

#[derive(Parser)]
#[command(
    name = "ggr",
    about = "Local terminal review surface for GitHub pull requests"
)]
struct Cli {
    /// Pull request number to review.
    pr: u64,
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

    let repo_root = util::find_git_root()?;
    let pr = gh::fetch_pr_details(cli.pr)?;

    if pr.commits.is_empty() {
        return Err(GgrError::PrNotFound { pr: cli.pr });
    }

    let initial_diff = gh::fetch_commit_diff(&repo_root, &pr.commits[0].sha)?;

    tui::run(pr, initial_diff, repo_root)
}
