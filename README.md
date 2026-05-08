# local-review

Workspace housing two local-first batched code-review tools that share a common
core.

| Crate                                                             | Binary | Status   | What it reviews                   |
| ----------------------------------------------------------------- | ------ | -------- | --------------------------------- |
| [`crates/jjr`](crates/jjr/README.md)                              | `jjr`  | shipped  | local jj stacks, pre-PR           |
| [`crates/ggr`](crates/ggr/Cargo.toml)                             | `ggr`  | scaffold | GitHub pull requests, by commit   |
| [`crates/local-review-core`](crates/local-review-core/src/lib.rs) | (lib)  | scaffold | shared diff/anchoring/storage/TUI |

The two tools wear the same UX — oldest-first walk through changes, anchored
inline comments, persistent local draft, batched submit — over different sources
of truth (`jj` for `jjr`, `gh` for `ggr`).

## Layout

- `crates/local-review-core/` — shared library. Currently a stub; code will
  migrate from `jjr` over follow-up commits.
- `crates/jjr/` — jj-stack review surface. Functional and shipping; see its
  [README](crates/jjr/README.md).
- `crates/ggr/` — GitHub-PR review surface. Scaffold only; not yet functional.

## Development

```sh
cargo build --workspace
just validate
```

`jjr` is the only crate with a release pipeline today (auto-tag on
`crates/jjr/Cargo.toml` version bump, then publish to crates.io and bump the
Homebrew formula).
