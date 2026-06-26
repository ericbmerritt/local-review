# local-review

Two terminal tools for batched code review. They share the same TUI, semantic
extraction engine, anchoring algorithm, and severity vocabulary.

| Tool  | Reviews                                 | Comments go to          | Install                         |
| ----- | --------------------------------------- | ----------------------- | ------------------------------- |
| `jjr` | Your local jj stack, before pushing     | Claude (edits the code) | `cargo install jjr` or Homebrew |
| `ggr` | A GitHub pull request, commit-by-commit | GitHub PR review API    | Homebrew                        |

## What these tools do

Both tools walk you through changes **one entity at a time**. A tree-sitter pass
extracts every changed function, method, class, struct, and similar construct,
then sorts them in **dependency-first order**: entities that others depend on
come first, so you review foundational code before the callers that use it.

You comment on specific lines, files, or the whole change. Comments accumulate
locally and never touch the network until you say so. When you're done, the tool
submits — to Claude (which edits the code) or to GitHub (which posts the
review).

The approach is intentionally batched. You read diffs, form judgments, record
them all, then send. No round-trips per comment. No context switching.

## The entity list

The primary view is a list of changed entities, not a flat file list:

```
  Δ authenticate()          src/auth/login.rs :42-78    fn · sig+body    8 callers   ●
  ⊕ UserToken               src/auth/token.rs :12-28    struct · added
  Δ session_parse()         src/db/session.rs :90-115   fn · body         2 callers
```

Columns scale with terminal width: name, file path, line range, caller count,
annotation. Sorted deepest-dependency-first by default; `o` toggles to file+line
order. Press `Enter` on any row to open a focused diff pre-scrolled to that
entity.

## Semantic extraction

13 languages — Rust, Python, Go, TypeScript, JavaScript, Java, Scala, Kotlin,
Bash, YAML, JSON, TOML, SQL — via tree-sitter. The extractor identifies
functions, methods, classes, structs, traits, database tables, config
properties, and more, and classifies each as added / modified / deleted / moved
with a change annotation (sig changed · body · sig+body).

## Install

```sh
# jjr
cargo install jjr
brew install ericbmerritt/jjr/jjr

# ggr
brew install ericbmerritt/jjr/ggr
```

Runtime dependencies: `jj` for `jjr`, `gh` for `ggr`.

## Development

```sh
git clone https://github.com/ericbmerritt/local-review
cd local-review
nix develop            # or direnv allow
just validate          # build + lint + test (90% coverage floor)
```

→ [`crates/jjr/README.md`](crates/jjr/README.md) — full jjr docs\
→ [`crates/ggr/README.md`](crates/ggr/README.md) — full ggr docs

## License

MIT OR Apache-2.0
