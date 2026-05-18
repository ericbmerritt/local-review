# local-review

Two terminal tools for batched, commit-by-commit code review. They share the
same TUI, anchoring engine, and severity vocabulary — but they sit at different
points in the agentic-development loop.

`jjr` is the load-bearing idea: **review the agent's stack before it leaves your
workstation.** When an agent writes commits on your behalf, your name goes on
them. `jjr` walks the local `trunk()..@` stack, captures line / change /
description / stack-scoped comments, and hands them back to Claude. The agent
edits the changes in place; the codebase change is the reply. You re-review and
push when you're satisfied.

`ggr` extends the same loop to GitHub PRs you didn't author. Open a PR by
number, walk each commit oldest-to-newest, draft inline comments, submit one
review. Same TUI, same anchoring, same batched discipline — different source of
truth (`gh` instead of `jj`) and different submission target (the GitHub PR
review API instead of Claude).

| Tool  | Reviews                             | Comments go to          | Install                                  |
| ----- | ----------------------------------- | ----------------------- | ---------------------------------------- |
| `jjr` | Your local jj stack, before pushing | Claude (edits the code) | `cargo install jjr` or Homebrew          |
| `ggr` | A GitHub pull request, by commit    | GitHub PR review API    | Homebrew (crates.io publication pending) |

## The assumption these tools make

Both tools are built around a single premise: **commits are the unit of
review.**

A commit is an atomic, intentional change with a subject, a body, and a coherent
diff. A stack of commits tells a story. When you review a stack or a PR, you
read it in the order it was written — commit by commit — because that's the
order that makes the author's reasoning legible.

If you squash everything into a single commit before pushing, or if your PR is
one giant diff with no internal structure, these tools aren't for you. The
per-commit walk is the whole point. Without commits that mean something, there's
nothing to walk.

This isn't a limitation to work around. It's a design choice. Tools built around
"show me the whole diff" already exist. These tools are for people who treat
commits as communication.

## The shared idea

Code review is a batched operation, not a real-time chat. You read a diff,
decide what needs changing, and record that judgment. Doing it one comment at a
time in a browser — with round-trips to GitHub, notifications flying — creates
friction that makes you review less carefully or skip it entirely.

Both tools eliminate that friction by keeping the whole review session local:

- **Walk oldest-to-newest.** Changes and commits are reviewed in the order they
  were written, so you see the story of the code rather than a jumbled diff.
- **Draft locally, submit once.** Comments accumulate on disk. Nothing reaches
  the network until you explicitly submit. You can quit mid-review and resume
  exactly where you stopped.
- **Inline, anchored comments.** Comments attach to specific lines via text
  matching, not line numbers. When the code shifts under an edit or force-push,
  the tool re-anchors; if it can't, the comment goes stale and surfaces
  separately rather than silently drifting to the wrong line.
- **Same TUI.** Scroll, file picker, severity labels (`[REQUIRED]`,
  `[SUGGESTION]`, `[NOTE]`), side-by-side diff, stale panel — identical between
  the two tools.

## `jjr` — review your own stack before it ships

You wrote code with an agent. The agent has your name on it now. `jjr` is how
you check it before it becomes a PR.

It walks `trunk()..@`, opens the first unreviewed change, and lets you comment
at line / change / description / stack scope. When you're done, `C` hands the
comments to Claude; the agent edits the stack in place. You re-review. Repeat
until you push.

→ See [`crates/jjr/README.md`](crates/jjr/README.md) for full docs.

## `ggr` — review a GitHub PR commit-by-commit

A PR is a stack. `ggr` treats it like one: you open a PR by number or URL, walk
each commit's diff, and draft inline comments. When you're satisfied, `S` opens
a verdict modal — approve, request changes, or comment — and posts everything as
a single GitHub review. Replies to existing threads, partial failure recovery,
and stale-draft detection on force-push are all handled.

→ See [`crates/ggr/README.md`](crates/ggr/README.md) for full docs.

## Shared core

`crates/local-review-core` provides everything both tools use: the diff
renderer, line-anchoring algorithm, TUI framework (parameterised by a
`ReviewSurface` trait), JSONL comment storage helpers, and the severity/scope
model. Adding a new review surface means implementing `ReviewSurface` and
writing a thin shell around a data source.

## Install

```sh
# jjr — crates.io or Homebrew tap
cargo install jjr
brew install ericbmerritt/jjr/jjr

# ggr — Homebrew tap (crates.io publication pending)
brew install ericbmerritt/jjr/ggr
```

Both require their respective CLI dependencies at runtime: `jj` for `jjr`, `gh`
for `ggr`.

## Development

```sh
git clone https://github.com/ericbmerritt/local-review
cd local-review
nix develop            # or direnv allow
just validate          # build + format + lint + tests (90% coverage floor)
```

Individual targets: `just build`, `just lint`, `just test`, `just format`.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE).
