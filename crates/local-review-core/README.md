# local-review-core

Shared core for [`jjr`](https://crates.io/crates/jjr) and
[`ggr`](https://crates.io/crates/ggr) — the two binaries in the
[local-review](https://github.com/ericbmerritt/local-review) workspace.

This crate is published for dependency-resolution reasons only. It is not a
general-purpose library and has no stability guarantees outside the binaries
that consume it. Pin to an exact version if you depend on it directly.

## What's in here

- `diff` — unified-diff parser.
- `anchoring` — fuzzy comment re-anchoring across mutating diffs.
- `comment`, `change_id`, `severity`, `error` — shared data types.
- `revset_hash` — stable hash of a jj revset expression.
- `tui` — `ratatui`-based TUI framework parameterised by a `ReviewSurface` trait
  that each binary implements.

For the actual review tools, see the workspace README.

## License

Dual-licensed under MIT or Apache-2.0, matching the workspace.
