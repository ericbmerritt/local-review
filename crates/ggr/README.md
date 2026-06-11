# ggr

**Review GitHub pull requests commit-by-commit in your terminal.**

A PR is a stack of commits. `ggr` treats it that way: open a PR by number or
URL, walk each commit's diff oldest-to-newest, draft inline comments locally,
then submit everything as a single GitHub review. No browser required.

`ggr` is part of the
[local-review](https://github.com/ericbmerritt/local-review) workspace alongside
`jjr` (pre-push stack review). Both share the same TUI, comment model, and
anchoring engine. The difference is the source of truth (`gh` instead of `jj`)
and the submission target (GitHub PR review API instead of Claude).

## Install

```sh
brew install ericbmerritt/jjr/ggr
```

`cargo install ggr` will work once the crate is published to crates.io; the
binary is not on crates.io yet. To install from source meanwhile:

```sh
cargo install --path crates/ggr
```

Requires [`gh`](https://cli.github.com) authenticated to GitHub (or GitHub
Enterprise Server).

## Usage

```sh
# Auto-detect repo from the current directory's git remote
ggr 42

# Explicit repo — works from any directory
ggr acme/myrepo#2429

# GitHub Enterprise Server
ggr --url https://github.example.com acme/myrepo#2429

# Paste a full pull URL from the browser
ggr https://github.com/owner/repo/pull/2429
```

## Review workflow

1. **Open a PR.** `ggr 42` fetches the PR, re-anchors any drafts from your last
   session, and opens the description page. Existing GitHub review threads
   render inline below their anchor lines.

2. **Walk the commits.** `n` / `p` move between commits (oldest first) and the
   description page. `Tab` / `Shift-Tab` jump between files. `f` opens the file
   picker.

3. **Draft comments.** With the cursor on a diff line:
   - `c` or `Enter` — line-scoped comment
   - `m` — commit-scoped comment (goes in the review body as an attribution
     block)
   - `P` — PR-scoped comment (goes in the review body verbatim)
   - `r` — reply to an existing GitHub review thread (cursor must be on a
     thread)
   - `e` — edit a draft you wrote earlier
   - In the composer, `Ctrl-X` saves, `Ctrl-D` deletes, `Esc` cancels.
   - Severity: `M-r` required · `M-s` suggestion · `M-n` note (default)

4. **Submit.** `S` opens a verdict modal:
   - `a` — Approve
   - `r` — Request changes
   - `c` or `Enter` — Comment (requires at least one non-stale draft or reply)
   - `Esc` — Cancel

   On submit, `ggr` posts one `POST /reviews` call covering all inline comments,
   then fans out reply calls serially. Stale drafts are excluded. On full
   success all drafts are cleared; on partial failure (a reply failed) the
   posted drafts are cleared and the remaining stay on disk for retry.

5. **Re-review after force-push.** Press `R` to re-fetch the current PR state
   and re-anchor your drafts. Drafts whose commit SHA is gone from the PR are
   re-anchored via commit-subject matching; if no unique successor exists they
   go stale. The stale panel lists them with reasons; `d` deletes a stale draft,
   `Esc` dismisses.

## CLI subcommands

```sh
# List all local drafts for a PR
ggr drafts 42
ggr drafts acme/repo#42

# Clear all local drafts (use --stale to clear only stale ones)
ggr clear 42
ggr clear 42 --stale
```

## Keybindings

### Main view

| Key                    | Action                                            |
| ---------------------- | ------------------------------------------------- |
| `↑` `↓` / `j` `k`      | Scroll line                                       |
| `PgUp` `PgDn`          | Scroll page                                       |
| `Home` `g` / `End` `G` | Top / bottom                                      |
| `Tab` / `Shift-Tab`    | Next / previous file                              |
| `n` / `p`              | Next / previous commit (or description)           |
| `c` / `Enter`          | New line-scoped comment                           |
| `m`                    | New commit-scoped comment                         |
| `P`                    | New PR-scoped comment                             |
| `r`                    | Reply to thread on current line                   |
| `e`                    | Edit draft on current line                        |
| `T`                    | Toggle thread expand/collapse                     |
| `S`                    | Submit (opens verdict modal)                      |
| `R`                    | Refresh — re-fetch PR state and re-anchor drafts  |
| `f`                    | File picker                                       |
| `\|`                   | Cycle diff layout (auto / unified / side-by-side) |
| `?`                    | Help                                              |
| `q`                    | Quit                                              |

### Composer (when writing a comment)

| Key      | Action                                    |
| -------- | ----------------------------------------- |
| `M-l`    | Scope: line (default when on a diff line) |
| `M-c`    | Scope: change / commit                    |
| `M-k`    | Scope: PR (same as `P` from main view)    |
| `M-r`    | Severity: required                        |
| `M-s`    | Severity: suggestion                      |
| `M-n`    | Severity: note (default)                  |
| `Ctrl-X` | Save                                      |
| `Ctrl-D` | Delete (when editing an existing draft)   |
| `Esc`    | Cancel                                    |

## Comment scopes and severity

**Scopes:**

- **Line** — anchored to a specific line in a specific file in a specific
  commit. Renders inline below the anchor line.
- **Commit** — anchored to a whole commit. Folded into the review body as a
  quoted attribution block: `> Commit abc1234 — "commit title"`.
- **PR** — anchored to the whole PR. Rendered verbatim in the review body.

**Severity** becomes the first line of the submitted comment body, matching
`jjr`'s format for cross-tool consistency:

- `[REQUIRED]` — must be addressed
- `[SUGGESTION]` — should be addressed when safe
- `[NOTE]` — informational

## Draft storage

Drafts live in `~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/` and are never
shared or committed. Nothing reaches GitHub until you press `S`.

## License

MIT OR Apache-2.0
