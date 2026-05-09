## Phase 1: Read-only PR diff view

| Status    | Started    | Completed |
| --------- | ---------- | --------- |
| 🔄 active | 2026-05-09 |           |

Establishes the ggr binary and the read-only diff-viewing surface for GitHub
pull requests. The data source is `gh pr view --json ...` for PR metadata
(commit list, title, branch names) and `git show <sha> --format="" --no-color`
for per-commit diffs. Diff parsing reuses `local-review-core`'s `diff::parse`.
The reviewer navigates commits with `n`/`p` (oldest-to-newest), files with
`Tab`/`Shift-Tab`, and lines with arrow keys or `j`/`k`. Same terminal width
rules as jjr: refuse to render below 60 cols; below 80 cols omit optional footer
segments. Phase 1 is intentionally comment-free — no composer, no `.gh-review/`
directory touched.

#### Delivers

- `crates/ggr/` with its own `error.rs` (snafu), `gh.rs` (`gh`/`git` subprocess
  wrappers), `pr.rs` (PrDetails, CommitEntry), `util.rs` (find_git_root, shared
  layout helpers), and `tui.rs` + `tui/diff_view.rs` + `tui/help_screen.rs`
- `ggr <pr-number>` opens a TUI showing the first commit's diff, with PR title
  and commit position in the stack bar
- `n`/`p` navigates commits; `Tab`/`Shift-Tab` cycles files within a commit;
  `j`/`k`/arrows move the line cursor; `PgUp`/`PgDn`/`g`/`G` scroll
- `?` opens a help screen; `q` exits cleanly
- Errors: `gh` missing → install hint; `git` missing → error; PR not found →
  clear message; terminal too narrow → message with min cols

#### Done When

- `cargo build` produces a `ggr` binary
- `ggr <pr>` opens a TUI showing the first commit's diff
- `n`/`p` switches commits; commit position reflects in the stack bar
- `Tab` and `Shift-Tab` cycle files; `q` exits cleanly
- `?` opens help; `just validate` passes

#### Depends On

- (none — builds on `local-review-core` only)

---

## Phase 2: Local comments

| Status     | Started | Completed |
| ---------- | ------- | --------- |
| ⬜ planned |         |           |

Adds the composer modal and local JSONL comment storage, mirroring jjr's
Phase 2. Comment anchor identifiers use commit SHAs in place of jj change IDs.
Storage at `.gh-review/comments/<sha>.jsonl`. Scopes: line only in this phase.
No GitHub posting — everything stays local. `Anchor::Line`, `Anchor::Commit`,
and `Anchor::PR` variants defined (Commit and PR active in Phase 3).

#### Depends On

- phase-1-read-only-pr-diff-view

---

## Phase 3: PR-level comments and overview screen

| Status     | Started | Completed |
| ---------- | ------- | --------- |
| ⬜ planned |         |           |

Activates `Anchor::Commit` and `Anchor::PR` scopes. PR-level comments stored in
`.gh-review/comments/_pr_<number>.jsonl`. Overview screen (analogous to jjr
Screen 4) shows PR-level comments at top, then commit rows with inset
commit-level comments. Cursor tracking at `.gh-review/cursor.json`.

#### Depends On

- phase-2-local-comments

---

## Phase 4: Reanchoring

| Status     | Started | Completed |
| ---------- | ------- | --------- |
| ⬜ planned |         |           |

Re-uses `local-review-core`'s anchor matching algorithm. Commits in a PR are
immutable by convention (force-push aside), so the primary reanchoring case is
context drift from `git rebase --interactive` or force-push. Stale screen
mirrors jjr Screen 5.

#### Depends On

- phase-3-pr-level-comments-and-overview-screen

---

## Phase 5: Packet generation and Claude

| Status     | Started | Completed |
| ---------- | ------- | --------- |
| ⬜ planned |         |           |

Generates a Claude prompt packet from local comments (same format as jjr
`packet.rs`). Invokes `claude -p` with the packet on stdin. Post-Claude diff
reload shows the reviewer what changed. `C` keybind from main view opens
confirmation modal; `ggr claude <pr>` works from CLI. GitHub comment posting is
deferred to Later Enhancements.

#### Depends On

- phase-4-reanchoring
