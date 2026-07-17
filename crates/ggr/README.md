# ggr

**Review GitHub pull requests commit-by-commit in your terminal.**

`ggr` opens a PR, extracts changed entities (functions, methods, structs, …)
across all commits using tree-sitter, sorts them in dependency-first order, and
lets you draft inline comments locally. When you're done, `S` posts everything
as a single GitHub review.

Part of the [local-review](https://github.com/ericbmerritt/local-review)
workspace alongside `jjr` (pre-push stack review).

## Install

```sh
brew install ericbmerritt/jjr/ggr
# or from source:
cargo install --path crates/ggr
```

Requires [`gh`](https://cli.github.com) authenticated to GitHub or GitHub
Enterprise Server.

## Usage

```sh
ggr 42                                          # auto-detect repo from git remote
ggr acme/myrepo#2429                            # explicit repo, any directory
ggr --url https://github.example.com owner/repo#2429   # GHE
ggr https://github.com/owner/repo/pull/2429    # full URL
```

## Review workflow

### 1. Open a PR

`ggr 42` fetches the PR, re-anchors any saved drafts, and opens the
**description page** for entry 0 — the PR title, body, and PR-level comments,
scrollable with `j`/`k`.

Press `e` to switch to the **entity pane** — an aggregated list of every entity
changed across all commits, deduplicated and sorted dependency-first:

```
  Δ authenticate()          src/auth/login.rs :42-78    fn · sig+body    8 callers
  ⊕ UserToken               src/auth/token.rs :12-28    struct · added
  Δ session_parse()         src/db/session.rs :90-115   fn · body         2 callers
```

Press `e` again to return to the description. Press `n` / `p` to walk the
individual commits (oldest first).

### 2. Read entities per commit

Each per-commit screen (entries 1..N) opens on the same entity list as the PR
overview, but scoped to that commit's changes:

```
  Δ authenticate()          src/auth/login.rs :42-78    fn · sig+body    8 callers
  ⊕ UserToken               src/auth/token.rs :12-28    struct · added
  Δ session_parse()         src/db/session.rs :90-115   fn · body         2 callers
```

Columns scale with terminal width: name, file path with directory context, line
range, caller count (requires dependency graph — see below), change annotation.
Sorted dependency-first by default; `o` toggles to file+line order. Cosmetic
entities (whitespace / comment-only changes) are dimmed; `;` hides them.

`Enter` opens a focused entity diff — the full file diff pre-scrolled to the
entity — with a passive status bar:
`authenticate() modified · sig+body · called from 8 places`

`Tab` advances to the next unreviewed entity (wrapping); `Shift-Tab` steps back
one entity. `F` opens the file list. `x` clips the diff to just the entity's
lines.

### 3. Draft comments

With the cursor on a diff line:

| Key           | Action                          |
| ------------- | ------------------------------- |
| `c` / `Enter` | New line-scoped comment         |
| `m`           | New commit-scoped comment       |
| `P`           | New PR-scoped comment           |
| `r`           | Reply to a GitHub review thread |
| `e`           | Edit a draft you wrote earlier  |

In the composer: `Ctrl-X` saves, `Ctrl-D` deletes, `Esc` cancels.\
Severity: `M-r` required · `M-s` suggestion · `M-n` note (default).

### 4. Submit

`S` opens the verdict modal from any screen:

- `a` — Approve
- `r` — Request changes
- `c` / `Enter` — Comment (needs at least one non-stale draft or reply)
- `Esc` — Cancel

`ggr` posts one `POST /reviews` call with all inline comments, then fans out
reply calls. Stale drafts are excluded. On success all drafts are cleared; on
partial failure, posted drafts are cleared and failed ones remain for retry.

### 5. Refresh after force-push

`R` re-fetches the current PR state and re-anchors drafts. Drafts whose commit
SHA is gone from the PR are matched by commit subject; if no unique successor
exists they go stale.

## Keybindings

### Entity list and description page

| Key               | Action                                            |
| ----------------- | ------------------------------------------------- |
| `j` `k` / `↑` `↓` | Move cursor / scroll                              |
| `Enter`           | Open entity diff (or PR description for row 0)    |
| `Tab`             | Next **unreviewed** entity (wraps)                |
| `Shift-Tab`       | Previous entity (reviewed or not)                 |
| `e`               | Toggle description ↔ entity pane (entry 0 only)   |
| `F`               | File list                                         |
| `n` / `p`         | Next / previous commit                            |
| `1` / `2` / `3`   | Severity filter                                   |
| `;`               | Toggle cosmetic entity visibility                 |
| `o`               | Cycle entity order: risk / dependency / file      |
| `g`               | Toggle concern grouping: clustered / flat         |
| `x`               | Blast-radius peek: list callers of focused entity |
| `S`               | Submit (verdict modal)                            |
| `R`               | Refresh — re-fetch PR, re-anchor drafts           |
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
| `F`               | File list                                          |
| `n` / `p`         | Next / previous commit                             |
| `c` / `Enter`     | New line comment                                   |
| `m`               | New commit-scoped comment                          |
| `r`               | Reply to thread                                    |
| `e`               | Edit draft                                         |
| `T`               | Toggle thread expand/collapse                      |
| `\|`              | Cycle diff layout: auto / unified / side-by-side   |
| `Esc` / `q`       | Return to entity list                              |

## Dependency graph

For richer entity ordering and caller counts in the status bar, `ggr` can clone
the PR's repository to `/tmp/ggr-repos/<host>/<owner>/<repo>/` (shallow, reused
across sessions) and build a cross-file call graph.

The clone is read-only. Only tree-sitter parses run against it; no code is
executed. Disable if you prefer not to download code:

```sh
ggr --no-graph 42          # disable for this invocation
GGR_NO_GRAPH_CLONE=1 ggr 42  # disable via env
```

## Comment scopes and severity

**Scopes:**

- **Line** — anchored to a specific diff line; renders inline.
- **Commit** — anchored to a whole commit; folded into the review body.
- **PR** — anchored to the whole PR; rendered verbatim in the review body.

**Severity** prefixes each submitted comment (`[REQUIRED]` / `[SUGGESTION]` /
`[NOTE]`), matching `jjr`'s format for cross-tool consistency.

## CLI subcommands

```sh
ggr drafts 42                # list local drafts for a PR
ggr clear 42                 # clear all drafts
ggr clear 42 --stale         # clear only stale drafts
```

## Draft storage

`~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/` — local only, never committed.
Nothing reaches GitHub until you press `S`.

## License

MIT OR Apache-2.0
