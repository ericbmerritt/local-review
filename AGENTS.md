# Agent Instructions

This is a Rust CLI tool wrapping a TUI for self-review of jj stacks before they
become PRs. No database, no network I/O, no async.

## Mental model

The mental model lives in `specs/local-stack-review-edd.md`. Read the
**Principles** section first; it constrains every implementation decision.
Notably:

- The tool is for jj users. Claude edits each change in place. Don't
  accommodate git-style append-fix-commit instincts.
- The review cycle is the unit of progress.
- Claude addresses comments by editing code. The codebase change is the only
  reply — no decline-with-reasoning, no summary report.
- The jj revset is the source of truth. Orphaned comments persist on disk but
  are not surfaced.
- The tool does not model "done."

## Code layout

Functional core in the middle, imperative shell at the edges. Pure modules
take data and return data — no IO, no subprocess, no clock. Each `.rs` file
in `src/` carries a module-level doc comment summarising its role; read those
rather than relying on a list here.

See `specs/jjr-mvp.ladder.md` for current scope and what is still planned.

Layout is flat (`mod_module_files = "deny"`). `tui.rs` is the only module with
a same-named subdirectory (`tui/`) because the TUI is genuinely multi-file.

## Quality posture

Strict clippy + rustc lints in `Cargo.toml [lints.*]`. Notable denies:

- `unwrap_used`, `expect_used` — no Result shortcuts. Errors flow through
  `Result<T, JjrError>` everywhere via `snafu`.
- `print_stdout`, `print_stderr` — use `writeln!` against an explicit
  `std::io::stderr().lock()` handle.
- `as_conversions` — no `as` casts. `TryFrom`/`From` only.
- `unsafe_code`, `dead_code`, `unreachable_pub`.

Tests are exempted from `unwrap`/`expect`/`dbg`/`print` denies (see
`clippy.toml`).

## Specs

The `specs/` directory contains the engineering design document, the TUI
design, and the milestone ladder. Read the EDD and TUI design before
implementing anything non-trivial.

## Workflow

`pgc` is the ladder management tool used in this monorepo. If `pgc` is not on
PATH, refer to `specs/jjr-mvp.ladder.md` directly for milestone progress.
