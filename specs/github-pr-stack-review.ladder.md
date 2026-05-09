## Phase 1: Shared review TUI; ggr shows existing GitHub state

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| 🟡 in-progress  | 2026-05-09 |            |

Move the review TUI from `crates/jjr/src/tui*` into
`crates/local-review-core/src/tui/`, parameterized by a `ReviewSurface` trait.
The trait abstracts: the ordered stack of entries (each with identifier,
description, lazy diff), how to fetch the diff for an entry, how to render
existing-state context (no-op for jjr; thread blocks for ggr), and
composer-entry hooks (stubs until P2).

Per the spec at `specs/github-pr-stack-review.md` (_Local-Review-Core
Boundary_): the reviewer's experience is identical between jjr and ggr; what
differs is the source-of-stack and the submission target. Anything required to
keep that experience identical lives in core. ggr and jjr are thin
source-and-sink shells around it.

The user-visible delivery for this phase is ggr's existing-state rendering. ggr
fetches:

- `gh pr view <ref> --json` — PR description, metadata
- `gh api repos/<owner>/<repo>/pulls/<n>/reviews` — prior reviews
- `gh api repos/<owner>/<repo>/pulls/<n>/comments` — inline review threads
  (already threaded by GitHub)
- `gh api repos/<owner>/<repo>/issues/<n>/comments` — issue-thread comments

Issue-thread comments render as a chronological section beneath the PR
description on the description page. Inline review threads render as collapsible
blocks immediately beneath the diff line they anchor to: author handle,
timestamp, body, replies indented under parent. Default expanded; `T` toggles
globally to reduce visual noise during a fast walk.

Outdated threads (GitHub returns `position: null`, or `original_commit_id`
doesn't match any current PR commit) render with a faded color and "outdated"
label but stay visible — they're historical context the reviewer may want to
read.

Cursor JSON shape: `{commit_sha, file, line, side}`. Loaded on open; written on
quit. Storage layout per spec (_Comment Storage_):
`~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/cursor.json`.

Critical non-regression: jjr must still ship and work end-to-end. The TUI
parameterization must preserve jjr's composer, packet generation, and
claude-invocation paths. These are not the user-visible delivery of this phase —
they're already-shipping behavior that must survive the move. The acceptance
criteria include manual smoke tests of jjr's existing flows.

Workspace lints (per `CLAUDE.md`): strict — `unwrap_used`, `expect_used`,
`print_stdout`, `print_stderr`, `as_conversions` denied. Errors flow through
`Result<T, _Error>` via `snafu`. Tests are exempted from these denies (see
`clippy.toml`).

#### Delivers

- `local-review-core::tui` module exposing the review TUI parameterized by a
- jjr migrated to consume the core TUI; jjr behavior unchanged
- ggr migrated to consume the core TUI
- ggr fetches existing GitHub state via `gh api`: PR description, issue-thread
- Issue-thread comments render as a chronological section beneath the PR
- Inline review threads render as collapsible blocks beneath their anchor lines
- Outdated threads (GitHub `position: null` or `original_commit_id` not in
- `T` keybinding toggles thread expand/collapse globally
- Cursor persistence at

#### Done When

- `cargo build -p jjr -p ggr` succeeds
- `just validate` passes
- Manual: jjr non-regression — opening jjr on a stack walks the diff, composer
- Manual: `ggr <pr-ref>` on a real PR shows description body and issue-thread
- Manual: walking commits shows existing inline review threads beneath their
- Manual: `T` toggles all threads to collapsed, `T` again toggles back
- Manual: a thread anchored to a force-pushed-away commit renders with a faded
- Manual: quit and reopen ggr; resumes at the last viewed commit + file + line

#### Depends On

- (none)

## Phase 2: Local drafts at three scopes

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Build the comment data model in `local-review-core`, parameterized over the
identifier type (jjr's `ChangeId` vs ggr's `CommitId`) and over the scope shape
(jjr: line/change/stack with `Anchor::Description`; ggr: line/commit/pr without
description anchor).

Per the spec at `specs/github-pr-stack-review.md` (_Comment Model_ section), the
draft schema for ggr is `ggr-comment/v1` with `kind: "comment" | "reply"`. Phase
2 implements `kind: "comment"` only — replies come in P3.

Validity rules (enforced at write time, re-checked on load):

- `kind = "comment"` with `scope = "line"`: requires commit_sha, file
  (repo-root-relative POSIX), side, exactly one of {old_line, new_line} matching
  the side, hunk_header (verbatim `@@ ... @@`), target_text (≤1024 chars),
  context_before (≤3 lines), context_after (≤3 lines). Other anchor fields
  absent.
- `kind = "comment"` with `scope = "commit"`: requires only commit_sha.
  file/side/lines/hunk/target/context absent.
- `kind = "comment"` with `scope = "pr"`: no anchor fields. The pr_number on the
  record is the entire anchor.
- `severity` always present; composer defaults to `note` if reviewer doesn't
  pick.

Storage layout per spec (_Comment Storage_):

- `~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/drafts/<commit-sha>.jsonl` —
  line and commit drafts for that commit
- `~/.local/share/ggr/<host>/<owner>/<repo>/<pr>/drafts/_pr.jsonl` — PR-scoped
  drafts (leading underscore avoids collision with real commit SHAs, which never
  begin with underscore)
- Append for new drafts; atomic read-modify-write-then-rename for edit and
  delete (append-only is incompatible with edit). Atomic rename ensures a crash
  mid-write cannot corrupt prior drafts.

Composer modal: ggr reuses jjr's composer (now in `local-review-core` via P1).
The composer's three exit paths — save new, save edit, delete — write through
the storage layer above.

`ggr drafts <pr-ref>` reads all draft files for this PR and prints them in
human-readable form. `ggr clear <pr-ref>` truncates each draft file (preserves
directory structure for future cycles). The `--stale` flag is accepted but
currently a no-op; the stale filter ships in P4.

jjr non-regression: jjr's existing `.jj-review/comments/<change-id>.jsonl`
storage and composer must continue to work. The parameterization in core is the
seam that lets ggr have its own storage layout without changing jjr's behavior.

#### Delivers

- Comment data model in `local-review-core` parameterized over identifier
- ggr composer modal entry: `Enter`/`c` for line scope, dedicated key for commit
- Severity selector (note/suggestion/required), defaulting to note
- Validity rules per spec — line scope requires commit_sha + file + side +
- Edit and delete via the same composer (atomic read-modify-write-then-rename of
- Persistence at
- `ggr drafts <pr-ref>` lists local drafts in human-readable form
- `ggr clear <pr-ref>` truncates all draft files (preserves directory);

#### Done When

- `cargo build -p ggr` succeeds
- `just validate` passes
- Manual: reviewer adds a line draft on commit X, file Y, line Z; quits ggr;
- Manual: edit changes the body; delete removes the draft from disk
- Manual: drafts at all three scopes work; severity selector defaults to note
- Manual: invalid drafts (e.g., line scope with missing commit_sha) are rejected
- Manual: `ggr drafts <pr-ref>` shows correct output; `ggr clear <pr-ref>`
- Manual: jjr non-regression — jjr's draft model unchanged, still uses

#### Depends On

- shared-review-tui-ggr-shows-existing-github-state

## Phase 3: Reply composer and batched submit to GitHub

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Ship the reply composer and the GitHub submit endpoint in one phase. They're
combined because reply drafts that can't be submitted have no reviewer-visible
value, and submit needs the reply data model in place to dispatch correctly.

**Reply composer.** When the cursor is on an existing thread block (rendered in
P1), `r` opens the composer in reply mode. The reply draft has `kind: "reply"`,
carries `parent_comment_id`, and stores in `_replies.jsonl`.

The `parent_comment_id` is GitHub's _review comment ID_, not a thread ID.
GitHub's reply endpoint takes the parent comment's ID and replies in the same
thread. ggr stores the ID exactly as the API returned it on fetch in P1. Per
spec validity rules: reply drafts have `parent_comment_id` set; `scope` and all
anchor fields absent.

**Submit per spec** (`specs/github-pr-stack-review.md`, _GitHub API Mapping_):

1. Verdict modal: APPROVE / REQUEST_CHANGES / COMMENT, default COMMENT.
2. Empty-submit semantics: APPROVE/REQUEST_CHANGES always allowed (the verdict
   is the carry, even with zero drafts). COMMENT with zero drafts errors with
   "nothing to submit; pick approve or request-changes if you intended to weigh
   in without comments."
3. Review POST: `gh api repos/<owner>/<repo>/pulls/<n>/reviews -X POST` with
   payload `{event, body, comments}`.
   - `event` ← verdict.
   - `body` ← composed from PR-scoped drafts (rendered verbatim) plus
     commit-scoped drafts as quoted attribution blocks.
   - `comments[]` ← built from line-scoped drafts. Each entry:
     `{path, line, side, commit_id, body}`. The `commit_id` is set to the
     draft's `commit_sha` so the comment anchors to that specific commit, not
     the PR head.
4. Reply fan-out: for each reply draft,
   `gh api repos/<owner>/<repo>/pulls/<n>/comments -X POST` with
   `in_reply_to=<parent_comment_id>`. Serial. ggr posts replies after the review
   submission returns successfully.

**Severity markers** per spec (_Severity in ggr_): the marker is the first line
of the submitted comment body. Format matches jjr's packet rendering for
cross-tool consistency:

- `[REQUIRED]` — must be addressed
- `[SUGGESTION]` — should be addressed when safe
- `[NOTE]` — informational

**Commit-scoped attribution** per spec (_Why commit-scoped comments go in the
body_):

```
> Commit abc1234 — "implement retry policy"
>
> <comment body>
```

The leading `> Commit <sha-short> — "<subject>"` is the convention. GitHub has
no PR-review primitive for "this whole commit," so commit-scoped drafts go in
the review body with explicit attribution.

**Partial failure handling** per spec (_Submit ordering and partial failure_):

- Review POST happens first. If it fails, no replies are sent and all drafts
  remain on disk for retry.
- If review succeeds but a reply fails, the review and all preceding successful
  replies are on GitHub. The failing reply and any subsequent unsubmitted reply
  drafts remain on disk.
- ggr reports a structured summary: review status, per-reply status, which
  drafts to retry.
- The reviewer can re-submit; ggr skips drafts whose corresponding GitHub object
  already exists (detected on next fetch, since drafts are cleared on success).

On full success: all drafts (including replies) cleared via the P2
atomic-rewrite path.

jjr non-regression: jjr's `jjr claude` packet path is unchanged — the submit
endpoint is ggr-only code.

#### Delivers

- `r` keybinding (when cursor is on an existing thread block) opens the composer
- Reply drafts stored in
- Reply drafts render indented under the parent thread, visually distinct from
- `S` keybinding triggers submit modal with verdict choice (APPROVE /
- Empty-submit handling: APPROVE/REQUEST_CHANGES with zero drafts is allowed;
- Submit posts one `gh api repos/<owner>/<repo>/pulls/<n>/reviews -X POST`
- Severity markers `[REQUIRED]`, `[SUGGESTION]`, `[NOTE]` rendered as the first
- Commit-scoped drafts folded into the review body with
- Line-scoped drafts posted with `commit_id` set to the draft's commit_sha
- On full success, all drafts cleared via the atomic-rewrite path from P2
- On partial failure, drafts that did not reach GitHub remain on disk; ggr

#### Done When

- `cargo build -p ggr` succeeds
- `just validate` passes
- Manual: reviewer drafts at all three scopes plus a reply, submits with verdict
- Manual: severity markers visible as first line of submitted comment bodies
- Manual: commit-scoped drafts appear in review body with commit pointer;
- Manual: replies land in their target threads
- Manual: empty submit with verdict COMMENT errors with clear message; empty
- Manual: simulated partial failure (e.g., bad parent_comment_id) leaves correct
- Manual: jjr's claude-packet path still works (non-regression)

#### Depends On

- local-drafts-at-three-scopes

## Phase 4: Re-review across cycles with stale handling

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Final phase. Closes the cycle loop per the spec at
`specs/github-pr-stack-review.md` (_Re-Review Semantics_).

When the reviewer reopens ggr on a PR they've reviewed before:

1. ggr fetches the current PR state from GitHub: commits, threads, reviews,
   description (the same fetches as P1).
2. Local drafts are loaded.
3. For each line-scoped draft:
   - If `commit_sha` is still in the current commit list, run the line-anchoring
     algorithm against the current diff for `(commit_sha, file)`. The algorithm
     is `local-review-core::anchoring::match_anchor`, already implemented for
     jjr. Re-anchored: `status = "pending"`. Failed (target_text + context don't
     match anywhere in the hunk): `status = "stale"`, with `mismatch_reason` set
     per the algorithm's outcome.
   - If `commit_sha` is no longer in the commit list (force-push rewrote or
     dropped the commit), try commit-subject successor: find a commit in the new
     list whose subject equals the original commit's subject. If a unique match
     exists, re-anchor the draft to that commit's SHA and run line-anchoring on
     the new diff. If multiple or no matches, mark stale with
     `mismatch_reason = "commit not in PR"`.
4. For each commit-scoped draft:
   - If `commit_sha` in PR, status pending.
   - If not, attempt subject-based successor (same heuristic as line). On unique
     match, re-anchor. On no match, mark stale.
5. PR-scoped drafts: never stale (the pr_number is stable).
6. For each reply draft:
   - If `parent_comment_id` is still present in the freshly fetched comment
     list, status pending.
   - If the parent comment was deleted from GitHub (rare but possible — the
     author can delete their own comments), mark stale with
     `mismatch_reason = "parent comment deleted"`.
   - Outdated parents (parent comment is still on GitHub but its anchor is now
     outdated due to force-push) are NOT stale — the reply will still post
     correctly to the existing thread.

Stale drafts are surfaced separately, not inline. The reviewer can clear them,
edit them, or ignore them. Stale drafts are excluded from submission unless the
reviewer explicitly re-anchors or upgrades them (e.g., editing a stale draft
changes its anchor and re-runs validation).

`ggr clear <pr-ref> --stale` clears only drafts with `status = "stale"`. The
flag was wired in P2; this phase implements the actual filter.

`R` mid-session refresh: re-fetches everything (description, commits, threads,
reviews) and re-runs the re-anchoring pass. Replaces the in-memory snapshot. Per
the spec, ggr operates on the fetch-snapshot for the session and does not poll;
refresh is the explicit way to update.

Submit failures due to stale anchors (e.g., GitHub rejects a comment because the
`commit_id` is no longer in the PR head's history): ggr surfaces the failure and
prompts the reviewer to press `R`. Drafts whose anchors became stale are NOT
cleared; they remain for re-evaluation.

jjr non-regression: jjr's existing line-anchoring on reopen for jj changes is
unchanged. The shared algorithm in `local-review-core::anchoring` continues to
power both tools.

#### Delivers

- On reopen, fetch current PR state and re-anchor local drafts (re-uses the P1
- Line/commit-scoped drafts: if `commit_sha` is still in the PR's commit list,
- If `commit_sha` is no longer in the commit list, attempt commit-subject
- On no successor or non-unique match, mark stale with
- Reply drafts: if `parent_comment_id` no longer present in fetched comments,
- PR-scoped drafts never go stale
- Stale panel surfacing stale drafts with their `mismatch_reason`; reviewer can
- Stale drafts excluded from submission unless the reviewer explicitly
- `ggr clear <pr-ref> --stale` filter implemented (the flag was wired in P2)
- `R` keybinding for mid-session refresh: re-fetch state and re-run re-anchoring
- Submit failures due to stale anchors prompt the reviewer to refresh

#### Done When

- `cargo build -p ggr` succeeds
- `just validate` passes
- Manual: reviewer creates a line draft on commit X; PR is force-pushed (a new
- Manual: reviewer creates a line draft on commit Y; PR is force-pushed dropping
- Manual: stale panel shows stale drafts with reasons
- Manual: `ggr clear --stale` clears only the stale drafts, leaves pending
- Manual: `R` mid-session re-fetches state and updates the stale panel
- Manual: jjr's re-anchoring for jj changes still works (non-regression)

#### Depends On

- reply-composer-and-batched-submit-to-github

## Notes

### Out of scope (Later Enhancements)

Deferred per spec; do not pull into MVP phases:

- **Claude as review-orientation layer.** Pre-review pass where Claude surfaces
  context. Shared between jjr and ggr. Be careful: must not contradict jjr's
  principle that the codebase change is the reply.
- **Resolve conversation.** Mark threads resolved from ggr.
- **Edit own submitted comments.** Once on GitHub, ggr does not modify
  GitHub-side state beyond submit.
- **Multi-reviewer filtering.** All existing comments rendered as-is in MVP.
- **Posting non-review (issue-thread) comments.** Read-only in MVP.
- **Open-in-browser keybinding.** For tasks ggr doesn't support (merging, label
  management, etc.).
- **Persistent diff/thread cache; offline mode.** MVP fetches everything fresh
  on open.

### Bookkeeping

The stale `specs/ggr-mvp.ladder.md` will be removed in the same PR as this
ladder.
