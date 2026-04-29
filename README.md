# jjr

**Local stack review for agent-generated code.**

Before agent-generated commits leave your workstation as PRs, you have to review them. They have your name on them. The agent wrote the code; you're the one who's accountable for it.

`jjr` is the tool for that pass.

## What it's for

You sent an agent off to implement a spec. It came back with a stack of fifteen `jj` commits. They compile, the tests pass, and now you have to read them — commit by commit, oldest to newest — and decide whether what you're about to publish actually represents your judgment.

This is **self-review**, not collaboration. There's nobody on the other end. It's you, looking at code an agent wrote, deciding whether it's good enough to ship under your name.

The workflow `jjr` encodes is a **review cycle**:

1. Walk the stack from oldest to newest.
2. For each commit, read the diff.
3. Where something needs to change, leave a comment — on a line, on the whole commit, or on the stack.
4. When done with a commit, advance to the next.
5. When done with the stack, hand the comments to the agent. The agent edits the changes in place.
6. Re-review the modified stack. New cycle.

You repeat the cycle as many times as the work demands. When you're satisfied, you push. The tool doesn't tell you when to stop — that judgment is yours.

That's it. No PR system. No web UI. No automated reviewer. Just the missing primitive between *agent finishes generating* and *you push to GitHub*.

## What it assumes

You're using [jujutsu](https://github.com/jj-vcs/jj). When the agent addresses your comments, it edits each change in place — that's how jj works. If you bring git instincts (commits are sacred, fixes go in new commits), the loop will surprise you. Trust jj's mutability model; that's the substrate the tool is built on.

## Why this exists

The current local review primitive is `jj show | less`. That works for reading. It doesn't work for the actual review loop, which requires capturing line-specific intent without losing your place in the stack.

Without a tool, the loop falls apart into scratchpad notes, copy/paste, and ad hoc Claude prompts written from memory of what you saw three commits ago. The result is either thin review (you skim, you trust, you ship) or no review (you stop reading at commit four of fifteen).

Neither is acceptable when the code has your name on it.

`jjr` makes the sequential walk fast enough that you actually do it.

## What it isn't

- **Not a PR review tool.** It runs against your local jj working copy, before anything is pushed.
- **Not an AI reviewer.** Claude doesn't review the code. You review the code. Claude addresses the comments you leave by editing the codebase — that's the only response. No prose, no summary, no decline-with-reasoning. The diff is the reply.
- **Not a GitHub client.** Doesn't talk to GitHub. Doesn't create PRs. Doesn't post comments anywhere.
- **Not collaborative.** Comments are local, never committed, never shared.
- **Not a code editor.** It's a viewer with comment affordances.
- **Not a gate.** The tool doesn't model "done." There's no approved state, no required-comments-outstanding warning, no quit-time summary. It surfaces what's there; you decide when you're satisfied. Pushing happens outside the tool.

## Workflow

```bash
# agent generates a stack of changes
$ claude --headless implement-spec.md

# you review the stack
$ jjr --stack
  # walks oldest to newest
  # n = next change, p = previous, c = comment, C = send to Claude

# Claude addresses your comments
  # press C inside the TUI, or run from CLI:
$ jjr claude @

# re-review the modified stack
$ jjr --stack
  # resumes where you stopped; stale comments surface separately

# ship it
$ jj git push
```

## Installation

```bash
cargo install jjr
```

Requires `jj` (jujutsu) on PATH. Optional: `claude` CLI for the remediation handoff.

## Quick reference

| Command | What it does |
|---|---|
| `jjr` | Review the current change (`@`) |
| `jjr --stack` | Review the current stack, oldest to newest, resuming where you left off |
| `jjr <change-id>` | Review a single specific change |
| `jjr <revset>` | Review changes returned by an arbitrary jj revset |
| `jjr packet [revset]` | Print the review packet that would be sent to Claude |
| `jjr claude [revset]` | Send comments to Claude CLI for remediation |
| `jjr export [revset]` | Export comments as JSONL or markdown |
| `jjr clear [revset]` | Clear all comments for a revset |

In the TUI:

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | Move line |
| `n` / `p` | Next / previous change in stack |
| `Tab` / `Shift-Tab` | Next / previous file |
| `Enter` / `c` | New comment — scope (line / change / stack) defaults from cursor |
| `s` | Stack overview |
| `S` | Stale comments |
| `C` | Send current change to Claude |
| `?` | Help |
| `q` | Quit |

Inside the composer:

| Key | Action |
|---|---|
| `^L` / `^C` / `^K` | Scope: line / change / stack |
| `^1` / `^2` / `^3` | Severity: note / suggestion / required |
| `^X` | Save |
| `Esc` | Cancel |

## Comments

Comments come in three scopes:

- **line** — anchored to a specific line in a specific file in a specific change. The default. For "this `.unwrap()` will panic" or "rename this variable."
- **change** — anchored to a whole change. For "this commit does too much, split it" or "the description doesn't match the code."
- **stack** — anchored to the whole stack you're reviewing. For "rename `retry_wrapper` to `retry_policy` throughout" or "don't introduce new public APIs in this stack."

Each comment carries a severity:

- **required** — Claude addresses this by editing the code. If it's not addressed in the next cycle's diff, you'll see that and can re-comment, escalate, or fix it yourself.
- **suggestion** — Claude addresses if safe and consistent with the change's design. If a suggestion would broaden scope or break intent, Claude leaves it. The diff (or its absence) is the response.
- **note** — Informational. Claude doesn't act on it.

There is no decline-with-reasoning channel. Claude responds by editing the code, full stop. If it doesn't change something you flagged, that's the conversation — you read the next diff and adjudicate.

Comments are stored locally in `.jj-review/`, ignored by both `git` and `jj`. They're never committed.

## Re-review

After Claude edits the working copy, the diff changes. `jjr` re-anchors comments by matching `target_text` plus surrounding context — not line numbers, which shift on every edit.

- Comments that re-anchor cleanly show inline as before.
- Comments that can't re-anchor are marked **stale** and surface in a separate panel (`S`) with the reason: target text changed, anchor not found, or file removed.

Stale comments aren't auto-deleted. You decide whether to clear them, edit them to re-anchor, or send them to Claude anyway with `--include-stale`.

## Why local, why before publication

Two reasons.

**The PR shouldn't be where review starts.** When the agent generates the stack, the diff is fresh in your head — you wrote the spec, you know what you asked for. Reviewing then, locally, before publication, is when your judgment is sharpest. By the time it's a PR, you're context-switched away.

**Your name is on it.** Whatever ships under your authorship represents your engineering judgment. An agent producing a working test suite is necessary but not sufficient — the code also has to be the kind of code you'd write. That's a judgment only you can make, and only by reading it.

`jjr` is the surface that makes the reading fast enough to actually do.

## Status

MVP. Single-change and stack review, JSONL comment storage, Claude CLI handoff, sequential resumability. See [the engineering design document](./specs/local-stack-review-edd.md) for full spec and [the TUI design](./specs/jjr-tui-design.md) for screen layouts.

## License

TBD.
