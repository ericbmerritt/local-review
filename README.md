# jjr

**Local stack review for agent-generated code.**

Before agent-generated commits leave your workstation as PRs, you have to review
them. They have your name on them. The agent wrote the code; you're the one
who's accountable for it.

`jjr` is the tool for that pass.

## What is jjr?

You sent an agent off to implement a spec. It came back with a stack of fifteen
`jj` commits. They compile, the tests pass, and now you have to read them —
commit by commit, oldest to newest — and decide whether what you're about to
publish actually represents your judgment.

`jjr` is a terminal UI that walks the stack with you. You read each diff, leave
line / change / stack-scoped comments where something needs to change, then hand
the comments to Claude in one shot. Claude addresses the comments by editing
each change in place; you re-review the modified stack. Repeat until you're
satisfied, then push.

This is **self-review**, not collaboration. There's nobody on the other end.
It's you, looking at code an agent wrote, deciding whether it's good enough to
ship under your name.

## The review cycle

The workflow `jjr` encodes is a cycle:

1. Walk the stack from oldest to newest.
2. For each commit, read the diff.
3. Where something needs to change, leave a comment — on a line, on the whole
   commit, or on the stack.
4. When done with a commit, advance to the next.
5. When done with the stack, hand the comments to the agent. The agent edits the
   changes in place.
6. Re-review the modified stack. New cycle.

You repeat the cycle as many times as the work demands. When you're satisfied,
you push. The tool doesn't tell you when to stop — that judgment is yours.

That's it. No PR system. No web UI. No automated reviewer. Just the missing
primitive between _agent finishes generating_ and _you push to GitHub_.

## Why this exists

The current local review primitive is `jj show | less`. That works for reading.
It doesn't work for the actual review loop, which requires capturing
line-specific intent without losing your place in the stack.

Without a tool, the loop falls apart into scratchpad notes, copy/paste, and ad
hoc Claude prompts written from memory of what you saw three commits ago. The
result is either thin review (you skim, you trust, you ship) or no review (you
stop reading at commit four of fifteen).

Neither is acceptable when the code has your name on it.

`jjr` makes the sequential walk fast enough that you actually do it.

GitHub PR review and Gerrit are designed for the _other_ side of publication —
once code is up for someone else to look at. `jjr` is the surface _before_
publication, when the diff is fresh in your head and your judgment is sharpest.
By the time it's a PR, you're context-switched away.

## What it isn't

- **Not a PR review tool.** It runs against your local jj working copy, before
  anything is pushed.
- **Not an AI reviewer.** Claude doesn't review the code. You review the code.
  Claude addresses the comments you leave by editing the codebase — that's the
  only response. No prose, no summary, no decline-with-reasoning. The diff is
  the reply.
- **Not a GitHub client.** Doesn't talk to GitHub. Doesn't create PRs. Doesn't
  post comments anywhere.
- **Not collaborative.** Comments are local, never committed, never shared.
- **Not a code editor.** It's a viewer with comment affordances.
- **Not a gate.** The tool doesn't model "done." There's no approved state, no
  required-comments-outstanding warning, no quit-time summary. It surfaces
  what's there; you decide when you're satisfied. Pushing happens outside the
  tool.

## What it assumes

You're using [jujutsu](https://github.com/jj-vcs/jj). When the agent addresses
your comments, it edits each change in place — that's how jj works. If you bring
git instincts (commits are sacred, fixes go in new commits), the loop will
surprise you. Trust jj's mutability model; that's the substrate the tool is
built on.

## Install

`jjr` is built and run from the Nix dev shell. The flake pins the Rust toolchain
and pulls in `jj`, `just`, `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`,
`alejandra`, `statix`, and `prettier`.

```bash
git clone https://github.com/emerritt/jujutsu-review
cd jujutsu-review
nix develop                    # or `direnv allow` if you use direnv
cargo install --path .         # builds and installs `jjr` to ~/.cargo/bin
```

You'll need `jj` (jujutsu) on PATH at runtime. The Nix dev shell provides it;
outside the dev shell, install jj separately. The `claude` CLI is optional —
only needed for `jjr claude` to hand comments off for remediation.

## Quick start

From inside any jj-tracked repository:

```bash
$ jjr
```

That's it. With no arguments, `jjr` resolves the default stack (`trunk()..@`)
and drops you into the TUI. On a fresh stack with no reviewed-state, you land at
the oldest change so the natural `n` flow walks the stack bottom-up. On a
half-reviewed stack, you land at the most-recent change you haven't finished —
the front of the new work. The `n` / `p` keys walk oldest-to-newest. Use
`jjr --stack --restart` to clear the saved cursor and start over.

A typical session:

```bash
# 1. (your agent of choice generates a stack of changes)

# 2. you review the stack
$ jjr
  # fresh stack: opens at the oldest change; resumed stack: opens at the most-recent unreviewed change
  # ↑ ↓ to scan a diff, n / p to move between commits (oldest-to-newest), Tab to cycle files
  # Enter on a line to comment; Ctrl-X saves
  # press C to send comments to Claude

# 3. Claude addresses your comments (alternative to pressing C above)
$ jjr claude @

# 4. re-review the modified stack
$ jjr
  # resumes where you stopped; stale comments surface separately under S

# 5. ship it
$ jj git push
```

## Commands

| Command                                             | What it does                                                                                                                                                          |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `jjr`                                               | Walk the stack (`trunk()..@`); fresh stack opens at the oldest change, resumed stack opens at the most-recent unreviewed change, then `n` / `p` move oldest-to-newest |
| `jjr --stack`                                       | Same as bare `jjr`; explicit form                                                                                                                                     |
| `jjr --stack --restart`                             | Clear the saved cursor and start at the oldest change                                                                                                                 |
| `jjr <change-id>`                                   | Review a single specific change                                                                                                                                       |
| `jjr <revset>`                                      | Review changes returned by an arbitrary jj revset                                                                                                                     |
| `jjr packet [revset] [--include-stale] [-o <path>]` | Print the review packet that would be sent to Claude (or write to a file)                                                                                             |
| `jjr claude [revset] [--include-stale]`             | Send comments to Claude CLI for remediation                                                                                                                           |
| `jjr export [revset] [--format markdown\|jsonl]`    | Export comments as JSONL (default) or markdown                                                                                                                        |
| `jjr clear <revset> [--stale\|--orphaned] [--yes]`  | Clear comments for a revset                                                                                                                                           |

## Key bindings

### Main view

| Key                    | Action                                                        |
| ---------------------- | ------------------------------------------------------------- |
| `↑` `↓` / `j` `k`      | Move line cursor                                              |
| `PgUp` `PgDn`          | Page up / down                                                |
| `Home` `End` / `g` `G` | Top / bottom of diff                                          |
| `Tab` / `Shift-Tab`    | Next / previous file                                          |
| `n` / `p`              | Next / previous change in stack                               |
| `Enter` / `c`          | New comment on current line                                   |
| `e`                    | Edit comment (cursor on a comment)                            |
| `d`                    | Delete comment (cursor on a comment)                          |
| `1` / `2` / `3`        | Filter to required / suggestion / note (press again to clear) |
| `f`                    | File picker — jump to a file or the change description        |
| `r`                    | Refresh — re-run `jj show` and reload comments                |
| `s`                    | Stack overview                                                |
| `S`                    | Stale comments view                                           |
| `U`                    | Toggle reviewed status on the current file                    |
| `C`                    | Send current change to Claude                                 |
| `?`                    | Help                                                          |
| `q`                    | Quit                                                          |

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

Comments come in three scopes:

- **line** — anchored to a specific line in a specific file in a specific
  change. The default. For "this `.unwrap()` will panic" or "rename this
  variable."
- **change** — anchored to a whole change. For "this commit does too much, split
  it" or "the description doesn't match the code."
- **stack** — anchored to the whole stack you're reviewing. For "rename
  `retry_wrapper` to `retry_policy` throughout" or "don't introduce new public
  APIs in this stack."

There's also a fourth scope, **description**, available when the cursor is on a
change's commit message — for comments on the commit description itself.

Each comment carries a severity:

- **required** — Claude addresses this by editing the code. If it's not
  addressed in the next cycle's diff, you'll see that and can re-comment,
  escalate, or fix it yourself.
- **suggestion** — Claude addresses if safe and consistent with the change's
  design. If a suggestion would broaden scope or break intent, Claude leaves it.
  The diff (or its absence) is the response.
- **note** — Informational. Claude doesn't act on it.

There is no decline-with-reasoning channel. Claude responds by editing the code,
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
to re-anchor, or send them to Claude anyway with `--include-stale`.

Stack-scoped comments are never stale (no anchoring to content); they reappear
in every cycle until cleared.

## Configuration

Settings live in a single global file: `$XDG_CONFIG_HOME/jjr/config.toml` if
`XDG_CONFIG_HOME` is set, otherwise `~/.config/jjr/config.toml`. The file is
optional; missing fields fall back to defaults.

```toml
[ui]
transition_screen = "auto"        # "auto" | "always" | "never" (default "never")

[agent]
tool = "claude"                   # CLI binary used by `jjr claude` and the C-key send-to-claude flow
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

## Storage

Everything is local. `jjr` writes to a `.jj-review/` directory at the repo root,
ignored by both `git` and `jj`:

```
.jj-review/
├── comments/
│   ├── <change-id>.jsonl    # one file per change, one JSONL row per comment
│   └── _stack.jsonl         # stack-scoped comments; records carry revset hash
├── cursor.json              # last-viewed change per resolved revset, for resume
└── reviewed.json            # per-change file-reviewed status (the ✓ indicator)
```

Comments are never committed and never shared. `.jj-review/` is added to
`.gitignore` and `.jjignore` on first run.

## Status

MVP complete. All milestones in the [ladder](specs/jjr-mvp.ladder.md) are
landed: read-only diff view, line / change / stack / description-scoped comments
with severity, stack walking with cursor resume, line re-anchoring with stale
view, packet generation, Claude invocation with working-copy guard, and polish
(export, file picker, refresh, severity filters, reviewed tracking).

Stable: single-change review, stack walking, comment storage, anchoring, packet
generation, Claude single-change handoff, JSONL/markdown export.

Not yet implemented (deferred): stack-wide Claude handoff, inter-cycle diff
feature, full `jjr orphans` view, syntax highlighting, GitHub integration,
multi-reviewer support.

See the [engineering design document](specs/local-stack-review-edd.md) for full
spec, the [TUI design document](specs/jjr-tui-design.md) for screen layouts, and
the [milestone ladder](specs/jjr-mvp.ladder.md) for what's landed and what's
planned.

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

Other targets: `just format`, `just lint`, `just test`, `just build`. `just`
with no arguments lists everything.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE),
at your option.
