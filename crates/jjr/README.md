# jjr

**Review agent-generated code before it ships.**

When an agent writes commits on your behalf, your name goes on them. `jjr` is
how you check the work. It walks your local jj stack, extracts every changed
entity (function, method, struct, …) using tree-sitter, sorts them by dependency
order, and lets you comment at any scope. When you're done, `C` sends a
structured context bundle to Claude — the changed entity's full body plus its
direct callers and callees — and Claude edits the code in place. You re-review.
Repeat until you push.

## Why jujutsu

`jj` makes the review loop cheap enough to run repeatedly:

- **History rewriting is first-class.** `jj split`, `jj squash`, `jj absorb` are
  everyday commands. The agent re-shapes its stack when told to.
- **Working copy is a commit.** Edits are tracked the moment they hit disk.
- **`trunk()..@` is a stable scope.** The boundary of "what I'm reviewing"
  doesn't shift while you're looking at it.

## Quick start

```sh
cd your-jj-repo
jjr                  # open the entity list for trunk()..@
# j/k to move, Enter to open an entity diff
# c to comment on the current line, Ctrl-X to save
# C to send comments to Claude
jjr                  # re-review the edited stack
jj git push          # ship it
```

## The entity list

The default screen shows changed entities sorted deepest-dependency-first:
entities that others call appear before the things that call them. On wide
terminals, each row shows name, file, line range, caller count, and change
annotation.

```
  Δ authenticate()          src/auth/login.rs :42-78    fn · sig+body    8 callers   ●
  ⊕ UserToken               src/auth/token.rs :12-28    struct · added
  Δ session_parse()         src/db/session.rs :90-115   fn · body         2 callers
```

Press `o` to toggle between dependency order and file+line order.

Entities with only cosmetic changes (whitespace, comment rewrapping) are dimmed
and annotated `[cosmetic]`; press `;` to hide them entirely.

## Entity diff view

`Enter` on an entity opens a focused diff: the full file diff, pre-scrolled to
the entity's start line, with the entity's range highlighted. `Tab` advances to
the next unreviewed entity (wrapping); `Shift-Tab` steps back one entity. `F`
opens the file list if you want to browse by file instead. `x` clips the view to
just the entity's lines.

The status bar shows passive context:
`authenticate() modified · sig+body · called from 8 places`

## Claude context bundle

When you press `C`, the prompt sent to Claude includes — for each line comment —
the full body of the entity containing the commented line, plus its direct
callers and direct callees. Claude can address a comment without breaking the
API surface it relies on.

Budget: 16k tokens per comment (override with `JJR_CONTEXT_BUDGET=<n>`). When
over budget, dependents drop first, then dependencies, then a truncation note is
appended.

## Comments

Four scopes, set in the composer with `M-l` / `M-c` / `M-k` / `M-d`:

| Scope       | Where it renders             | Use for                               |
| ----------- | ---------------------------- | ------------------------------------- |
| line        | Inline below the anchor line | "this `.unwrap()` will panic"         |
| change      | Change description view      | "this commit does too much, split it" |
| description | Inline in the commit message | "this bullet doesn't match the code"  |
| stack       | Stack overview (`s`)         | "rename across the whole stack"       |

Three severities, set with `M-r` / `M-s` / `M-n`:

- **required** — Claude addresses this. Non-response is legible in the next
  diff.
- **suggestion** — Claude addresses if safe and consistent with the change's
  design.
- **note** — Informational; Claude doesn't act on it.

## Keybindings

### Entity list (`Screen::Main`)

| Key               | Action                                            |
| ----------------- | ------------------------------------------------- |
| `j` `k` / `↑` `↓` | Move cursor                                       |
| `Enter`           | Open entity diff (or description for row 0)       |
| `Tab`             | Next **unreviewed** entity (wraps)                |
| `Shift-Tab`       | Previous entity (reviewed or not)                 |
| `F`               | Open file list (escape hatch)                     |
| `n` / `p`         | Next / previous change in stack                   |
| `c`               | New comment (scope follows cursor position)       |
| `1` / `2` / `3`   | Filter by required / suggestion / note            |
| `;`               | Toggle cosmetic entity visibility                 |
| `o`               | Cycle entity order: risk / dependency / file      |
| `g`               | Toggle concern grouping: clustered / flat         |
| `x`               | Blast-radius peek: list callers of focused entity |
| `R`               | Clear entity cache and re-extract                 |
| `s`               | Stack overview                                    |
| `S`               | Stale comments view                               |
| `?`               | Help                                              |
| `q`               | Quit                                              |

### Entity diff view

| Key               | Action                                             |
| ----------------- | -------------------------------------------------- |
| `j` `k` / `↑` `↓` | Scroll line                                        |
| `PgUp` `PgDn`     | Scroll page                                        |
| `g` `G`           | Top / bottom                                       |
| `Tab`             | Next **unreviewed** entity's diff (wraps)          |
| `Shift-Tab`       | Previous entity's diff (reviewed or not)           |
| `x`               | Toggle entity-clip (entity lines only ↔ full file) |
| `F`               | Open file list                                     |
| `n` / `p`         | Next / previous change                             |
| `c` / `Enter`     | New comment on current line                        |
| `e`               | Edit existing comment                              |
| `d`               | Delete existing comment                            |
| `1` / `2` / `3`   | Severity filter                                    |
| `\|`              | Cycle diff layout: auto / unified / side-by-side   |
| `U`               | Toggle file reviewed                               |
| `C`               | Send to Claude                                     |
| `Esc` / `q`       | Return to entity list                              |

### Composer

| Key                           | Action                                     |
| ----------------------------- | ------------------------------------------ |
| `M-l` / `M-c` / `M-k` / `M-d` | Scope: line / change / stack / description |
| `M-r` / `M-s` / `M-n`         | Severity: required / suggestion / note     |
| `Ctrl-X`                      | Save                                       |
| `Ctrl-D`                      | Delete (when editing)                      |
| `Esc`                         | Cancel                                     |

### Send to Claude (`C`)

| Key     | Action                                              |
| ------- | --------------------------------------------------- |
| `v`     | View the full rendered prompt                       |
| `Enter` | Send — suspends TUI, runs Claude, redraws on return |
| `Esc`   | Cancel                                              |

## Commands

| Command                                            | Description                                 |
| -------------------------------------------------- | ------------------------------------------- |
| `jjr`                                              | Walk the stack (`trunk()..heads(@::)`)      |
| `jjr --stack --restart`                            | Reset cursor and start at the oldest change |
| `jjr <change-id>` / `jjr <revset>`                 | Review one change or a custom revset        |
| `jjr claude [revset] [--include-stale]`            | Send comments to Claude from the CLI        |
| `jjr packet [revset]`                              | Print the rendered Claude prompt to stdout  |
| `jjr export [revset] [--format markdown\|jsonl]`   | Export comments                             |
| `jjr clear <revset> [--stale\|--orphaned] [--yes]` | Clear comments                              |

## Configuration

`$XDG_CONFIG_HOME/jjr/config.toml` (fallback: `~/.config/jjr/config.toml`):

```toml
[ui]
transition_screen = "auto"     # "auto" | "always" | "never"

[agent]
tool = "claude"                # any CLI binary on PATH
extra_args = []                # flags passed before the `--` separator
```

To skip Claude's per-edit approval prompts:

```toml
[agent]
extra_args = ["--dangerously-skip-permissions"]
```

`JJR_CONTEXT_BUDGET=<n>` — override the 16k-token Claude bundle budget.

## Files

```
.jj-review/
├── comments/
│   ├── <change-id>.jsonl    # line, change, and description comments
│   └── _stack.jsonl         # stack-scoped comments
├── entities/                # tree-sitter extraction cache (auto-managed)
├── cursor.json              # last-viewed change per revset, for resume
└── reviewed.json            # per-entity reviewed status
```

Ignored by both `git` and `jj`. Never committed, never shared.

## Install

```sh
cargo install jjr
brew install ericbmerritt/jjr/jjr
# or from source:
cargo install --path crates/jjr
```

Requires `jj` on `PATH`. The `claude` CLI is optional — only needed for `C` and
`jjr claude`.

## License

MIT OR Apache-2.0
