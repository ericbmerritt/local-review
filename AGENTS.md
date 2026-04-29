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
take data and return data — no IO, no subprocess, no clock:

Phase 1 modules (current):

- `src/change_id.rs` — ChangeId/CommitId value types with validated parse
- `src/diff.rs` — unified diff parser (pure)
- `src/error.rs` — JjrError enum
- `src/jj.rs` — subprocess wrapper (shell)
- `src/lib.rs` — crate root
- `src/main.rs` — CLI entry point (shell)
- `src/tui.rs` — terminal UI event loop (shell)
- `src/tui/diff_view.rs` — diff rendering (pure)
- `src/tui/help_screen.rs` — help overlay (pure)
- `src/util.rs` — shared pure utilities (clamp, page_size, truncate)

Future modules (planned in later phases):

- `src/comment.rs`, `src/anchoring.rs`, `src/packet.rs` — comment model and
  re-anchoring algorithm (pure)
- `src/stack.rs` — stack/revset resolution (pure)
- `src/claude.rs`, `src/store.rs`, `src/cursor.rs`, `src/config.rs` — shell
  layer for Claude handoff, comment storage, cursor, config

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

- `specs/local-stack-review-edd.md` — domain model, comment scopes, prompt
  format, anchoring algorithm
- `specs/jjr-tui-design.md` — seven screens, keybind grammar, resize behavior
- `specs/jjr-mvp.ladder.md` — phase plan; each phase is a vertical slice

Read both specs before implementing anything non-trivial.

## Workflow

`pgc` is the ladder management tool used in this monorepo. If `pgc` is not on
PATH, refer to `specs/jjr-mvp.ladder.md` directly for phase status.
