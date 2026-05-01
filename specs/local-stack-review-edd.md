# Local Stack Review — Engineering Design Document

## Purpose

Build a local terminal tool for reviewing a generated jj stack before it becomes
a pull request.

The tool exists to support the human review pass between agent-generated code
and external PR publication. It lets the reviewer move through a stack of
generated changes, inspect each diff, leave line-specific comments, and
explicitly hand those comments to Claude CLI for remediation.

This is not a PR system. It is not a dashboard. It is not an AI reviewer.

It is a local review surface for generated code before that code leaves the
workstation.

## Core Problem

The current local review primitive is effectively:

```
jj show | less
```

That works for reading a diff. It does not support the actual review loop:

1. Inspect the generated change.
2. Notice a problem at a specific line.
3. Capture the concern without leaving the review context.
4. Continue through the rest of the stack.
5. Hand the accumulated reviewer intent back to the agent.
6. Re-review the result before creating or updating a PR.

Today, that loop falls apart into loose prose, copy/paste, scattered notes, and
ad hoc Claude prompts.

The missing primitive is simple:

> Show a diff. Comment on the diff. Move through the stack.

## Design Thesis

Generated code needs a local arbitration pass before PR creation.

The reviewer should not have to choose between reading raw diffs in a pager or
creating a PR just to get review UI affordances.

The right local abstraction is a terminal diff review tool that understands jj
stacks and captures human comments as structured state. The comments are not
decoration. They are the handoff contract between human judgment and agent
remediation.

## Principles

These are the load-bearing truths the design rests on. They shape what the tool
does and what it refuses to do.

### This tool is for jj users

The reviewer is using jujutsu. Claude edits each change in place — that is how
jj works. Commits are mutable; the change ID is the stable handle, the content
moves under it. A reader bringing git instincts (commits are sacred, fixes go in
new commits) will misread the whole loop. We don't accommodate that mental
model. We assume jj.

### The review cycle is the unit of progress

A cycle is one pass: reviewer reads the stack, leaves comments, hands them to
Claude, Claude edits the codebase, reviewer re-reviews. Cycles repeat until the
reviewer is satisfied. There is no fixed number of cycles, no upper bound, no
built-in expectation that one cycle is enough. The tool is shaped to make the
cycle cheap so the reviewer takes as many as the work warrants.

### Claude addresses by editing the code; the fix is the reply

When the reviewer hands Claude a packet of comments, Claude responds by editing
the working copy at the appropriate change. There is no decline-with-reasoning
channel, no separate textual response, no summary report. If Claude cannot
safely address a comment, Claude leaves it. The reviewer sees what changed and
what didn't on the next cycle and adjudicates from the diff.

This is non-obvious and load-bearing. It means the tool does not need to render
Claude's prose anywhere — there is no Claude prose to render. It also means the
reviewer's only feedback channel into Claude's reasoning is the next round of
comments.

### The jj revset is the source of truth

The tool's view of "the stack" is whatever the resolved revset returns at
invocation time. If a change leaves the stack — abandoned, dropped, rebased away
— its on-disk comments persist but become unreachable. They are treated as
unreviewed. This is acceptable: the change is no longer something the reviewer
is reviewing. The comments are not lost (they sit in `.jj-review/comments/`),
they are simply not in scope.

### The tool does not model "done"

There is no "approved" state. No required-comments-outstanding gate. No
quit-time summary that hints at incomplete work. The tool surfaces — comments,
staleness, cursor position — and lets the reviewer decide. Pushing is the
implicit terminus, and pushing happens outside this tool. The reviewer's
judgment is the only completion signal, and the tool stays out of its way.

### Fast and deep are both supported

The tool exists because review must be fast enough to actually happen. It also
has to support careful review when the change demands it. These are not opposing
goals; they are different modes of the same loop. Affordances should not
penalize either. When evaluating a future feature, ask: does this make the tool
support speed-and-depth, or does it favor one at the other's cost?

## Goals

- Review a generated jj stack locally before PR creation.
- Navigate between changes in the stack.
- View each change as a diff.
- Add, edit, and delete comments on specific diff lines.
- Persist comments locally per change.
- Support normal terminal navigation, including arrow keys.
- Support configurable keybindings, including a Dvorak-friendly profile.
- Export comments as structured data.
- Generate a Claude CLI prompt from comments and relevant diff context.
- Keep review and remediation as separate, explicit steps.

## Non-Goals

- Do not build a hosted review system.
- Do not require GitHub.
- Do not create PRs in the MVP.
- Do not post GitHub comments in the MVP.
- Do not run AI review automatically.
- Do not invoke Claude automatically whenever a comment is written.
- Do not solve CI, ownership, approval, merge, or reviewer assignment.
- Do not build a general-purpose IDE.

## Operating Context

The expected workflow is agent-generated stacked development using jj.

A stack of changes is produced locally. Before those changes become PRs, a human
reviewer performs a local review pass. The reviewer may decide that some changes
are good, some need small corrections, and some should be reworked.

This tool provides the review surface for that pass.

## Primary Workflow

```
jjr [<change-id>]
```

If no argument is provided, walk the full stack (`trunk()..@`). Providing a
change ID or revset expression reviews that single change.

Example invocations:

```
jjr
jjr @
jjr 'main..@'
jjr 'ancestors(@) & mutable()'
jjr --stack
```

Inside the tool, the reviewer can:

1. Move through the diff.
2. Add a comment to the current diff line.
3. Move to the next or previous file.
4. Move to the next or previous change in the stack.
5. Continue commenting.
6. Export comments.
7. Send comments to Claude CLI explicitly.

## User Interface Model

The UI is a terminal diff viewer with comments.

The main view shows:

- current change position in stack
- change id / short identifier
- change description
- current file path
- diff content
- visible comment markers
- status line with available actions

Example status line:

```
Change 2/5  src/foo.rs  ↑↓ move  Enter comment  Tab next file  n next change  C send to Claude  q quit
```

## Navigation

The tool must support both normal terminal navigation and optional Vim-style
navigation.

Arrow keys are required and first-class.

Required movement bindings:

```
↓ / j        move down one diff line
↑ / k        move up one diff line
→ / l        move into / expand / next logical item where applicable
← / h        move out / collapse / previous logical item where applicable
PageDown     move down one page
PageUp       move up one page
Home         jump to top of current diff
End          jump to bottom of current diff
n            next change in stack
p            previous change in stack
Tab          next file
Shift-Tab    previous file
Enter        add/edit comment on current line
c            add/edit comment on current line
Esc          cancel modal / close editor
q            quit
```

The default experience must work for someone who does not know Vim bindings.
Vim-style keys are convenience bindings, not the primary interface.

## Keybindings

Keybindings must be configurable.

The tool ships with profiles:

```
default
vim-qwerty
vim-dvorak
```

Default bindings prioritize normal terminal expectations. Arrow keys always
work. Character bindings are configurable.

Suggested config (`.jj-review/config.toml`):

```toml
[keybindings]
profile = "vim-dvorak"

[keybindings.custom]
move_down = ["Down", "j"]
move_up = ["Up", "k"]
next_change = ["n"]
previous_change = ["p"]
comment = ["Enter", "c"]
```

## Stack Model

The tool operates over an ordered list of jj changes.

Inputs:

- explicit revset from the user
- default current change `@`
- `--stack` flag, defined as `trunk()..@` evaluated against the current working
  copy. This depends on `trunk()` being resolvable (jj's revset alias for the
  trunk branch, configurable via `revset-aliases.'trunk()'`). If the revset
  evaluation errors or returns an empty set, fall back to `@` and emit a
  warning. The exact revset is documented and not inferred dynamically.

Each stack entry includes:

- change id
- commit id if available
- description
- parent/child position
- diff
- associated local comments

### Divergent changes

When a change ID has multiple visible commits (divergence), jj disambiguates
with a `/<index>` suffix: `abc/1`, `abc/2`. jjr treats each variant as a
distinct stack entry keyed by the disambiguated change ID. No special UI
handling, no per-variant filtering — divergent variants are just changes that
happen to share a prefix.

Behavior is explicit:

- `jjr @` reviews only the current change.
- `jjr <change-id>` reviews a single change. First-class use case: returning to
  one specific change to address feedback or re-review after Claude edits.
- `jjr <revset>` reviews the ordered changes returned by that revset.
- `jjr --stack` uses the documented default stack revset above. Resumes at the
  oldest change with pending review work, not always at index 1 (see
  Resumability).

No clever inference beyond this.

## Resumability

For deep stacks, sequential review takes time and the reviewer often pauses
partway. The tool maintains a per-stack cursor so they resume where they
stopped.

State is stored at `.jj-review/cursor.json` keyed by a hash of the resolved
revset:

```json
{
  "revsets": {
    "<hash>": {
      "revset": "trunk()..@",
      "last_change_id": "abc333",
      "updated_at": "2026-04-29T14:22:01Z"
    }
  }
}
```

Resume rule for `jjr --stack`:

1. If a cursor exists for this revset and `last_change_id` is still in the
   resolved stack, open at the _next_ unreviewed change after `last_change_id` —
   or `last_change_id` itself if it has no comments yet.
2. If no cursor exists or `last_change_id` is no longer in the stack, open at
   the oldest change.
3. `jjr --stack --restart` clears the cursor and opens at the oldest change.

The cursor advances when the reviewer moves forward in the stack (`n`). It does
not advance on `p`. Quitting the tool persists the cursor at the last-viewed
change.

## Diff Source

Diffs are obtained via `jj show <change> --git --color=never`. Always pass
`--color=never`; `jj` may emit ANSI codes by default depending on terminal
detection.

`jj show` returns a single unified diff covering all files in the change. The
parser splits per-file on `diff --git` headers and processes each file
independently. A parse failure on one file is logged and that file is excluded
from review; remaining files are unaffected.

The unified diff parser must handle:

- **Multifile diffs:** split per-file as above.
- **Hunk header edge cases:** zero counts (`@@ -0,0 +5,12 @@` for pure
  additions, `@@ -3,5 +3,0 @@` for pure deletions) and trailing function context
  after the second `@@` (`@@ -10,5 +10,7 @@ impl Client {`). Both are valid and
  must parse.
- **Binary files:** detected by the `Binary files ... differ` header. The UI
  shows "Binary file not shown" and disables line comments for that file.
- **Renames and copies:** the diff header carries `rename from` / `rename to`
  (or `copy from` / `copy to`). The tool tracks the new path. Comments anchored
  to a renamed file in a prior change may not re-anchor cleanly across the
  rename and become stale; this is acceptable in the MVP.
- **File encoding:** UTF-8 is assumed. Files with invalid UTF-8 bytes surface a
  clear error and are excluded from review with a warning, not a crash.

## Comment Model

A comment is the unit of reviewer intent. It comes in three scopes — line,
change, and stack — corresponding to where the intent attaches.

- **Line-scoped** comments anchor to a specific line in a specific file in a
  specific change. This is the default and the most common kind. Anchored by
  target text, hunk header, and surrounding context so the comment can be
  re-associated after small edits. Line numbers alone are not enough for durable
  anchoring.
- **Change-scoped** comments anchor to a change as a whole. No file, no line.
  Use these for change-level concerns like "this change does too much, split it"
  or "the description doesn't match the code."
- **Stack-scoped** comments anchor to the resolved revset. Use these for
  cross-cutting concerns like "rename `retry_wrapper` to `retry_policy`
  throughout" or "don't introduce new public APIs in this stack."

All three scopes share severity semantics and persist across review cycles.

```
type Comment = {
  schema_version: "diff-comment/v2";
  scope: "line" | "change" | "stack";
  repo_root: string;            // absolute path
  revset: string;               // the revset jjr was invoked with

  // Set for scope = "line" or "change"; absent for scope = "stack"
  change_id?: string;
  commit_id?: string;

  // Set only for scope = "line"
  file?: string;                // repo-root-relative POSIX path
  side?: "old" | "new";
  old_line?: number;
  new_line?: number;
  hunk_header?: string;         // verbatim "@@ ... @@" line
  target_text?: string;         // verbatim line content, max 1024 chars, truncated with ellipsis if longer
  context_before?: string[];    // up to 3 preceding lines, verbatim
  context_after?: string[];     // up to 3 following lines, verbatim

  comment: string;              // free text, no length limit
  severity: "note" | "suggestion" | "required";
  created_at: string;           // RFC 3339
  updated_at?: string;          // RFC 3339
  status?: "pending" | "stale" | "orphaned"; // see Re-Review Semantics
  mismatch_reason?: string;     // populated when status is "stale"; see anchoring algorithm
};
```

The schema bumps to `diff-comment/v2` to add `scope`. v1 records (line-scoped,
missing the `scope` field) are read as `scope: "line"` for backward
compatibility and rewritten on next save.

### Severity semantics

Severity applies uniformly across all three scopes.

- `required`: Claude addresses this by editing the code at the appropriate
  location. Failure to address surfaces in the next cycle as an unchanged diff
  at the comment's anchor.
- `suggestion`: Claude addresses when safe and consistent with the existing
  design. If a suggestion would broaden scope, contradict change intent, or
  introduce risk, Claude leaves it. There is no decline-with-reasoning text —
  the diff (or its absence) is the response.
- `note`: informational only. Included in packets and exports. Claude does not
  act on notes unless the reviewer explicitly upgrades them.

## Comment Storage

Comments are stored locally in the repository under `.jj-review/comments/`:

```
.jj-review/comments/<change-id>.jsonl   # line- and change-scoped comments for a single change
.jj-review/comments/_stack.jsonl        # stack-scoped comments, keyed in-record by revset hash
```

Line- and change-scoped comments for a given change live in the same per-change
file. The `scope` field on each record distinguishes them. This keeps related
intent for one change in one place.

Stack-scoped comments live in `_stack.jsonl`. Multiple stacks (different
revsets) coexist in the same file; each record carries the `revset` it was
authored against, and the file is filtered by revset hash on load. The leading
underscore in `_stack` avoids any collision with a real change ID (jj change IDs
do not begin with underscores).

For divergent changes, the disambiguator slash is replaced with an underscore in
the per-change filename only: change ID `abc/1` is stored at
`.jj-review/comments/abc_1.jsonl`. The in-memory `change_id` field retains the
canonical jj form (`abc/1`); the underscore substitution is purely a filesystem
encoding to avoid creating directory hierarchies for divergent variants. JSON
serialization round-trips the canonical form.

JSONL: simple, inspectable, append-friendly, easy to feed into other tools.

`.jj-review/` is added to `.gitignore` and `.jjignore` on first run,
idempotently. Each file is created if missing; existing entries are not
duplicated. This is local reviewer state and is not committed.

## Re-Review Semantics

After Claude edits the working copy via `jjr claude`, the underlying diff
changes. Comments anchored to that diff may no longer match. The behavior
depends on scope.

MVP behavior:

1. Comments persist verbatim across cycles. They are not auto-deleted.
2. On reopening `jjr`, the tool re-resolves the revset and runs the appropriate
   freshness check per scope (below).
3. **Line-scoped:** re-read the change diff and run the line-anchoring
   algorithm. Re-anchored → `status = "pending"`. Failed → `status = "stale"`,
   surfaced in the stale panel, not inline.
4. **Change-scoped:** if the change is still in the resolved stack,
   `status = "pending"`. If the change is no longer in the stack,
   `status = "orphaned"`.
5. **Stack-scoped:** never stale, never orphaned. They attach to the revset,
   which is what the reviewer chose to review. They reappear at the top of every
   cycle's packet until cleared.
6. Stale comments can be cleared (`jjr clear --stale`), edited and manually
   re-anchored, or carried forward as-is. Orphaned comments persist on disk but
   are not surfaced in the UI; a future `jjr orphans` view will list them.

The distinction matters: **stale** means the anchor moved (the comment may or
may not still be valid; the reviewer adjudicates). **Orphaned** means the change
vanished from scope (the comment is no longer something the reviewer is
reviewing, by definition of #6 above). They are different conditions and get
different treatment.

Claude addressing a comment by editing the code typically results in the comment
going stale on the next cycle (the target text moved or vanished). This is
expected and not a problem — the reviewer reads the new diff and decides whether
the original concern is now resolved.

### Line-anchoring algorithm

For each line-scoped comment, against the current diff for the same
`(change_id, file)`:

1. **Locate the hunk.** If the comment's `hunk_header` carries function-context
   (the segment after the second `@@`), find the current hunk in the same file
   with matching function-context. Otherwise, consider all hunks in the file.
   Line-number ranges in the hunk header are not used for matching — they shift
   on every edit.

2. **Exact match within the hunk.** Search for a line where `target_text`
   matches exactly _and_ `context_before` and `context_after` match exactly
   (within the available window — fewer than 3 lines is acceptable at hunk
   boundaries). On unique match: re-anchor, `status = "pending"`. On multiple
   exact matches (repeated identical lines), prefer the match closest to the
   original `display_line_number` if recorded; otherwise mark stale.

3. **Fuzzy match within the hunk.** If exact match fails, search for
   `context_before + context_after` with any line between them, _or_
   `target_text` with matching context on one side only. On unique fuzzy match:
   mark stale with `mismatch_reason` populated (`"target_text changed"`,
   `"context_before changed"`, `"context_after changed"`). The comment is
   surfaced in the stale panel with the reason shown so the reviewer can decide
   quickly.

4. **No match in file.** Mark stale with `mismatch_reason = "anchor not found"`.

5. **File no longer in diff.** Mark stale with
   `mismatch_reason = "file not in diff"`.

6. **Change no longer in resolved stack.** Mark `status = "orphaned"`. Skip
   line-anchoring entirely; the change isn't in scope.

The MVP does not parse anything Claude does to mark comments as "addressed" —
there is no Claude reply to parse, by design. The diff is the only signal, and
the reviewer reads it.

## Claude CLI Integration

Claude is invoked explicitly, not automatically. The review act and the
remediation act are separate steps in the cycle:

```
review -> comment -> continue reviewing -> send to Claude -> Claude edits -> next cycle
```

Required commands (MVP):

```
jjr claude @
jjr claude <revset>
```

Inside the UI:

```
C   send current change's comments to Claude
```

The MVP supports current-change Claude handoff. Stack-wide handoff
(`jjr claude --stack`, `A` keybinding) is a later enhancement.

### What Claude does with a packet

Claude reads the packet and edits the working copy at the appropriate change.
That is the entire interaction:

- Claude does not produce a textual reply, summary, or status report.
- Claude does not annotate the packet, mark comments as addressed, or write to a
  log the tool reads.
- If Claude cannot safely address a comment, Claude leaves the relevant code
  alone. The reviewer reads the new diff on the next cycle and adjudicates from
  what they see.

The codebase is the only channel back. This is deliberate. See the Principles
section.

### Why Claude does not run on each comment

Invoking Claude as each comment is made creates review instability:

1. Reviewer comments on line 143.
2. Claude edits the file.
3. The diff changes underneath the reviewer.
4. Line anchors shift.
5. The reviewer loses place.
6. Remaining comments may become stale mid-pass.

Batch mode preserves a stable review surface and a clear human decision
boundary.

Later, the tool may support explicit per-comment remediation:

```
Ctrl-A    ask Claude to address this one comment
```

But comment-save does not trigger Claude.

### Invocation behavior

- The packet is generated, written to a temp file, and passed to Claude CLI.
- Stale comments are excluded from the packet by default. Pass `--include-stale`
  to include them.
- Orphaned comments are always excluded from packets — the change isn't in
  scope. (`--include-stale` does not include orphaned.)
- Stack-scoped comments for the resolved revset are always included when
  invoking via `jjr claude --stack` or any revset-spanning invocation. For
  per-change `jjr claude @` or `jjr claude <change>`, stack-scoped comments are
  also included so Claude has the cross-cutting context, even though only one
  change is being acted on.
- If there are no `pending` comments for the target revset (across all scopes),
  `jjr claude` errors with "no comments to send" and does not invoke Claude.
- Claude's exit code is captured. On non-zero exit, the tool reports the error.
  The jj working copy reflects whatever edits Claude made before failing; the
  tool does not attempt to roll back.
- On successful return, the tool re-runs `jj show` for the affected change(s)
  and prompts the reviewer to re-enter the review UI to inspect the new state.
  This re-entry begins the next review cycle.

## Packet and Claude Commands

These are related but distinct:

- `jjr packet [revset]` generates the review packet and writes it to stdout (or
  `-o <path>`). It does not invoke Claude. Useful for inspection, debugging, and
  piping.
- `jjr claude [revset]` generates the same packet and invokes Claude CLI with it
  interactively. Roughly equivalent to `claude "$(jjr packet [revset])"`, but
  managed by the tool so working-copy state and exit handling are consistent.
  Claude runs interactively (no `-p`) so the user can approve edits in real
  time; Claude takes over the terminal for the session and returns control on
  exit.

`jjr packet` is the inspectable artifact. `jjr claude` is the action.

## Review Packet

Packet contents:

- repo root (absolute path)
- revset
- stack-scoped comments (rendered first; shape Claude's overall approach)
- stack entries in order
- for each entry: change id, commit id, description, change-scoped comments,
  file list, line-scoped comments grouped by file, relevant hunks

Packet is rendered as the Claude prompt below.

## Claude Prompt Format

The prompt is generated deterministically from the packet. The format is fixed.

### Template

```
You are editing a local jj working copy.

A human reviewer reviewed a stack of generated changes and left comments at three
scopes: stack-level (cross-cutting concerns), change-level (concerns about a whole
change), and line-level (concerns about specific diff lines).

Your job:
1. Address each comment by editing the code at the appropriate location.
2. Required comments must be addressed. Suggestion comments should be addressed
   when safe and consistent with the change's existing design. Notes are
   informational; do not act on notes unless explicitly asked.
3. Preserve the original intent of each change. Make the smallest safe edits.
4. Do not broaden scope. Do not rewrite unrelated code.
5. If you cannot safely address a comment, leave the relevant code alone. Do not
   write justifications, summaries, or status reports — the reviewer reads the
   resulting diff on the next cycle and adjudicates. The codebase is the reply.
6. Edit changes in place using jj's mutability model. Do not create new fix-up
   commits unless the comment explicitly asks for one.

Repository: <repo_root>
Revision: <revset>

## Stack-Level Review Comments

<rendered, one block per stack-scoped comment, in created-at order; section omitted if none>

## Changes

<for each change in the stack, in stack order:>

  Change ID: <change_id>
  Commit: <commit_id>
  Description: <change description>

  ### Change-Level Review Comments

  <rendered, one block per change-scoped comment; subsection omitted if none>

  ### Line-Level Review Comments

  <rendered, one block per line-scoped comment, in file order then line order; subsection omitted if none>

  ### Relevant Diff Context

  <one rendered hunk per file with line-scoped comments, full hunk including 3 lines context;
   subsection omitted if no line-scoped comments for this change>
```

Sections with no content are omitted entirely (no empty headers). A packet with
only stack-level comments does not render a "Changes" section header for changes
that have no comments of their own — it lists changes that have comments and
their diff context, in stack order.

### Comment block rendering

Line-scoped:

```
### [<SEVERITY>] <file>:<line> (<side>)
Hunk: <hunk_header>
Target line:
    <target_text>
Context:
    <context_before[0]>
    <context_before[1]>
    <context_before[2]>
>>> <target_text>
    <context_after[0]>
    <context_after[1]>
    <context_after[2]>

Comment:
<comment text, verbatim, no reflow>
```

Change-scoped:

```
### [<SEVERITY>] (change-level) <change_id>

Comment:
<comment text, verbatim, no reflow>
```

Stack-scoped:

```
### [<SEVERITY>] (stack-level)

Comment:
<comment text, verbatim, no reflow>
```

### Concrete example

```
### [REQUIRED] src/client.rs:142 (new)
Hunk: @@ -140,7 +140,12 @@ impl Client {
Target line:
    let resp = self.inner.request(req).await?;
Context:
    pub async fn send(&self, req: Request) -> Result<Response> {
        let req = self.prepare(req)?;
>>> let resp = self.inner.request(req).await?;
        Ok(resp)
    }

Comment:
This bypasses the retry policy configured on Self. The rest of the module
is built around honoring that policy; this path needs to call the retry
wrapper, not the inner client directly.
```

The format is stable. Downstream tooling (parsers, evals, future inter-cycle
diff features) can rely on it.

## CLI Surface

```
jjr                                   # open stack review UI (trunk()..@ by default)
jjr [revset]                          # open single-change review UI
jjr --stack                           # open stack review UI (explicit; same as bare jjr)
jjr export [revset]                   # export comments, default jsonl
jjr export [revset] --format markdown
jjr export [revset] --format jsonl
jjr packet [revset]                   # write review packet to stdout
jjr packet [revset] -o <path>         # write review packet to file
jjr claude [revset]                   # generate packet and invoke Claude CLI (current change in MVP)
jjr claude [revset] --include-stale   # include stale comments in packet
jjr clear [revset]                    # clear all comments for revset
jjr clear [revset] --stale            # clear only stale comments
```

## Data Flow

```
jj revset (source of truth)
   |
   v
ordered stack entries
   |
   v
jj show <change> --git
   |
   v
diff parser
   |
   v
terminal review UI
   |
   v
.jj-review/comments/<change-id>.jsonl   (line + change scope)
.jj-review/comments/_stack.jsonl        (stack scope, keyed by revset hash)
   |
   v
review packet generator  ──>  jjr packet (stdout)
   |
   v
Claude CLI prompt  (stack-scope at top, then changes with their change-scope and line-scope comments)
   |
   v
claude  (invoked by jjr claude — edits codebase in place)
   |
   v
next review cycle
```

## Implementation

Rust. Single binary. Same toolchain as the rest of the internal stack.

Crates:

- `ratatui` — terminal UI
- `crossterm` — terminal events
- `serde` / `serde_json` — data model
- `time` — RFC 3339 timestamps
- `anyhow` / `thiserror` — errors
- `unidiff` or equivalent unified-diff parser — diff parsing (do not roll a
  parser)
- `tokio` — subprocess execution for `jj` and `claude`

A Python/Textual prototype would be faster to first screen, but produces a
throwaway. Skip the prototype; iterate in Rust.

## MVP Scope

1. Open a single jj diff.
2. Navigate with arrow keys and basic keyboard shortcuts.
3. Add, edit, delete line-scoped comments on changed lines.
4. Add, edit, delete change-scoped and stack-scoped comments.
5. Persist comments locally as JSONL.
6. Open a revset as a stack and navigate next/previous change.
7. Re-anchor line-scoped comments on reopen; mark stale where anchoring fails.
   Mark change-scoped comments orphaned when their change leaves the stack.
8. Export comments (`jjr export`).
9. Generate review packet (`jjr packet`) including all three comment scopes.
10. Invoke Claude CLI explicitly for the current change (`jjr claude @`).

## Later Enhancements

- **Stack-wide Claude handoff** (`jjr claude --stack`).
- **Inter-cycle diff.** Show the reviewer what Claude changed between cycle N
  and cycle N+1. Because jj rewrites changes in place, `jj show` only ever shows
  the cumulative state — not what moved this cycle. The tool would snapshot the
  change content at packet-send time and diff against the post-edit state. This
  is the highest-leverage future feature: it is the surface that makes "Claude
  addressed by editing the code" legible to the reviewer at a glance, without
  requiring Claude to produce a textual report.
- **Orphaned-comment surfacing** (`jjr orphans`): list comment files for change
  IDs not present in any current revset. When a change is abandoned, undone, or
  dropped via `jj abandon` / `jj undo` / `jj rebase`, its comments persist on
  disk but are not currently reachable through the UI. The principle
  (`the jj revset is the source of truth`) means orphan recovery is opt-in, not
  automatic.
- **Mark comments as resolved/unresolved manually.**
- **Show comment markers in the diff gutter.**
- **Difftastic rendering.**
- **Delta-style rendering.**
- **Syntax highlighting.**
- **Side-by-side diff mode.**
- **Import/export GitHub review comments.**
- **Create PRs after local review passes.**
- **Per-comment remediation** (`Ctrl-A`).
- **Include test failures or CI output** in Claude prompt.
- **Multiple reviewers.**

## Decisions

These resolve previously open questions and are not subject to MVP debate:

- **Default stack revset:** `trunk()..@`. Fall back to `@` if empty or if
  `trunk()` is unresolvable. Bare `jjr` defaults to stack mode using this
  revset; `jjr <change-id>` reviews a single change.
- **Claude scope:** one change at a time in MVP. Stack-wide later.
- **Comment commitment:** never. `.jj-review/` is added to `.gitignore` and
  `.jjignore` on first run.
- **Commentable lines:** added and removed lines only in MVP. Context lines
  later.
- **Automatic Claude invocation on comment save:** never.
- **Re-review staleness:** detected by `target_text` + context match. Stale
  comments persist and are shown separately, not inline.
- **Note severity in Claude packet:** included for context, Claude does not act
  on them.
- **Stale comments in Claude packet:** excluded by default. Use
  `--include-stale` to include them.
- **Empty packet:** `jjr claude` errors out rather than invoking Claude with no
  comments.
- **Divergent changes:** treated as distinct stack entries keyed by jj's
  disambiguated change ID (`<change-id>/<index>`). Storage filename replaces the
  slash with an underscore (`abc/1` → `abc_1.jsonl`); the canonical `change_id`
  is preserved in the comment data.
- **Resume position:** `jjr --stack` resumes at the next unreviewed change after
  the last cursor position. Use `--restart` to force from oldest.
- **Address contract:** Claude addresses by editing the code. There is no
  decline-with-reasoning channel, no textual reply, no summary report. The
  codebase change is the reply.
- **"Done" semantics:** not modeled. The reviewer's judgment is the only
  completion signal; pushing happens outside the tool.
- **Stack source of truth:** the resolved revset at invocation. Changes that
  leave the stack are orphaned; their comments persist on disk but are not
  surfaced.
- **Comment scopes:** line, change, and stack — all supported in MVP.
  Stack-scoped comments are included in every packet (including per-change
  packets) so Claude has the cross-cutting context.
- **Prompt authoring:** prompts are generated from comments, never edited by the
  reviewer. To change what Claude sees, change the comments.

## Open Questions

These are unresolved and shape later work. Listed so the next implementer
doesn't pick a side by accident.

### How does Claude communicate back to the reviewer when it needs to?

The principle is settled: the codebase change is the reply, and there is no
textual decline channel. But genuine cases remain where Claude has a question
rather than a code change — "the comment asks for X, but Y already does it; do
you want both?" or "this conflicts with another comment three changes earlier."

Two sketched directions, neither selected for MVP:

1. **Running Claude Code session.** The reviewer keeps a Claude Code session
   open in another terminal. `jjr claude` injects the packet into that session.
   Claude can ask follow-ups conversationally. The reviewer answers in the same
   session. Pro: minimal new surface, leverages an existing tool. Con: requires
   a Claude Code session to be running and configured; failure modes when it
   isn't are awkward.

2. **Well-known location.** The reviewer's comments are written to a known path
   (already are, under `.jj-review/`). The reviewer tells Claude (in any
   session, any tool) "go address the comments." Pro: tool-agnostic, decouples
   jjr from Claude Code specifically. Con: more human steps, more room for the
   reviewer and Claude to disagree about which packet is current.

The shared aspiration: fewer human actions in the loop. MVP does not pick.
`jjr claude` runs Claude CLI once with the packet and exits; if Claude needs to
ask, Claude has nowhere to ask in MVP.

This question is explicitly left open. A solution should preserve the existing
principle (the codebase change is the primary reply) and only handle the edge
case of genuine ambiguity.

## Success Criteria

The MVP is successful if a reviewer can:

1. Generate a stack locally.
2. Run `jjr --stack` or `jjr <revset>`.
3. Review each change diff in the terminal.
4. Add comments at line, change, and stack scope.
5. Move to the next and previous changes in the stack.
6. Export comments.
7. Explicitly invoke Claude CLI to address comments.
8. Re-review the modified stack across multiple cycles, with stale comments
   surfaced separately and orphaned comments dropped from view.

The tool replaces:

```
jj show | less
```

plus loose prose to Claude with:

```
jjr --stack
jjr claude @
```

## One-Sentence Summary

A local terminal review surface for jj stacks generated by an agent, capturing
reviewer intent at line, change, and stack scope, then handing it to Claude CLI
as a fixed-format packet that Claude responds to by editing the code in place.
