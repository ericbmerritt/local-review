# Agent Instructions

This is a Cargo workspace housing two local-first batched code-review tools that
share a common core:

- `crates/jjr/` — review surface for local jj stacks, pre-PR. Functional and
  shipping. No database, no network I/O, no async.
- `crates/ggr/` — review surface for GitHub pull requests, walked
  commit-by-commit. Scaffold only; not yet implemented. Will introduce network
  I/O at the gh-CLI shell boundary.
- `crates/local-review-core/` — shared library. Stub today; logic migrates from
  `jjr` over follow-up commits.

Default to working in `crates/jjr/` unless the task specifies otherwise.

## Mental model (jjr)

The mental model lives in `specs/local-stack-review-edd.md`. Read the
**Principles** section first; it constrains every implementation decision.
Notably:

- The tool is for jj users. Claude edits each change in place. Don't accommodate
  git-style append-fix-commit instincts.
- The review cycle is the unit of progress.
- Claude addresses comments by editing code. The codebase change is the only
  reply — no decline-with-reasoning, no summary report.
- The jj revset is the source of truth. Orphaned comments persist on disk but
  are not surfaced.
- The tool does not model "done."

## Code layout

Functional core in the middle, imperative shell at the edges. Pure modules take
data and return data — no IO, no subprocess, no clock. Each `.rs` file in
`crates/jjr/src/` carries a module-level doc comment summarising its role; read
those rather than relying on a list here.

See `specs/jjr-mvp.ladder.md` for current scope and what is still planned.

Layout is flat (`mod_module_files = "deny"`). `tui.rs` is the only module with a
same-named subdirectory (`tui/`) because the TUI is genuinely multi-file.

## Quality posture

Strict clippy + rustc lints in the workspace root
`Cargo.toml [workspace.lints.*]`, inherited by every crate via
`lints.workspace = true`. Notable denies:

- `unwrap_used`, `expect_used` — no Result shortcuts. Errors flow through
  `Result<T, JjrError>` everywhere via `snafu`.
- `print_stdout`, `print_stderr` — use `writeln!` against an explicit
  `std::io::stderr().lock()` handle.
- `as_conversions` — no `as` casts. `TryFrom`/`From` only.
- `unsafe_code`, `dead_code`, `unreachable_pub`.

Tests are exempted from `unwrap`/`expect`/`dbg`/`print` denies (see
`clippy.toml`).

## Specs

The `specs/` directory contains the engineering design document, the TUI design,
and the milestone ladder. Read the EDD and TUI design before implementing
anything non-trivial.

## Workflow

`pgc` is the ladder management tool used in this monorepo. If `pgc` is not on
PATH, refer to `specs/jjr-mvp.ladder.md` directly for milestone progress.

## Design defaults

These are settled project-level rules distilled from reviewer cycles. The
team-execution pipeline loads them at start so the panel doesn't re-litigate
them and the executor starts with them pre-loaded. Each rule carries a `Why:`
rationale and `Source:` cycle citations so future readers can judge whether the
rule still holds (and retire it if not).

- **Strip control characters at error-Display boundaries.** Any public error
  variant carrying a `String` derived from external input (file content, HTTP
  response body, CLI argument, env var, captured `stdout`/`stderr` from a
  subprocess) calls `strip_controls` at construction or at `Display`. This
  binary renders structured output to terminals; raw user-input echoing into
  stderr is an ANSI/OSC injection vector via hostile GitHub Enterprise hosts.
  **Why:** Repeated finding across multiple yelena cycles plus a 4-reviewer
  CONSENSUS at T2c16; the same fix shape recurs. **Source:** [T2 cycles 15-17,
  multiple yelena and CONSENSUS findings].
- **Comments justify their bytes by what the code can't say.** Comments that
  paraphrase the next line, restate the function name, or narrate code structure
  are deleted before commit. Comments explaining non-obvious cross-module
  behavior, external-system interaction (a third-party tool's defaults, an HTTP
  API's quirks), or intentional design tradeoffs (different layers'
  responsibilities) stay. When in doubt: ask whether a careful reader with only
  the code at this location (not the surrounding crate, not the external
  system's docs) can recover the comment's information. If yes, delete. If the
  comment names a coupling the code at this location can't show, keep it.
  **Why:** Thin-file modules with cross-module shared rules; the call-site
  comment is often the only documentation of the coupling. **Source:** Multiple
  T2 OVERRIDE entries — [T2/c15 OVERRIDE editor w2], [T2/c17 OVERRIDE editor
  w5/w6/w7/w8].
- **Sentinel state uses `Option<T>::take/replace`, not a typed sentinel.** When
  state has a "no real value yet, but the type slot needs to be present" stage,
  type the field as `Option<T>` and use `.take()` or `.replace()` to transition.
  Do not introduce a sentinel variant or a placeholder struct that has the same
  type as a real value but is semantically not-yet. The same shape applies to
  one-shot reply channels in actor systems (reeve converged on the same fix
  pattern). **Why:** Sentinel-via-typed-marker is a type lie that compiles and
  passes happy-path tests; `Option<T>` makes the state distinction structural
  and the transition atomic. **Source:** [T1.5/c8-c10 Trigger-1 thrash on
  `__Pending`, human escalation T1.5c10]; cross-codebase reinforcement from
  reeve (`SpawnRelay` resolution to `Option<Recipient<X>>::take()`).
- **Search adjacent files in the same crate before writing helpers.** Before
  writing any helper function, validation routine, mock actor, capturing
  collector, fixture writer, or shared utility, grep the surrounding crate (not
  just the same file or the deps) for similar patterns. Common patterns in this
  codebase: validation predicates (`valid_segment` vs `valid_hostname`),
  subprocess boilerplate (`gh` invocations across `pr.rs`/`gh.rs`), input
  sanitization (`strip_controls` duplicated across files), test scaffolding
  (mock actors, fixture writers — same pattern, different domain). If you find
  yourself writing the same 4+ line helper for the third time, factor it into
  the crate's appropriate module before committing. **Why:** Iris (dry-eye)
  catches this deterministically; the cost is one grep before writing.
  **Source:** [T2/c14 dry-eye CONSENSUS valid_segment/valid_hostname, dry-eye
  i:2 run_gh boilerplate], [T2/c16 dry-eye i1 strip_controls dup], [T1.5/c2
  dry-eye d1 SeverityHistogram dup].
- **`.len()` for string-character checks is a UTF-8 byte trap unless upstream is
  ASCII-restricted.** `s.len()` returns byte count; for character count, use
  `s.chars().count()`. `.len()` is correct _only_ when an upstream guard (e.g.
  `s.chars().all(is_ascii)`) limits input to ASCII bytes; in that case `.len()`
  is equivalent and faster. When the input is unrestricted user text,
  `chars().count()` is required. **Why:** Multiple cycles flagged this; the rule
  has nuance worth encoding so future executors get it up front. **Source:**
  [T2/c9 prof.p1+editor.w1 CONSENSUS revert to .len()], [T2/c12 greybeard.m1
  OVERRIDE THIRD OCCURRENCE].
- **`valid_hostname` admits underscores (GHE support > strict RFC).** Hostname
  validation accepts `_` in segments. GitHub Enterprise hostnames frequently
  contain underscores; rejecting them would break GHE users. The exec-not-shell
  invocation pattern means the underscore doesn't open a shell-injection vector.
  **Why:** Explicit human decision at T2/c8 weighing GHE compatibility against
  RFC strictness. **Source:** [T2/c8 what-if.w1 DECISION].
- **`valid_segment` rejects `.` and `..` segments and leading/trailing dots.**
  Path traversal protection. `valid_segment` returns false for `"."`, `".."`,
  segments starting with `.`, and segments ending with `.`. Test coverage
  required for each. **Why:** Path-traversal class; flagged in multiple cycles.
  **Source:** [T2/c5 CONSENSUS what-if.G2+redteam.HU-4], [T2/c17 magnus.m1].
