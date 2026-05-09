# GitHub PR Stack Review — Engineering Design Document

## User Narrative

Priya is reviewing Marco's PR #2429 — a six-commit auth rewrite. She has about
an hour before her next meeting.

She opens it from the terminal:

```
ggr 2429
```

ggr fetches the PR. The description page shows Marco's writeup and the two
issue-thread comments from earlier this week. Priya reads them, hits `n`, and
lands on commit 1's first changed file.

Commit 1 ("extract Session into its own module") is mechanical. She walks the
diff with arrows, hits `Tab` to cycle files, sees nothing worth commenting on,
hits `n`.

Commit 2 ("introduce SessionStore trait"). On line 88 of `src/session/store.rs`
she sees a method that bypasses the per-store locking convention established in
commit 1. She hits `Enter`, picks `required`, and types: "This bypasses the
per-store mutex from commit 1. Either move the lock into the trait or document
why it's safe to skip." The composer closes; the comment is a local draft.

She also wants to flag that commit 2's message says "introduce SessionStore
trait" but the diff also moves three unrelated logging calls. She opens the
composer at _commit_ scope, picks `note`, types "This commit also moves logging
— should be a separate commit." Local draft.

Commits 3, 4, and 5 walk fast. Commit 6 ("rename retry_wrapper to retry_policy")
shows an existing thread on line 14 from another reviewer asking "why now?" —
Marco has already replied. Priya hits the reply shortcut, agrees with Marco's
reasoning, severity `note`. Local draft.

Cross-cutting concern: the rename in commit 6 doesn't touch the docs. She opens
the composer at PR scope, picks `suggestion`, types "docs/auth.md still uses
retry_wrapper." Local draft.

She hits `S` to submit. The verdict modal asks for approve / request changes /
comment, defaulting to comment. She picks `request changes` because of the
locking concern in commit 2. ggr translates her drafts into one GitHub review
submission plus one reply call, posts them, and clears local drafts on success.
Total time: 38 minutes.

Marco fixes the locking issue and force-pushes the next morning. Priya gets the
GitHub notification, reopens:

```
ggr 2429
```

ggr fetches the new state. Marco rewrote commit 2; the original SHA is gone from
the PR's commit list, but a new commit with the same subject is present. Priya's
submitted line comment from the prior cycle shows up as an existing thread on
the new commit, marked outdated because it was anchored to the old SHA. She
walks the new commit, sees the lock now in place, leaves a `note` saying
"thanks." Submit, verdict `approve`. Done.

## Purpose

Build a local terminal tool for reviewing **other people's** stacked GitHub pull
requests, walking commit-by-commit, capturing reviewer intent at three scopes,
and submitting it back as one GitHub review.

This is the companion tool to `jjr`. They share the review experience and
diverge at the endpoints:

- `jjr` reviews **your own** jj stack before you push it. Claude addresses your
  comments by editing the working copy. The codebase change is the reply.
- `ggr` reviews **someone else's** GitHub pull request after they pushed it. The
  reviewee addresses your comments by updating the PR. The GitHub review is the
  reply.

The two tools are not duplicates. They are the same shape applied to two
different review situations.

## Core Problem

The current way to review someone else's stacked PR is to open the PR in a web
browser and look at the merged diff. GitHub's "files changed" tab collapses the
stack into one big diff and loses the structure the author worked to create. The
"files changed by commit" affordance shows one commit at a time, but the
navigation is built for inspection, not for a review pass — there is no way to
walk the stack as a sequence of discrete review units, accumulate intent on
each, and submit the whole thing in one go.

For a small one-commit PR, the browser is fine. For a stacked PR — the shape
that agent-driven development tends to produce — the browser fights the
reviewer:

1. Read the description and the description-thread comments.
2. Look at commit 1's diff, file by file.
3. Notice a problem at a specific line. Capture intent without losing place in
   the walk.
4. Continue to commit 1's other files, then commit 2, then commit 3.
5. Capture commit-level concerns ("commit 2's message lies; the code does X, not
   Y").
6. Capture PR-level concerns ("the rename in commits 4–6 should be its own PR").
7. Submit the whole accumulated intent as a single GitHub review with a verdict.
8. The reviewee addresses on their side. They force-push.
9. Re-review the updated PR.

In a browser, steps 2–6 collapse into "scroll a giant diff and use the
inline-comment textarea." Stack structure is invisible. Drafts get posted
prematurely. Walks get interrupted. Loops break.

The missing primitive is simple:

> Show a PR as a stack. Walk it commit-by-commit. Comment at line, commit, and
> PR scope. Submit the whole thing as one GitHub review.

## Design Thesis

GitHub's review UI is built around a single change. When the change is a stack,
that UI obscures the structure the reviewer most needs to see.

The right local abstraction is a terminal tool that treats a PR as an ordered
list of commits, walks them as the unit of progress, and produces a single
GitHub review at the end. The reviewer never has to leave the terminal during a
review pass, and never has to choose between losing their place and posting
half-formed drafts.

## Principles

These are the load-bearing truths the design rests on. They shape what the tool
does and what it refuses to do.

### ggr is for reviewing other people's PRs

The reviewer is not the author. The reviewer cannot edit the author's commits.
This is the foundational asymmetry that makes ggr a different tool from jjr.

The implications cascade:

- The reviewer's intent goes back to the author through GitHub, not through code
  edits.
- Submitted comments persist on a server the reviewer does not control. Once
  submitted, they are no longer the reviewer's to manage from ggr — edits,
  deletions, and resolutions happen on GitHub itself.
- Force-pushes happen out of band, at the author's discretion, between the
  reviewer's cycles.

Self-review is jjr's job. ggr does not try to support it. A reviewer using ggr
on their own PR is using the wrong tool — the right tool is jjr, before the PR
ever exists.

### A PR is a stack

The PR is identified by `(host, owner, repo, number)`. Its content is the
ordered list of commits between the merge base and the PR head, fetched fresh
from GitHub on each open.

The PR number is the stable handle. The commit list is the moving part —
force-pushes replace SHAs without changing the PR's identity.

This is the analog of jjr's "the resolved revset is the stack." For ggr, the
PR's commit list at fetch time is the stack.

### The GitHub review is the reply

The reviewer's comments are not for the reviewer. They are intent addressed to
the author. The carrier is GitHub's review system: a single submission with
inline comments, a body, and a verdict (approve / request changes / comment).

This is the deliberate counterpart to jjr's "the codebase change is the reply."
In jjr, the reviewer's intent reaches Claude as a packet, and Claude edits code
in place; there is no textual reply because there is no "other side." In ggr,
the reviewer's intent reaches the author as a GitHub review; there is a textual
reply (the comments themselves) because the author is a different person who
needs to read them.

The asymmetry between the tools at this single point of contact — "who acts on
the comments?" — accounts for almost every other difference.

### Stack walking is the load-bearing UX

ggr exists because GitHub does not surface stack structure for review. Every UX
decision must keep the commit-by-commit walk first-class.

- The default screen after the description page is commit 1's first file's diff,
  not the merged PR diff.
- Navigation is `n`/`p` for commits, `Tab`/`Shift-Tab` for files within a
  commit.
- Scope-by-scope: line comments anchor to specific commits. Commit comments
  attach to a specific commit. PR-scope is a separate explicit scope, not a side
  effect of where the cursor was.
- The merged-diff view is not implemented. The reviewer who wants it has the
  browser.

If a future feature would obscure the per-commit walk, it does not belong in
ggr.

### Local drafts, batched submission

The reviewer's drafts live on the reviewer's machine, not on GitHub, until the
reviewer chooses to submit. Submission is a single GitHub review API call: all
inline comments, all replies, the body, and the verdict, in one shot.

This mirrors GitHub's "pending review" concept and turns it into the default
rather than a feature buried two clicks deep. It also keeps drafts safe from the
reviewer accidentally posting them mid-walk.

### Existing GitHub state is read-only context

When the reviewer opens a PR with ggr, the PR may already carry threads from
prior cycles, other reviewers, or the author themselves. ggr renders all of that
inline as context. The reviewer can read it and reply to threads, but cannot
edit existing comments — those belong to whoever authored them, on a server ggr
does not own.

Resolving conversations is also out of scope for the MVP. It is a write action
against shared thread state with subtle semantics; future work, not
foundational.

### The tool does not model "done"

There is no approved-yet flag, no required-comments-outstanding gate, no check
before submit beyond the verdict choice. The reviewer's judgment is the only
completion signal. The submit verb is an explicit action; the reviewer chooses
when to take it.

This is identical to jjr's principle. Pushing happens outside jjr; submit
happens inside ggr but is still the reviewer's decision, not the tool's.

### Fast and deep are both supported

The same principle as jjr. ggr exists because review must be fast enough to
actually happen. It also has to support careful review when the change demands
it. These are not opposing goals. When evaluating any future feature, ask: does
this make the tool support speed-and-depth, or does it favor one at the other's
cost?

## Goals

- Open a GitHub PR by number, owner/repo#number, full URL, or auto-detect from
  the cwd's git remote.
- Render the PR description and existing PR-thread context first.
- Walk the PR's commits in order, file-by-file, line-by-line.
- Render existing review state inline as read-only context: prior reviews,
  inline threads, replies.
- Compose drafts at line, commit, and PR scope, with a severity hierarchy.
- Compose replies to existing threads.
- Submit all drafts at once as a single GitHub review with a verdict.
- Re-review the PR across multiple cycles, with stale local drafts surfaced
  separately and outdated submitted comments displayed accordingly.
- Use the `gh` CLI for all GitHub access, inheriting the user's auth and GHE
  configuration.

## Non-Goals

- Do not run AI review. Claude has no role in the MVP.
- Do not edit other people's comments. Reviewer's own submitted comments are
  also not editable from ggr.
- Do not resolve conversations. Out of scope for the MVP.
- Do not support PR creation, branch checkout, or any local code editing. ggr is
  pure read + comment + submit.
- Do not poll for updates mid-session. ggr operates on the snapshot it fetched
  at open time.
- Do not show the merged-diff view. Stack walking is the only review surface.
- Do not duplicate the GitHub UI. ggr is a stack-walking review tool, not a
  general PR client. Operations beyond the MVP shape (closing PRs, managing
  reviewers, merging, labels, etc.) belong in the browser or `gh` CLI.

## Operating Context

**The reviewer.** A peer or maintainer reviewing someone else's PR. They have
read access to the repo (otherwise they could not see the PR) but not
necessarily push access (they cannot edit the author's commits, by design). They
are working in a terminal, often alongside other work, on the timescale of tens
of minutes per PR rather than days.

**The author.** Off-machine and asynchronous. The reviewer cannot expect the
author to be online. The author addresses review feedback on their own time, on
their own machine, and force-pushes the result. ggr makes no assumption about
how the author works — they may use jjr, a browser, their IDE, or a different
agent entirely.

**The PR.** Often agent-generated, often a stack of three to ten commits, often
touching multiple files per commit. ggr is built for this shape; one-commit PRs
work but the stack-walking UX has no leverage on them.

**The session.** Bounded by the reviewer's attention. A typical session is one
PR, opened, walked, and submitted in a single sitting. The reviewer may quit
mid-walk and reopen later; ggr resumes from the last cursor position. Multi-PR
sessions are out of scope — ggr is invoked per-PR.

**The tools.** `gh` CLI authenticated to the host (github.com or a GHE
instance). A local repo checkout is optional; if present, ggr can use `git show`
for diff fetching, otherwise it fetches via `gh api`. The reviewer's terminal
must be at least 60 columns wide; below that ggr refuses to render.

**The notification mechanic.** Out of band. ggr does not surface "the author has
updated this PR since you last reviewed." The reviewer learns about updates
through GitHub's notification system, email, or a teammate poking them. They
reopen ggr when they're ready for the next cycle.

The reviewer may or may not have a local clone of the repo. ggr does not require
one. The PR number plus `(host, owner, repo)` is sufficient to drive the entire
workflow.

## The Review Cycle

A _cycle_ is one review pass: open, walk, draft, submit. The cycle is the unit
of progress in ggr, identical in shape to jjr's cycle even though the actors and
endpoints differ.

**A cycle begins** when the reviewer invokes `ggr <pr-ref>`. ggr fetches the
PR's current state from GitHub: commits, threads, reviews, description. Local
drafts from previous cycles (if any) are loaded and re-anchored against the
current state.

**During the cycle**, the reviewer walks the stack, captures intent as local
drafts, and may reply to existing threads. Drafts live on disk; nothing is sent
to GitHub until submit.

**A cycle ends** on successful submit. ggr posts the review (one `POST /reviews`
call, plus one `POST /comments` reply call per drafted reply), clears local
drafts on success, and the cycle is complete. The reviewer typically quits ggr
after submitting; quitting without submitting also ends the cycle but leaves
local drafts intact for the next reopen.

**Between cycles**, the author addresses the review on their side, possibly
force-pushing. GitHub records the review and threads. ggr holds no in-memory
state between cycles; everything is reconstructed from disk and from the GitHub
fetch on the next open.

**State that crosses cycles** (lives between invocations):

- Submitted comments and threads (on GitHub).
- Local drafts that were not submitted (on disk).
- Cursor position (on disk).
- The PR number and the `(host, owner, repo)` triple (passed by the reviewer at
  invocation, or auto-detected from cwd).

**State that does not cross cycles**:

- The fetched diff content. ggr re-fetches from `gh api` on each open.
- The fetched commit list. Force-pushes between cycles will return a different
  list.
- Submit success or failure status. Once a cycle ends, ggr forgets it.

The number of cycles is unbounded. ggr does not model "done." The reviewer
decides when to stop reviewing.

## Primary Workflow

```
ggr <pr-ref>
```

Where `<pr-ref>` is one of:

```
ggr 42                                              # auto-detect repo from git remote
ggr acme/myrepo#2429                                # explicit repo, works from any directory
ggr --url https://github.example.com acme/myrepo#42 # GHE host + short form
ggr https://github.com/owner/repo/pull/2429         # full pull URL
```

The reviewer:

1. Sees the PR description page (description body + general PR-thread context).
2. Walks commits with `n`/`p`. Each commit's first file is the entry point;
   `Tab`/`Shift-Tab` cycle files within a commit.
3. Walks lines with arrows or `j`/`k`. Existing inline threads on the current
   line render inline as context.
4. Composes drafts at line, commit, or PR scope. Replies to existing threads
   when the cursor is on a thread.
5. Submits with `S`. A verdict modal asks for approve / request changes /
   comment, defaulting to comment. Submit posts one GitHub review (covering the
   body, line comments, and verdict) plus one reply call per drafted reply
   (GitHub's review API does not accept replies in the review payload; the
   separate `POST /comments` endpoint must be used).
6. On success, local drafts are cleared. On partial failure (review posted but a
   reply failed, or vice versa), drafts that did not reach GitHub remain on
   disk; ggr reports which calls succeeded and which did not.
7. The next cycle begins on the next reopen, after the author has addressed the
   review and updated the PR.

## User Interface Model

The UI mirrors jjr's review surface. Same header, footer, and stack-bar layout.
Same composer modal. Same scope semantics.

The differences from jjr's UI:

- The first screen is the PR description page (PR body + general PR comments).
  jjr has no equivalent because a jj stack has no description.
- Existing GitHub threads render inline beneath the diff line they anchor to,
  with author attribution and timestamps. The reviewer sees the thread in
  context, not in a separate screen.
- A reply composer is a variant of the new-comment composer, anchored to a
  specific thread rather than a fresh anchor.
- The submit modal asks for a verdict (defaulting to `comment`) before
  dispatching to GitHub. `approve` and `request changes` are explicit choices.

The footer, status line, and keybindings reuse jjr's where the underlying action
is identical (navigation, composer entry, scope selection). ggr-specific
keybindings (submit, refresh, reply, thread collapse) are additive.

## Navigation

Identical to jjr's navigation contract. Arrow keys are first-class. Vim-style
keys are convenience bindings. The default profile assumes no Vim knowledge.

```
↓ / j        next diff line
↑ / k        previous diff line
PgDn         next page
PgUp         previous page
Home / g     top of current diff
End / G      bottom of current diff
Tab          next file
Shift-Tab    previous file
n            next commit (or first commit from description page)
p            previous commit (or back to description page from commit 1)
Enter / c    add/edit comment on current line
r            reply to thread on current line (when one exists)
T            toggle thread expansion (collapse/expand all threads)
P            add a PR-scoped comment from any screen
Esc          cancel modal / close composer
S            submit review
R            refresh PR state from GitHub
?            help
q            quit
```

`S`, `R`, `r`, `T`, and `P` are net-new compared to jjr (jjr has no threads to
reply to or collapse, no PR scope as such, and no submit endpoint).

## PR Model

Inputs:

- A PR reference parsed into `(host, owner, repo, number)`.

The PR is fetched via `gh pr view <ref> --json …`. ggr extracts:

- PR title, description body, base branch, head branch, head SHA.
- Ordered list of commits (each with SHA, subject, body, author).
- Existing reviews and inline threads (via `gh api` for comprehensive state —
  `gh pr view`'s thread coverage is incomplete).

Each commit is a stack entry. Each entry carries:

- commit SHA (the durable identifier — stable until force-push)
- commit subject and body
- diff (fetched lazily when the reviewer first navigates to it)
- existing inline threads anchored to lines in this commit
- local drafts anchored to this commit

Force-push semantics: the PR retains its number, but its commit list is
replaced. Threads scoped to old commit IDs become "outdated" on GitHub's side.
Local drafts anchored to vanished SHAs go stale (see Re-Review Semantics).

### Diff source

Per-commit diffs are obtained via `gh api repos/<owner>/<repo>/commits/<sha>` or
by shelling out to `git show` if the repo is checked out locally. Either way,
the parser is `local-review-core`'s `diff::parse` — same unified-diff parser
used by jjr.

The parser handles the same edge cases jjr handles: multi-file diffs, zero-count
hunk headers, function-context after the second `@@`, binary files, renames and
copies, UTF-8 assumption with clear error on invalid encoding.

## Comment Model

A comment is the unit of reviewer intent. ggr supports three scopes:

- **Line-scoped** comments anchor to a specific line in a specific file in a
  specific commit. The default and most common kind. Anchored by target text,
  hunk header, surrounding context, and commit SHA. Line numbers alone are not
  enough for durable anchoring across re-review cycles.
- **Commit-scoped** comments anchor to a commit as a whole. No file, no line.
  Use for commit-level concerns ("this commit does too much, split it" or "the
  message says X but the code does Y").
- **PR-scoped** comments anchor to the PR. Use for cross-cutting concerns
  ("rename `retry_wrapper` to `retry_policy` throughout this PR" or "this should
  be two PRs").

All three scopes share severity semantics: `note` / `suggestion` / `required`.
Severity is informational on the GitHub side — it is rendered into the comment
text on submit, with a leading marker — but it is load-bearing for human
readability and for parity with jjr.

A draft comment carries:

```
schema_version: "ggr-comment/v1"
kind: "comment" | "reply"

# Common to all kinds:
host: string                  # github.com or GHE hostname
owner: string
repo: string
pr_number: integer
body: string                  # free text, no length limit
severity: "note" | "suggestion" | "required"
created_at: string            # RFC 3339
updated_at?: string

# kind = "comment": new top-level comment with one of three scopes
scope?: "line" | "commit" | "pr"        # required when kind = "comment"

# scope = "line" requires:
commit_sha?: string                     # the commit whose diff carries the line
file?: string                           # repo-root-relative POSIX path
side?: "old" | "new"
old_line?: number                       # set when side = "old"
new_line?: number                       # set when side = "new"
hunk_header?: string                    # verbatim "@@ ... @@" line
target_text?: string                    # verbatim, max 1024 chars
context_before?: string[]               # up to 3 lines
context_after?: string[]                # up to 3 lines

# scope = "commit" requires:
# commit_sha (above) — the only anchor field set for commit scope.
# All file/side/line/hunk/target/context fields absent.

# scope = "pr": no anchor fields. The PR_number above is the entire anchor.

# kind = "reply": targets an existing GitHub review comment by its API ID.
parent_comment_id?: string              # required when kind = "reply"
# The reply inherits its anchor from the parent comment on GitHub.
# scope is absent for replies.

# Set by re-anchoring on reopen, not by the composer:
status?: "pending" | "stale"            # see Re-Review Semantics
mismatch_reason?: string                # populated when status = "stale"
```

**Validity rules** (enforced at write time and re-checked on load):

- `kind = "comment"` requires `scope` and the fields listed under that scope's
  "requires" block. Other anchor fields must be absent.
- `kind = "reply"` requires `parent_comment_id`. `scope` and all anchor fields
  must be absent.
- `severity` is always present. The composer defaults it to `note` if the
  reviewer does not pick one.
- `parent_comment_id` is GitHub's _review comment ID_, not a thread ID. GitHub's
  reply endpoint takes the parent comment's ID and replies in the same thread.
  ggr stores the ID exactly as the API returned it on fetch.

### Severity in ggr

In jjr, severity drives Claude's behavior — `required` must be addressed,
`suggestion` is addressed when safe, `note` is informational. In ggr, severity
is rendered as a leading marker in the submitted comment text and is not
consumed programmatically by anything ggr controls. Whether the author treats a
`required` comment differently from a `note` is a human convention between
reviewer and reviewee.

The marker format matches jjr's packet rendering for cross-tool consistency:

- `[REQUIRED]` — must be addressed.
- `[SUGGESTION]` — should be addressed when safe.
- `[NOTE]` — informational.

The marker is the first line of the submitted comment body. Comments without a
marker (e.g., comments authored on GitHub directly) are treated as `note` when
ggr re-fetches them in a later cycle, and the severity field is left empty until
the reviewer overrides it.

The severity hierarchy is preserved both for parity with jjr's shared composer
and because reviewers find it useful to be explicit about the strength of an
ask.

### Existing GitHub state

In addition to drafts, ggr renders the PR's existing review state:

- **Inline review comments and their threads** are rendered as a collapsible
  block immediately beneath the diff line they anchor to. The block shows author
  handle, timestamp, severity (if the comment was submitted by ggr and carries a
  recoverable severity marker), and body. Subsequent replies are indented under
  the parent. The block defaults to expanded; the reviewer can collapse all
  threads with a keybinding to reduce visual noise during a fast walk.
- **The PR description** is rendered as the body of the description page, the
  entry screen.
- **Issue-level PR comments** (the general thread, not tied to a review) are
  rendered as a section beneath the description on the description page, in
  chronological order.

All of the above are read-only. The reviewer can scroll over them, read them,
and reply to threads via the composer, but cannot edit or delete them. Threads
that GitHub has marked outdated (because the commit they anchored to has been
force-pushed away, or the line no longer exists in the diff at all) are rendered
with a visible "outdated" marker and a faded color, but remain visible — the
reviewer may want to read them as historical context for the cycle.

## Comment Storage

Drafts only. Submitted comments live on GitHub.

**Storage location:**

```
~/.local/share/ggr/<host>/<owner>/<repo>/<pr_number>/
  drafts/<commit-sha>.jsonl       # line- and commit-scoped drafts for this commit
  drafts/_pr.jsonl                # PR-scoped drafts (kind = "comment", scope = "pr")
  drafts/_replies.jsonl           # replies (kind = "reply")
  cursor.json                     # last-viewed commit + file + line
```

Per-machine, per-reviewer. Not in the working tree, not in version control, not
coordinated with any other machine the reviewer might use. The reviewer's drafts
are entirely local until submitted.

**Append, edit, delete.** New drafts append a record. Editing a draft rewrites
the JSONL file in place (read-all, modify, atomic write-then-rename) —
append-only is incompatible with in-place edit. Deletion is the same rewrite
path. Atomic rename is used so a crash mid-write cannot corrupt prior drafts.

**On successful submit**, the corresponding draft records are removed from their
JSONL files via the same rewrite path. The files themselves are kept in place
(truncated to empty if no records remain) so the directory structure is stable
across cycles. Drafts that did not reach GitHub remain on disk for retry; see
_GitHub API Mapping_'s partial failure semantics.

**Filename conventions.** The leading underscore on `_pr` and `_replies` avoids
collision with real commit SHAs, which never begin with an underscore.

**Format.** JSONL — same rationale as jjr: simple, inspectable, append-friendly
for the common case of adding new drafts, easy to feed into `jq` or other tools
for debugging.

## Re-Review Semantics

When the reviewer reopens ggr on a PR they have reviewed before:

1. ggr fetches the current PR state from GitHub: commits, threads, reviews,
   description.
2. Local drafts are loaded.
3. For each line-scoped local draft:
   - If the draft's `commit_sha` is still in the PR's commit list, run the
     line-anchoring algorithm against the current diff for `(commit_sha, file)`.
     Re-anchored: `status = "pending"`. Failed: `status = "stale"`, surfaced in
     a stale panel.
   - If the `commit_sha` is no longer in the PR's commit list (the author
     force-pushed and the commit was rewritten or dropped), try to find a
     successor commit by matching the original commit's subject against the new
     commit list. If a unique subject match exists, re-anchor the draft to that
     commit's SHA and run the line-anchoring algorithm there. Otherwise, mark
     stale with `mismatch_reason = "commit not in PR"`.
4. For each commit-scoped local draft:
   - If `commit_sha` is in the PR, `status = "pending"`.
   - If not, attempt subject-based successor match (same as line-scoped). On
     match, re-anchor. On no match, mark stale with
     `mismatch_reason = "commit not in PR"`.
5. PR-scoped drafts are never stale.
6. For each reply draft (`kind = "reply"`):
   - If the `parent_comment_id` is still present in the freshly fetched comment
     list, `status = "pending"`.
   - If the parent comment was deleted from GitHub (rare but possible — the
     author can delete their own comments), mark stale with
     `mismatch_reason = "parent comment deleted"`.
   - Outdated parents (parent comment is still on GitHub but its anchor is now
     outdated due to force-push) are not stale — the reply will still post
     correctly to the existing thread.

The re-anchoring algorithm core is shared with jjr (same fuzzy text + context
match within a hunk). The only ggr-specific addition is the commit-subject
successor heuristic for handling force-pushes.

Stale drafts are surfaced separately, not inline. The reviewer can clear them,
edit them, or ignore them. Stale drafts are excluded from submission unless the
reviewer explicitly re-anchors or upgrades them.

### Mid-session refresh

If the author force-pushes mid-session (the reviewer is in the middle of a walk
and hasn't submitted yet), ggr does nothing automatic. The reviewer can press
`R` to re-fetch state. A submit attempt that fails because anchors are stale
prompts the reviewer to refresh.

This is consistent with the principle that a session operates on the snapshot
fetched at open time. ggr does not poll.

## GitHub API Mapping

Submission posts one GitHub review plus zero or more reply calls in quick
succession. Replies are not part of the review payload — GitHub's review API
does not accept them — so ggr fans them out to the separate reply endpoint.

### Review submission

`POST /repos/{owner}/{repo}/pulls/{pull_number}/reviews`. Fields:

- **`event`** ← verdict: `APPROVE` / `REQUEST_CHANGES` / `COMMENT`.
- **`body`** ← composed from PR-scoped drafts (rendered verbatim) plus
  commit-scoped drafts (rendered as quoted blocks with pointers to the commit
  SHA and subject — see _Why commit-scoped comments go in the body_ below).
- **`comments[]`** ← one entry per line-scoped draft, of the form
  `{path, line, side, commit_id, body}`. The `commit_id` is set to the draft's
  `commit_sha` so the comment anchors to that specific commit, not the PR head.

### Reply fan-out

For each reply draft: `POST /repos/{owner}/{repo}/pulls/{pull_number}/comments`
with `in_reply_to` set to the draft's `parent_comment_id`. One HTTP call per
reply. ggr posts replies serially after the review submission returns
successfully; if a reply fails, ggr surfaces which one and leaves the
corresponding draft on disk.

### Submit ordering and partial failure

The review is posted first. If it fails, no replies are sent and all drafts
remain on disk for retry. If the review succeeds but a reply fails, the review
and all preceding successful replies are on GitHub; the failing reply and any
subsequent unsubmitted reply drafts remain on disk. ggr reports a structured
summary: review status, per-reply status, and which drafts to retry. The
reviewer can re-submit; ggr will skip drafts whose corresponding GitHub object
already exists (detected on next fetch, since drafts are cleared on success).

### Why per-commit anchoring

Setting `commit_id` on inline comments is the explicit GitHub mechanism for
anchoring a comment to a specific commit's diff rather than the PR head. The
trade-off: when the author force-pushes, comments anchored to old commit IDs are
marked "outdated" on GitHub and hidden from the default thread view.

This is accepted, because the alternative — anchoring to the PR head — loses the
"this issue is at commit 3 specifically" information that the reviewer cared
enough about to capture. Outdated-on-force-push is the known cost. It is the
same cost any commit-anchored review accepts.

### Why commit-scoped comments go in the body

GitHub has no PR-review primitive for "this whole commit." Inline comments are
line-anchored. The review body is freeform. So commit-scoped drafts are folded
into the body with explicit attribution:

```
> Commit abc1234 — "implement retry policy"
>
> This commit does too much. The retry policy and the new metric should
> be separate commits.
```

The leading `> Commit <sha-short> — "<subject>"` is the convention. This keeps
commit-level intent visible on the PR review page and preserves the reviewer's
stack-walking mental model in the rendered output.

### Empty-submit handling

A submit attempt produces a GitHub review only when the review carries
something. The matrix:

- **Verdict `APPROVE`** — always valid, regardless of draft count. The approval
  itself is the carry. A reviewer who has read the stack and has nothing to add
  but their endorsement should be able to submit in one keystroke.
- **Verdict `REQUEST_CHANGES`** — always valid, regardless of draft count. The
  block itself is the carry. A reviewer who needs to gate the PR but is
  articulating their reasoning verbally elsewhere is not forced to invent inline
  comments.
- **Verdict `COMMENT` with zero drafts** — error. This would produce a review
  with no opinion and no content. ggr surfaces "nothing to submit; pick approve
  or request-changes if you intended to weigh in without comments" and does not
  invoke the API.
- **Verdict `COMMENT` with at least one draft** — valid.

This mirrors jjr's "no comments to send" error for the analogous case of
`jjr claude` invoked with an empty packet.

### Authentication

Authentication is delegated to `gh`. ggr does not store or handle tokens. If
`gh` is not authenticated, ggr surfaces a clear error pointing the user at
`gh auth login`. GHE hosts inherit `gh`'s configuration (or ggr's `--url` flag,
which sets `GH_HOST` for the subprocess).

## Local-Review-Core Boundary

`local-review-core` is the shared library between jjr and ggr. The boundary is
drawn so that the review experience is identical in both tools, with the
source-of-stack and the submission target as the only per-tool concerns.

Core owns:

- Diff parsing (`diff::Diff`, `DiffFile`, `Hunk`, `Line`, `LineKind`).
- Anchor primitives: `LineAnchor`, `DescriptionAnchor`, `Side`,
  `MismatchReason`, severity, scope semantics.
- Anchor identifier types: `ChangeId` (for jjr's mutable changes), `CommitId`
  (for git SHAs); the comment data model is parameterized over the identifier
  type.
- Anchoring algorithm (`match_anchor`, `match_description_anchor`,
  `AnchorOutcome`).
- Comment data model and JSONL storage (parameterized over identifier and scope
  shape).
- Cursor / resume state.
- Review TUI: header, footer, stack bar, composer modal, scope selectors,
  scrollbars, side-by-side diff, file picker, stale panel, help screen.

Per-tool crates own:

- **jjr**: revset resolution (`jj log`, `jj show`), Claude packet generation
  (`packet.rs`), Claude CLI invocation, `Anchor::Description` on jj change
  descriptions, comment storage layout in `<repo>/.jj-review/`.
- **ggr**: PR resolution (`gh pr view`, `gh api`), per-commit diff fetch, GitHub
  review submission, reply endpoint, comment storage layout under
  `~/.local/share/ggr/`, the PR description page (no jjr equivalent), thread
  rendering and reply composer.

The TUI in core is parameterized over a small interface: how to fetch the stack,
how to fetch a diff for a stack entry, how to render existing-state context (jjr
renders nothing; ggr renders threads), and how to submit. Each tool wires that
interface to its own subprocess / API surface.

The shape of the boundary is set by the principle that the reviewer's experience
should be identical between the two tools. Anything required to keep that
experience identical lives in core. Anything that depends on jj-vs-GitHub
specifics lives in the per-tool crate.

## CLI Surface

```
ggr <pr-ref>                        # open the PR review UI
ggr --url <host> <pr-ref>           # GHE host
ggr <pr-ref> --refresh              # force a fresh fetch ignoring any cached state
ggr <pr-ref> --restart              # clear cursor, open at description page
ggr drafts <pr-ref>                 # show local draft state for this PR (debug/inspection)
ggr clear <pr-ref>                  # clear all local drafts for this PR
ggr clear <pr-ref> --stale          # clear only stale drafts
```

`<pr-ref>` formats are documented under Primary Workflow.

There is no `ggr export`, `ggr packet`, or `ggr claude`. Drafts go to GitHub via
submit; there is no local export for an external consumer in the MVP.

## Data Flow

```
                          (open)
gh pr view <ref> --json …                           # PR metadata + commits
gh api repos/<owner>/<repo>/pulls/<n>/reviews       # prior reviews
gh api repos/<owner>/<repo>/pulls/<n>/comments      # inline review threads
gh api repos/<owner>/<repo>/issues/<n>/comments     # issue-thread comments
                            |
                            v
       ordered commit list + threads + reviews + description
                            |
                            v
                    (per-commit, lazy)
       gh api repos/<owner>/<repo>/commits/<sha>    # diff for one commit
                            |
                            v
                  diff parser  (local-review-core)
                            |
                            v
                terminal review UI  (local-review-core)
                            |
                            v          (compose drafts)
        ~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/drafts/*.jsonl
                            |
                            v          (submit)
       gh api POST .../pulls/<n>/reviews            # one review (body, lines, verdict)
                            |
                            v          (per reply, serial)
       gh api POST .../pulls/<n>/comments           # reply to existing thread
                            |
                            v
            drafts cleared on success; partial drafts retained on failure
```

## Implementation

Rust. Single binary. Same toolchain as jjr.

Crates inherited from the workspace root:

- `ratatui` — terminal UI
- `crossterm` — terminal events
- `serde` / `serde_json` — data model and `gh` JSON parsing
- `time` — RFC 3339 timestamps
- `snafu` — errors
- `local-review-core` — shared review surface (see boundary above)

ggr-specific subprocess shell:

- `gh` CLI for all GitHub access. ggr does not link a Rust GitHub client. This
  keeps auth, host selection, and rate-limiting in the user's existing `gh`
  configuration.
- `tokio` for spawning `gh` subprocesses. Network I/O is hidden behind the
  imperative shell — the functional core (parsing, anchoring, TUI state) remains
  pure.

Async is confined to the gh-subprocess boundary. The TUI loop is synchronous.
Diff fetching is lazy: the first navigation to a commit triggers an
`gh api commits/<sha>` call, parsed, cached for the session.

## Alerting and Observability

ggr is a CLI tool. There is no production service to alert on, no SLO to track,
and no on-call rotation. Observability is local and synchronous: failures
surface to the reviewer at the point of failure.

**Foreground errors.** All errors during normal operation (gh not authenticated,
PR not found, network failure during fetch, submit API errors, malformed
response from `gh`, file I/O errors) surface as human-readable messages on
stderr or in the TUI status bar. The process exits non-zero on fatal errors.
Standard CLI conventions.

**TUI mode log.** While the TUI is active it owns the terminal, so any
unexpected stderr output (panics, `tracing` events, subprocess stderr) cannot be
printed without corrupting the screen. ggr redirects stderr to a log file at
`~/.local/share/ggr/log/ggr-<timestamp>-<pid>.log` for the duration of the TUI
session. On clean exit the file is preserved (so the reviewer can inspect it
after a confusing session); a `--clear-logs` flag or a manual `rm` deletes them.
ggr does not ship logs anywhere.

This mirrors jjr's stderr-redirection pattern. The log file is the only
operational artifact.

**Silent-failure surface.** The two ways ggr can fail silently are:

1. A submit succeeds at the HTTP layer but produces a malformed review on GitHub
   (e.g., a comment lands on the wrong line because ggr computed the wrong line
   number). Detection is on the next reopen, when the reviewer fetches the new
   state and sees the posted comment in an unexpected place. ggr cannot detect
   this itself; the reviewer must read the result.
2. A force-push between fetch and submit causes an inline comment to land on a
   commit that has been rewritten away. GitHub will accept the post (the commit
   ID still exists in the repo's git history even after force-push) but the
   comment lands on a "loose" commit not visible in the PR. ggr detects this on
   next reopen via the stale-anchor logic and surfaces it.

Neither of these triggers a paging surface. The reviewer is the detector.

**Telemetry.** None. ggr does not phone home. No anonymous usage metrics, no
error reporting, no version-check pings. The user is the only observer.

## MVP Scope

1. Open a PR by number, `owner/repo#number`, full URL, or auto-detect from the
   cwd's git remote.
2. Render the PR description page (description body + issue-thread comments) as
   the entry screen.
3. Walk commits with `n`/`p`. Walk files with `Tab`/`Shift-Tab`. Walk lines with
   arrows or `j`/`k`.
4. Render existing GitHub state in place: prior reviews and inline threads
   beneath their anchor lines, issue-thread comments on the description page.
   Outdated threads marked and faded.
5. Compose drafts at line / commit / PR scope, with severity.
6. Compose replies to existing threads.
7. Edit and delete local drafts before submission.
8. Persist drafts locally as JSONL under `~/.local/share/ggr/...`.
9. Re-anchor drafts on reopen using the shared anchoring algorithm plus the
   commit-subject successor heuristic for force-pushes. Mark stale drafts and
   surface them in a stale panel.
10. Submit all drafts in one action (`S`): one GitHub review post plus one reply
    post per drafted reply. Verdict modal defaults to `comment`; `approve` and
    `request changes` are explicit choices.
11. Clear drafts on successful submit. On partial failure, retain drafts that
    did not reach GitHub and report which calls succeeded.
12. `ggr drafts <pr-ref>` lists local draft state for inspection.
13. `ggr clear <pr-ref>` and `ggr clear <pr-ref> --stale` clear all drafts or
    only stale drafts.
14. Refresh PR state mid-session (`R`).
15. Help screen (`?`), quit (`q`), and all jjr-equivalent navigation.

## Later Enhancements

- **Claude as review-orientation layer.** A pre-review pass where Claude reads
  the diff and surfaces context — what the change is for, what to look at, what
  feels surprising. The same capability would serve jjr; the data shape is
  per-commit annotations stored locally, not pushed to GitHub. Out of scope for
  the MVP. **Be careful** — this layer must not contradict jjr's "the codebase
  change is the reply" principle by introducing a textual decline channel.
- **Resolve conversation.** Mark threads resolved from ggr.
- **Edit own submitted comments.** Currently out of scope; once a comment is on
  GitHub, ggr does not modify GitHub-side state beyond the submit action.
- **Multi-reviewer filtering.** Render existing state as-is in the MVP.
  Filtering by author, by review, by resolution status is later work.
- **Difftastic / delta rendering, syntax highlighting.** Inherited from jjr's
  later-enhancement list.
- **Posting non-review comments.** The PR's general thread (issue-style
  comments) is read in the MVP but not written. A `gh-thread` write surface is a
  later enhancement.
- **Open the PR in browser.** A quick keybinding to open the current PR /
  current commit / current line in the browser for tasks ggr doesn't support
  (e.g., merging).
- **Caching and offline mode.** The MVP fetches everything fresh on open. A
  persistent cache for diffs and threads, with an online/offline boundary, is a
  later optimization.
- **Stack-aware notifications.** Tell the reviewer when the author has pushed
  since the last cycle. Out of scope; the GitHub UI already surfaces this.

## Decisions

These resolve previously open questions and are not subject to MVP debate:

- **ggr is not for self-review.** That is jjr. The reviewer using ggr is not the
  author.
- **Stack walking is the primary UX.** The merged-diff view is not implemented
  in the MVP.
- **The reply mechanism is a single GitHub review submission with a verdict.**
  Not Claude. Not per-comment posting. Not a draft export.
- **Line comments are anchored to specific commits via GitHub's `commit_id`
  field.** Outdated-on-force-push is accepted.
- **Commit-scoped comments are folded into the review body** with quoted
  attribution to the commit. No separate per-commit GitHub primitive is used.
- **Existing GitHub state is rendered read-only** with reply support. Edit and
  resolve are not in the MVP.
- **Drafts live in `~/.local/share/ggr/...`** keyed by host/owner/repo/pr, not
  in the working tree.
- **Drafts are cleared on successful submit.** Submitted comments live on
  GitHub. On partial failure (review posted but a reply failed, or any later
  call failed), drafts whose calls did not succeed remain on disk for retry.
- **Verdict modal defaults to `comment`.** `approve` and `request changes` are
  explicit choices.
- **Empty-submit semantics.** Submit with verdict `APPROVE` or `REQUEST_CHANGES`
  is always allowed — the verdict is the carry, even with zero drafts. Submit
  with verdict `COMMENT` and zero drafts is an error: nothing to convey.
- **Mid-session refresh is manual.** ggr operates on the snapshot fetched at
  open time. No polling.
- **All GitHub access goes through `gh` CLI.** No direct Rust GitHub client.
  Auth and GHE host are `gh`'s problem.
- **TUI lives in `local-review-core`.** ggr and jjr are thin source-and-sink
  shells. The reviewer experience is identical between the two tools.
- **Severity is informational on the GitHub side.** Rendered into the comment
  text; not consumed programmatically by anyone ggr controls.
- **No PR description line-anchoring.** ggr has a PR description page; the
  reviewer comments on it at PR scope, not at line scope. jjr's
  `Anchor::Description` is for line comments on jj change messages and has no
  peer in ggr.
- **No Claude in MVP.** Future work, deliberately deferred. The data model has
  room for local annotations if Claude orientation lands later.

## Open Questions

These are unresolved and shape later work. Listed so the next implementer
doesn't pick a side by accident.

### How do PR-level issue-comments map onto ggr's PR scope?

The PR has two distinct comment surfaces on GitHub: review threads (inline +
review body) and the issue-style PR comment thread (the general conversation
tab). The MVP renders the issue thread on the description page as read-only
context. PR-scoped drafts are submitted into the review body, not the issue
thread.

This is a defensible choice — review submissions are the primary review
primitive — but it means the reviewer cannot post to the issue thread from ggr
in the MVP. If reviewers report they want to respond to issue-thread questions
in the same flow, this becomes a later-enhancement write surface.

### How should ggr handle PRs that aren't a stack?

A one-commit PR is a degenerate stack. ggr still walks it, but the "stack
walking" UX has no leverage. There may be an argument for a
single-commit-streamlined flow that skips the description page and goes straight
to the diff. The MVP does not implement this; the description page is always the
entry point.

### What happens when the reviewer's `gh` is authenticated as the PR

author?

ggr does not check. The reviewer is responsible for using the right tool.
Self-review with ggr is not prevented by code, only by the principle. (This is
symmetric to jjr's behavior — jjr does not detect "this is someone else's stack
you shouldn't review" either.)

### Is there a path for the reviewer to apply a `suggestion` block?

GitHub's "suggested change" syntax (\`\`\`suggestion fenced blocks) is a
write-side feature that lets a reviewer propose a literal replacement for a few
lines. ggr could surface this as a special composer mode. Out of scope for the
MVP, but worth keeping in mind if the data model ever encodes the proposed
replacement separately.

## Success Criteria

The MVP is successful if a reviewer can:

1. Open someone else's stacked PR with `ggr <pr-ref>`.
2. Read the description and existing PR thread.
3. Walk each commit's diff, file by file, line by line.
4. See existing review threads inline, attributed to their authors.
5. Compose drafts at line, commit, and PR scope, with severity.
6. Reply to existing threads.
7. Submit all drafts as one GitHub review with a verdict, in one keystroke.
8. Reopen the PR in a later cycle, see prior submitted comments as context, see
   local drafts re-anchored or marked stale, and continue reviewing.

The tool replaces:

```
open https://github.com/<owner>/<repo>/pull/<number>/files
# scroll, comment one at a time, submit individually, lose the stack
```

with:

```
ggr <pr-ref>
# walk the stack, draft locally, submit once
```

## One-Sentence Summary

A local terminal review surface that walks a GitHub pull request as a stack of
commits, captures reviewer intent at line, commit, and PR scope as local drafts,
and submits the lot as one GitHub review with a verdict.
