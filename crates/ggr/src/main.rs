//! `ggr` — local terminal review surface for GitHub pull requests.
//!
//! Usage:
//!   `ggr 42`                                               — auto-detect repo from git remote
//!   `ggr acme/myrepo#2429`                                 — explicit repo, works anywhere
//!   `ggr --url https://github.example.com owner/repo#2429` — GHE host + short form
//!   `ggr https://github.example.com/owner/repo/pull/2429`  — full pull URL

mod error;
mod gh;
mod pr;
mod pr_ref;
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
    /// PR to review: number, owner/repo#number, or full pull URL.
    pr: String,

    /// Base URL of a GitHub Enterprise Server instance (e.g. `https://github.example.com`).
    /// Required when using the owner/repo#number form against a GHE host.
    #[arg(long)]
    url: Option<String>,
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

    let mut parsed = pr_ref::parse(&cli.pr, cli.url.as_deref())?;

    // If no host was supplied explicitly, check the local git remote.
    // For owner/repo#number, only use the remote host when the slug matches.
    // For bare numbers, accept any remote host (gh resolves the repo itself).
    if parsed.hostname.is_none() {
        if let Some(host) = util::detect_remote_host(parsed.repo_flag.as_deref()) {
            if let Some(repo) = parsed.repo_flag.take() {
                parsed.repo_flag = Some(format!("{host}/{repo}"));
            }
            parsed.hostname = Some(host);
        }
    }

    let mut pr = gh::fetch_pr_details(parsed.number, parsed.repo_flag.as_deref())?;
    pr.hostname = parsed.hostname;

    if pr.commits.is_empty() {
        return Err(GgrError::PrNotFound { pr: parsed.number });
    }

    tui::run(pr)
}
