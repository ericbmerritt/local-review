# jjr

**Local stack review for agent-generated code.**

Before agent-generated commits leave your workstation as PRs, you have to review
them. They have your name on them. The agent wrote the code; you're the one
who's accountable for it.

`jjr` is the tool for that pass. It walks you through a stack of jj changes,
captures line / change / description / stack-scoped comments, and hands the
comments to Claude (or another agent CLI) for remediation. The agent edits the
changes in place. You re-review. Repeat until you push.

This is **self-review**, not collaboration. Comments stay on disk, ignored by
git and jj. Nothing is committed. Nothing is shared.

## Why jujutsu

Agent-driven coding generates many small commits that need rewriting before they
ship: split this one, squash these two, drop that experiment, fix the
description. The git workflow for that is interactive rebase — slow,
error-prone, and easy to bail on. The friction tax is paid every cycle.

`jj` removes that tax in three places that matter for the review loop:

- **History rewriting is first-class.** `jj split`, `jj squash`, `jj rebase`,
  `jj absorb` are everyday commands, not ceremony. The agent re-shapes its own
  stack when told to, and `jj` records conflicts as first-class objects in
  commits rather than halting the rebase — descendants re-parent automatically
  and you handle conflicts where they actually live instead of in a 30-minute
  interactive rebase.
- **Working copy is a commit.** The agent's edits are tracked the moment they
  hit disk. There's no "did I `git add`?" failure mode and no staging area to
  desynchronize from intent.
- **A stack is the review unit.** `trunk()..@` gives `jjr` a stable, exact
  definition of "the work I'm reviewing." One change is one commit is one review
  pass. The boundary doesn't drift while you're looking at it.

`jjr` exists because `jj`'s data model removes the friction the agent loop
generates. The reviewer points at content; the tool re-anchors comments across
the agent's rewrites; the cycle stays cheap enough to run as many times as the
work needs. None of that is impossible on git, but on git it costs enough that
you stop running the loop. That's why this is `jujutsu-review`, not
`git-review`.

## Synopsis

```
jjr [revset]
jjr <subcommand> [args]
```

Two modes:

- **Stack mode** (default; bare `jjr`, `jjr --stack`, or `jjr <revset>` where
  the revset returns multiple changes): walk a sequence of changes
  oldest-to-newest with `n` / `p`.
- **Single-change mode** (`jjr <change-id>`, or any revset that returns one
  change): review a single change. Stack-mode keys (`s`, `n`, `p`) are inert.

The default revset is `trunk()..@`.

## Status

MVP complete and stable. The whole [milestone ladder](specs/jjr-mvp.ladder.md)
has landed: read-only diff view, line / change / description / stack-scoped
comments with severity, stack walking with cursor resume, side-by-side diff at
wide widths, line re-anchoring with stale view, packet generation, single-change
Claude handoff via the CLI (`jjr claude`) and stack-wide handoff via the in-TUI
`C` key in stack mode (both with working-copy guard), per-file reviewed
tracking, file picker, severity filters, refresh, JSONL/markdown export,
configurable agent CLI.

Not yet implemented (deferred): inter-cycle diff feature, full `jjr orphans`
view (the `--orphaned` flag on `jjr clear` covers cleanup), syntax highlighting,
GitHub integration, multiple-reviewer support.

See the [engineering design document](specs/local-stack-review-edd.md) for the
full spec and the [TUI design document](specs/jjr-tui-design.md) for screen
layouts.

## Install

The flake exposes a package, so the one-liner is:

```bash
nix profile install github:ericbmerritt/jujutsu-review
```

To run without installing:

```bash
nix run github:ericbmerritt/jujutsu-review -- --help
```

To build from a clone (Rust toolchain and review tooling come from the dev
shell):

```bash
git clone https://github.com/ericbmerritt/jujutsu-review
cd jujutsu-review
nix develop                    # or `direnv allow` if you use direnv
cargo install --path .         # builds and installs `jjr` to ~/.cargo/bin
```

`jj` (jujutsu) must be on `PATH` at runtime — `jjr` does not bundle it, so it
uses whatever `jj` your environment provides. The Nix dev shell installs one;
outside the dev shell, install jj separately. The `claude` CLI is optional —
only needed for `jjr claude` and the `C` keybind.

## Quickstart

From inside any jj-tracked repository:

```bash
$ jjr                          # opens the stack
  # ↑ ↓ to scan a diff, n / p to move between changes (oldest-to-newest)
  # Tab cycles files; f opens the file picker
  # Enter on a line to comment; Ctrl-X saves
  # press C to send comments to Claude

$ jjr claude @                 # alternative: send from CLI without entering the TUI

$ jjr                          # re-review the modified stack
  # resumes where you stopped; stale comments surface separately under S

$ jj git push                  # ship it
```

## Commands

| Command                                             | Description                                                                                                                                |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `jjr`                                               | Walk the stack (`trunk()..@`); fresh stack opens at oldest, resumed stack opens at the most-recent unreviewed change; `n` / `p` to advance |
| `jjr --stack`                                       | Same as bare `jjr`; explicit form                                                                                                          |
| `jjr --stack --restart`                             | Clear the saved cursor and start at the oldest change                                                                                      |
| `jjr <change-id>`                                   | Review a single specific change                                                                                                            |
| `jjr <revset>`                                      | Review changes returned by an arbitrary jj revset                                                                                          |
| `jjr packet [revset] [--include-stale] [-o <path>]` | Print the rendered Claude prompt to stdout, or write it to a file                                                                          |
| `jjr claude [revset] [--include-stale]`             | Send comments to the configured agent CLI for remediation                                                                                  |
| `jjr export [revset] [--format markdown\|jsonl]`    | Export comments as JSONL (default) or markdown                                                                                             |
| `jjr clear <revset> [--stale\|--orphaned] [--yes]`  | Clear comments for a revset                                                                                                                |

## Keybindings

### Main view

| Key                    | Action                                                                    |
| ---------------------- | ------------------------------------------------------------------------- |
| `↑` `↓` / `j` `k`      | Move line cursor                                                          |
| `PgUp` `PgDn`          | Page up / down                                                            |
| `Home` `End` / `g` `G` | Top / bottom of diff                                                      |
| `Tab` / `Shift-Tab`    | Next / previous file                                                      |
| `n` / `p`              | Next / previous change in stack                                           |
| `Enter` / `c`          | New comment on current line                                               |
| `e`                    | Edit comment (cursor on a comment)                                        |
| `d`                    | Delete comment (cursor on a comment)                                      |
| `1` / `2` / `3`        | Filter to required / suggestion / note (press again to clear)             |
| `f`                    | File picker — jump to a file or the change description                    |
| `r`                    | Refresh — re-run `jj show` and reload comments                            |
| `s`                    | Stack overview                                                            |
| `S`                    | Stale comments view                                                       |
| `U`                    | Toggle reviewed status on the current file                                |
| <code>&#124;</code>    | Cycle diff layout: auto / unified / side-by-side                          |
| `C`                    | Send to Claude (current change in single mode; whole stack in stack mode) |
| `?`                    | Help                                                                      |
| `q`                    | Quit                                                                      |

### Composer (when writing a comment)

| Key                           | Action                                     |
| ----------------------------- | ------------------------------------------ |
| `M-l` / `M-c` / `M-k` / `M-d` | Scope: line / change / stack / description |
| `M-r` / `M-s` / `M-n`         | Severity: required / suggestion / note     |
| `Ctrl-X`                      | Save                                       |
| `Ctrl-D`                      | Delete (when editing an existing comment)  |
| `Esc`                         | Cancel                                     |

`Ctrl-X` (not `Ctrl-S`) is deliberate — POSIX terminals eat `Ctrl-S` for XOFF
flow control.

### Stack overview (`s` from main view)

| Key               | Action                                                                                          |
| ----------------- | ----------------------------------------------------------------------------------------------- |
| `↑` `↓` / `j` `k` | Select row                                                                                      |
| `Enter`           | Open change (on a change row) / edit comment (on a comment row)                                 |
| `c`               | New comment; scope defaults from cursor (stack header → stack scope; change row → change scope) |
| `q` / `Esc`       | Back to main view                                                                               |

### Stale view (`S` from main view)

| Key               | Action                                                    |
| ----------------- | --------------------------------------------------------- |
| `↑` `↓` / `j` `k` | Select entry                                              |
| `Enter`           | View in source — navigate main view to the anchor         |
| `e`               | Edit and re-anchor (switch to main view, pick a new line) |
| `d`               | Delete focused stale comment                              |
| `q` / `Esc`       | Back to main view                                         |

### Send to Claude (`C` from main view)

| Key     | Action                                              |
| ------- | --------------------------------------------------- |
| `v`     | View the full rendered prompt                       |
| `Enter` | Send — suspends TUI, runs Claude, redraws on return |
| `Esc`   | Cancel                                              |

## Comments

Comments come in four scopes:

- **line** — anchored to a specific line in a specific file in a specific
  change. Renders inline directly below the target line, indented with a `┃`
  column marker. The default. For "this `.unwrap()` will panic" or "rename this
  variable."
- **change** — anchored to a whole change. Renders in that change's description
  view (file index 0), appended after the description body with a
  `─ on this change ─` separator. For "this commit does too much, split it" or
  "the description doesn't match the code."
- **description** — anchored to a specific line in a change's commit message.
  Renders inline below the anchor line in the description view. For "this bullet
  doesn't reflect what the code actually does."
- **stack** — anchored to the whole stack you're reviewing. Renders in the stack
  overview (`s`), at the top above the change rows. For "rename `retry_wrapper`
  to `retry_policy` throughout" or "don't introduce new public APIs in this
  stack."

Each comment carries a severity:

- **required** — Claude addresses this by editing the code. If it's not
  addressed in the next cycle's diff, you'll see that and can re-comment,
  escalate, or fix it yourself.
- **suggestion** — Claude addresses if safe and consistent with the change's
  design. If a suggestion would broaden scope or break intent, Claude leaves it.
  The diff (or its absence) is the response.
- **note** — Informational. Claude doesn't act on it.

There's no decline-with-reasoning channel. Claude responds by editing the code,
full stop. If it doesn't change something you flagged, that's the conversation —
you read the next diff and adjudicate.

## Re-review and stale comments

After Claude edits the working copy, the diff changes. `jjr` re-anchors comments
by matching `target_text` plus surrounding context — not line numbers, which
shift on every edit.

- Comments that re-anchor cleanly show inline as before.
- Comments that can't re-anchor are marked **stale** and surface in a separate
  panel (`S`) showing was/now lines and the mismatch reason: target text
  changed, anchor not found, or file removed.

Stale comments aren't auto-deleted. You decide whether to clear them, edit them
to re-anchor, or send them anyway with `--include-stale`.

Stack-scoped comments are never stale (no anchoring to content); they reappear
in every cycle until cleared. Description-scoped comments re-anchor against the
commit message; if the message changes enough they go stale alongside
line-scoped comments.

## Configuration

Settings live in a single global file: `$XDG_CONFIG_HOME/jjr/config.toml` if
`XDG_CONFIG_HOME` is set, otherwise `~/.config/jjr/config.toml`. The file is
optional; missing fields fall back to defaults.

```toml
[ui]
transition_screen = "auto"        # "auto" | "always" | "never" (default "never")

[agent]
tool = "claude"                   # CLI binary used by `jjr claude` and the C keybind
extra_args = []                   # flags passed to the agent before the `--` separator
```

To skip Claude's per-edit approval prompts, set:

```toml
[agent]
extra_args = ["--dangerously-skip-permissions"]
```

`tool` accepts any binary on `PATH` (e.g., `tool = "opencode"`). `jjr` spawns
the named binary with `extra_args`, then `--`, then the prompt path.

**Migration.** If you previously set config in
`<repo_root>/.jj-review/config.toml`, move it to
`$XDG_CONFIG_HOME/jjr/config.toml` (or `~/.config/jjr/config.toml`). Per-repo
config files are no longer read.

## Files

Everything is local. `jjr` writes to a `.jj-review/` directory at the repo root,
ignored by both `git` and `jj`:

```
.jj-review/
├── comments/
│   ├── <change-id>.jsonl    # line, change, and description comments per change
│   └── _stack.jsonl         # stack-scoped comments; records carry revset hash
├── cursor.json              # last-viewed change per resolved revset, for resume
├── reviewed.json            # per-change file-reviewed status (the ✓ indicator)
└── config.toml              # optional [ui] / [agent] config
```

Comments are never committed and never shared. `.jj-review/` is added to
`.gitignore` and `.jjignore` on first run.

## Development

Run all gates locally:

```bash
nix develop --command just validate
```

`just validate` runs `cargo build`, format checks (rustfmt + alejandra +
prettier + trailing-whitespace), lints (clippy with strict lint posture,
cargo-deny, statix), and the test suite under `cargo-llvm-cov` with a 90%
line-coverage floor on the functional core. The same command runs in CI on every
push and pull request.

Other targets: `just format`, `just lint`, `just test`, `just build`. Bare
`just` lists everything.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE),
at your option.
