//! `ggr` — local terminal review surface for GitHub pull requests.
//!
//! Scaffold only. Walks commits within a PR using the same anchored-
//! comment ergonomics as `jjr`, with `gh` as the source of truth in
//! place of `jj`. No functionality yet — this binary exists so the
//! workspace topology is in place before code migration begins.

use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    writeln!(
        stderr,
        "ggr {} — scaffold only, not yet functional",
        env!("CARGO_PKG_VERSION")
    )
}
