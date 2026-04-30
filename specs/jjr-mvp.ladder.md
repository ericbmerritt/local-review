## Phase 1: Read-only diff view

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-04-29 | 2026-04-29 |

Establishes the project skeleton and the read-only diff-viewing surface. Layout: flat src/ with one-file-per-concern modules; tui/ as the only subdirectory. Mirror the lint and quality posture from ~/workspace/monorepo-stacks/stacks/rust-project-linter/tools/rust-project-linter — copy clippy.toml and the [lints.*] block from its Cargo.toml verbatim. Implications: unwrap_used and expect_used are denied (every fallible operation flows through Result; no .unwrap() shortcuts even in tests); print_stdout and print_stderr are denied (use writeln! against an explicit locked Stderr handle, matching the linter's run_check pattern); as_conversions is denied (TryFrom/From only); unsafe_code, dead_code, and unreachable_pub are also denied. Errors via snafu (not anyhow/thiserror). Diff source per specs/local-stack-review-edd.md §Diff Source: jj show <change> --git --color=never (the --color=never flag is non-negotiable). Edge cases the parser must handle: multifile diffs split per `diff --git`, zero-count hunks (`@@ -0,0 +5,12 @@` for pure additions, `@@ -3,5 +3,0 @@` for pure deletions), trailing function context after the second @@ (e.g., `@@ -10,5 +10,7 @@ impl Client {`), binary files (Binary files ... differ — UI shows 'Binary file not shown' and disables comments), renames/copies (track the new path), UTF-8 only (invalid bytes surface a clear error and exclude the file with a warning, not a crash). TUI is intentionally minimal here: Screen 1 chrome only, no comment affordances yet. flake.nix should use fenix or rust-overlay to pin the toolchain. The strict lint posture means error UX must be deliberate from day one — no .unwrap() escape hatches. NOT in scope: comments (Phase 2), stack walking (Phase 3), reanchoring (Phase 4), meta-comments or stack overview (Phase 5).

#### Delivers

- Cargo project with dependency set (clap v4 derive, serde, serde_json, snafu, ratatui, crossterm, time, toml, unidiff) and [lints.*] posture mirrored from rust-project-linter
- flake.nix pinning Rust toolchain and providing just, cargo-llvm-cov, cargo-nextest, cargo-deny, alejandra, statix in the dev shell, with .envrc for direnv
- Justfile with validate / lint / test / format / build targets
- clippy.toml, deny.toml, AGENTS.md
- src/error.rs (snafu), src/change_id.rs (ChangeId/CommitId newtypes)
- src/jj.rs subprocess wrapper for jj show --git --color=never
- src/diff.rs unified-diff parser using unidiff (or patch crate if unidiff drops function-context)
- src/tui.rs minimal app shell rendering Screen 1 chrome (stack bar, file header, footer), with arrow / j / k movement, Tab / Shift-Tab file cycling, ? help (Screen 7 read-only), q quit
- tests/cli.rs first integration test against a one-change fixture jj repo using assert_cmd, predicates, and tempfile

#### Done When

- cargo build produces a jjr binary
- nix develop drops the user into a Rust dev shell with all tools available
- just validate passes inside the dev shell
- jjr <change-id> opens a TUI showing the diff for that change
- Arrow keys and j/k move the line cursor; Tab and Shift-Tab cycle files
- ? opens the help screen; q exits cleanly
- The integration test in tests/cli.rs passes against the fixture repo

#### Depends On

- (none)

## Phase 2: Line comments

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-04-29 | 2026-04-29 |

First persisted state. Wire format matches schema_version 'diff-comment/v2' from specs/local-stack-review-edd.md §Comment Model; only Anchor::Line is exercised in this phase. Anchor is an enum with associated data — Line { change_id, anchor }, Change { change_id }, Stack — making invalid states unrepresentable (parse, don't validate). Compiler enforces what the spec promises. Composer is Screen 2 from specs/jjr-tui-design.md: centered modal over dimmed main view, double-lined borders to distinguish from the modeless main view. Chrome: target-line context (three lines around the diff line, target marked with ▶), scope picker (only ^L is reachable in this phase but the binding stands), severity picker, body editor. Use tui-textarea for the body editor (multi-line, word wrap). Radio glyphs are [x] / [ ] (plain ASCII, monospace-bombproof). Composer keys: ^L scope=line; ^1 / ^2 / ^3 severity (note / suggestion / required); ^X save (NOT ^S — POSIX terminals eat ^S for XOFF flow control); Esc cancel. ^C inside the composer is captured as 'scope=change' rather than SIGINT — Esc is the cancel path. This is a deliberate departure from shell convention; document it in the help screen. Default severity for the first comment of a session is 'suggestion'; subsequent default to last-picked. Saved comments render inline immediately below their target line, indented with a `┃` column marker, per spec principle 5. Set target_text, hunk_header, context_before, context_after on save so Phase 4's reanchoring algorithm has the data it needs (even though Phase 2 doesn't yet exercise full reanchoring). Storage path: .jj-review/comments/<change-id>.jsonl. Divergent change IDs: slash → underscore in filename only (abc/1 → abc_1.jsonl); canonical change_id preserved in JSON. Reanchoring in this phase is trivial: same diff, exact target_text + context match. The full algorithm lives in Phase 4. CRITICAL: comment-save NEVER triggers Claude. Explicit principle: review and remediation remain separate. NOT in scope: stack walking (Phase 3), reanchoring across edits (Phase 4), change/stack scope (Phase 5).

#### Delivers

- src/comment.rs with Comment, Anchor (Line variant only in this phase), Severity, Status enums and serde
- src/store.rs JSONL read/write at .jj-review/comments/<change-id>.jsonl
- Composer modal (Screen 2) with scope picker, severity radios, tui-textarea body editor
- Inline comment rendering in the diff view
- Edit/delete affordances on a focused comment
- .jj-review/ idempotently added to .gitignore and .jjignore on first run

#### Done When

- Pressing c or Enter on a diff line opens the composer with target line marked ▶, scope=line preselected
- ^X saves; Esc cancels
- Saved comment appears inline below the target line
- Comment persists at .jj-review/comments/<change-id>.jsonl as JSONL
- Restarting jjr <change-id> shows the saved comment in place
- Cursor on a saved comment + e reopens composer prepopulated; d deletes
- .gitignore and .jjignore each contain a `.jj-review/` line, added idempotently on first run
- Divergent change IDs (e.g., abc/1) persist to abc_1.jsonl with canonical change_id preserved in the JSON

#### Depends On

- read-only-diff-view

## Phase 3: Stack mode and cursor resume

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-04-29 | 2026-04-29 |

Walking the stack. Default revset for --stack (and bare jjr): trunk()..@. trunk() is jj's revset alias for the trunk branch, configurable via revset-aliases.'trunk()' in the user's jj config. If trunk() is unresolvable (alias not configured) or the revset evaluates to empty, fall back to @ and emit a warning to stderr. The exact revset is documented and not inferred dynamically. Bare jjr defaults to stack mode using this revset; jjr <change-id> reviews a single change. Cursor state is stored at .jj-review/cursor.json keyed by a hash of the resolved revset (BLAKE3 of canonicalized revset string: lowercased, whitespace-normalized). The JSON shape per spec: {"revsets": {"<hash>": {"revset": "<original>", "last_change_id": "<id>", "updated_at": "<RFC3339>"}}}. Resume rule per specs/local-stack-review-edd.md §Resumability: if cursor's last_change_id is still in the resolved stack, open at the next unreviewed change after it (or at last_change_id itself if it has no comments yet); if no longer in the stack, open at the oldest. The cursor advances when the reviewer moves forward (n); it does NOT advance on p. Quitting persists at last-viewed change. Screen 3 (transition) default is 'never' per specs/jjr-tui-design.md (changed from 'auto' in iteration after Saskia review); reviewers opt in via .jj-review/config.toml [ui] transition_screen. Divergent changes: jj disambiguates via /<index> (abc/1, abc/2); each is a distinct stack entry keyed by canonical change_id. NOT in scope: reanchoring (Phase 4), stack overview screen (Phase 5).

#### Delivers

- src/stack.rs with ResolvedStack, StackEntry, and revset_hash via BLAKE3
- src/cursor.rs reading and writing .jj-review/cursor.json
- Revset resolution via jj log: trunk()..@ for --stack and bare jjr, arbitrary revsets for jjr <revset>, falling back to @ with a stderr warning if trunk() is unresolvable or empty
- n / p navigation between changes
- Stack progress bar in Screen 1 chrome reflecting N/M position
- Screen 3 (transition) with default config 'never'
- --restart flag for jjr --stack

#### Done When

- jjr --stack (and bare jjr) resolves trunk()..@ and walks oldest-to-newest
- jjr <revset> reviews changes returned by an arbitrary revset
- n advances stack position; p retreats; cursor advances on n only
- Quitting persists cursor at .jj-review/cursor.json keyed by revset_hash
- Re-invocation of jjr --stack resumes at the next unreviewed change after last_change_id (or at last_change_id itself if it has no comments)
- If last_change_id is no longer in the resolved stack, jjr opens at the oldest change
- jjr --stack --restart clears the cursor and opens at oldest
- Divergent changes (abc/1, abc/2) are distinct stack entries keyed by canonical change_id

#### Depends On

- line-comments

## Phase 4: Reanchoring and stale view

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-04-30 | 2026-04-30 |

The load-bearing algorithm for re-review. Implementation in src/anchoring.rs as a pure function: given a Comment and the current Diff, returns AnchorOutcome::ReAnchored {...} or AnchorOutcome::Stale { reason }. Algorithm per specs/local-stack-review-edd.md §Line-anchoring algorithm: (1) Locate the hunk by function-context (the segment after the second @@ in hunk_header); if no function-context recorded, consider all hunks in the file. Line-number ranges in the hunk header are NOT used for matching — they shift on every edit. (2) Exact match within the hunk on target_text + context_before + context_after (within the available window — fewer than 3 lines is acceptable at hunk boundaries). On unique match: re-anchor, status=pending. On multiple exact matches (repeated identical lines), prefer the match closest to the original display_line_number if recorded; otherwise mark stale. (3) Fuzzy match within the hunk: search for context_before + context_after with any line between them, OR target_text with matching context on one side only. On unique fuzzy match: mark stale with mismatch_reason populated ('target_text changed' / 'context_before changed' / 'context_after changed'). (4) No match in file: mark stale with mismatch_reason='anchor not found'. (5) File no longer in diff: mark stale with mismatch_reason='file not in diff'. (6) Change no longer in resolved stack: status=orphaned (skip anchoring entirely). Stale comments do NOT appear inline; pressing S from any view opens Screen 5 from specs/jjr-tui-design.md. Each stale entry shows was/now lines (concrete delta of why it's stale — the most important affordance in the screen) and the mismatch reason at the right edge. R reanchor manually is reserved as a future affordance and is NOT shown in the Screen 5 footer (per iteration after Saskia review — grayed-out keys are noise). MVP only has e edit & re-anchor (which opens the composer at a user-selected line). Stale comments persist on disk; not auto-deleted. NOT in scope: meta-comments (Phase 5), packet exclusion of stale (Phase 6).

#### Delivers

- src/anchoring.rs with the line-anchoring algorithm as a pure function
- Stale status on the Comment model with mismatch_reason
- Screen 5 (stale comments) with was/now display, Enter to view in source, e to edit and re-anchor, d to delete
- jjr clear --stale CLI command

#### Done When

- Comments re-anchor cleanly on reopen when target_text + context still match
- Comments that fail to re-anchor are marked status=stale with a populated mismatch_reason
- Stale comments do NOT appear inline in the diff
- S from any view opens the stale view
- Each stale entry shows was/now lines and the mismatch reason
- e from the stale view opens the composer to manually re-anchor at a user-selected line
- d deletes the focused stale comment
- jjr clear <revset> --stale clears all stale comments for a revset

#### Depends On

- stack-mode-and-cursor-resume

## Phase 5: Meta-comments and stack overview

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-04-30 | 2026-04-30 |

Adds Anchor::Change and Anchor::Stack variants. Change-scoped comments persist in <change-id>.jsonl alongside line-scoped (the scope discriminator distinguishes; serde tag 'scope' on the wire). Stack-scoped comments persist in .jj-review/comments/_stack.jsonl, with multiple stacks coexisting via in-record revset_hash filtering. The leading underscore in _stack avoids collision with real change IDs (jj change IDs do not begin with underscores). Composer's scope picker fully active: ^L line (default when cursor on a diff line), ^C change (default when cursor on a change row in stack overview), ^K stack (default on the stack-level header). Picker swaps the chrome context block per specs/jjr-tui-design.md Screen 2: scope=line shows three lines of diff context with ▶ marker; scope=change shows change ID and description; scope=stack shows the revset. Screen 4 (stack overview) accessed via s from main view. Renders stack-level comments at top with severity dot + one-line body preview, then ─── separator, then change rows. Change-level comments inset under their change row prefixed with ◆; right-edge dot count aggregates across all scopes; severity dots denote hot spots regardless of scope. Done indicator ✓ for changes with no comments at any scope (NOT 'approved' — jjr does not model approval per spec principle 'tool does not model done'). Column budget at 80 cols: idx(2) sp(2) cid(8) sp(2) desc(filled) sp(2) dots(3) sp(2) count(2). Truncate descriptions with `…` at the column where the dot column begins. Strip `│` and other box-drawing characters and ANSI escapes from preview rows. First-body-line only — multi-line bodies get the first line, truncated. Resize ladder for Screen 4: 120+ as specified; 100-119 description truncates and previews truncate; 80-99 drop the idx column (cursor ▶ already conveys position); <80 drop change-level inset preview text entirely, keep just ◆ change · severity prefix (body is one keystroke away on Enter). Orphan detection: when a change is no longer in the resolved revset (jj abandon / jj undo / jj rebase), its comment files persist on disk but are loaded with status=orphaned and not surfaced in the UI. Stack-scoped comments are NEVER stale (no anchoring to content) — they reappear in every cycle until cleared. From Screen 4, c opens the composer with default scope from cursor row. NOT in scope: orphaned-comment surfacing UI (deferred to Later Enhancements: jjr orphans), packet generation (Phase 6).

#### Delivers

- Anchor::Change and Anchor::Stack variants persisted and loaded
- .jj-review/comments/_stack.jsonl storage for stack-scoped comments keyed by revset_hash
- Composer scope picker fully active: ^L line, ^C change, ^K stack
- Screen 4 (stack overview) accessed via s from main view
- Stack overview renders stack-level comments at top, change-level inset under their change row with ◆ prefix
- Orphan detection: comment files for change_ids not in the resolved revset are loaded with status=orphaned and not surfaced in the UI
- 80-col column budget and resize ladder for Screen 4 per spec

#### Done When

- In the composer, ^C flips scope to change and ^K flips to stack; the chrome context block swaps to match (line shows diff context, change shows ID and description, stack shows the revset)
- Default scope follows the cursor: line on a diff line, change on a stack-overview change row, stack on the stack-overview header
- Change-scoped comments persist alongside line-scoped in <change-id>.jsonl with the scope discriminator
- Stack-scoped comments persist in _stack.jsonl filtered by revset_hash on load
- Stack overview shows stack-level comments at top with one-line preview, then ─── separator, then change rows with change-level inset under them prefixed by ◆
- Right-edge dot count on each change row aggregates across all scopes
- Done indicator ✓ for changes with NO comments at any scope
- When a change is no longer in the resolved revset, its comment files persist on disk but its comments load as status=orphaned and don't surface in the UI
- At 80 cols: idx + change_id (8 chars) + description (truncated with …) + dots (3) + count (2) all fit; preview rows strip │ and ANSI escapes; first body line only
- Resize ladder: <100 cols truncate descriptions; <80 cols drop idx column; <80 cols drop preview body keep ◆ change · severity prefix

#### Depends On

- reanchoring-and-stale-view

## Phase 6: Packet generation

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| 🟡 in-progress  | 2026-04-30 |            |

Produces the inspectable artifact before any Claude shell-out. Render is pure (packet/render_prompt); deterministic from packet input; downstream tooling can rely on the format. Template per specs/local-stack-review-edd.md §Claude Prompt Format. Order: header (Repository, Revision) → '## Stack-Level Review Comments' (if any) → '## Changes' → for each change in stack order: 'Change ID / Commit / Description' → '### Change-Level Review Comments' (if any) → '### Line-Level Review Comments' (if any) → '### Relevant Diff Context' (full hunk with 3 lines context, only if line comments present). Sections with no content are OMITTED entirely (no empty headers). Comment block rendering per spec §Comment block rendering: line-scoped uses target_text + context_before/after with `>>>` marker on the target line; change-scoped uses '### [<SEVERITY>] (change-level) <change_id>'; stack-scoped uses '### [<SEVERITY>] (stack-level)'. Inclusion rules: stale comments excluded by default (--include-stale opt-in); orphaned comments ALWAYS excluded (--include-stale does NOT include them); stack-scoped comments ALWAYS included even in per-change packets — they give Claude the cross-cutting context. Empty packet (no pending comments at any scope across the target revset) errors with 'no comments to send' and exits non-zero. The prompt's rules section follows the v2 contract: address by editing the code (not by writing summaries or declines), required must be addressed, suggestions are addressed when safe and consistent with the change's existing design, notes are informational, preserve original intent, smallest safe edits, do not broaden scope, do not rewrite unrelated code, edit changes in place using jj's mutability model. NO decline-with-reasoning clause. NO summary-of-which-were-addressed clause. Match wording in spec exactly to keep the format stable for downstream tooling. Output is byte-deterministic from the same packet input. NOT in scope: actually invoking Claude (Phase 7), Screen 6 send modal (Phase 7).

#### Delivers

- src/packet.rs with build_packet (assembles packet from comments + diff) and render_prompt (deterministic Claude prompt)
- jjr packet [revset] CLI command writing to stdout (default) or -o <path>
- Inclusion rules: stale excluded by default with --include-stale opt-in; orphaned ALWAYS excluded; stack-scoped ALWAYS included even in per-change packets
- Empty-packet error path

#### Done When

- jjr packet @ writes the prompt to stdout matching the spec template byte-exact
- jjr packet @ -o /tmp/p.txt writes to file
- jjr packet <revset> works against arbitrary revsets
- jjr packet @ --include-stale includes stale comments
- Empty packet (no pending comments at any scope across the target revset) errors with 'no comments to send' and exits non-zero
- Output order: Repository / Revision header → Stack-Level Review Comments (if any) → Changes → for each change in stack order: Change ID/Commit/Description → Change-Level Review Comments (if any) → Line-Level Review Comments (if any) → Relevant Diff Context (if any line comments)
- Sections with no content are omitted entirely (no empty headers)
- Comment block rendering matches spec: line-scoped uses target_text + context with `>>>` marker; change-scoped uses '### [<SEVERITY>] (change-level) <change_id>'; stack-scoped uses '### [<SEVERITY>] (stack-level)'
- Same input produces byte-identical output (deterministic)

#### Depends On

- meta-comments-and-stack-overview

## Phase 7: Claude invocation and review cycle

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Closes the review cycle. C from main view opens Screen 6 (send-to-Claude confirmation). Screen renders comment counts as a fixed scope×severity grid (one row per non-zero pair, no continuation rows under blank scope labels — the asymmetry was a Saskia finding, fixed). Files-affected uses the same grid grammar (file path + count). Stale count shown terse 'excluded' (no parenthetical teaching the CLI flag — the CLI flag is documented in --help and isn't actionable from this modal). v shows the full rendered prompt (same as jjr packet | less). Prompt is generated, not authored — to change what Claude sees, the reviewer cancels (Esc), edits comments, re-opens. CLI: jjr claude @ and jjr claude <revset> work without entering the TUI. MVP is single-change handoff per specs/local-stack-review-edd.md §MVP Scope #9. Stack-wide handoff (jjr claude --stack, A keybind) is deferred. Invocation sequence: (1) capture prior @ via jj log -r @ -T 'change_id' or equivalent; (2) if @ != target_change, run jj edit <target_change>; (3) write packet to tempfile::NamedTempFile; (4) run claude -p with packet redirected to stdin via Command::stdin(File::open(tempfile_path)); stdout/stderr passthrough to user terminal so they see Claude's progress; (5) capture exit code; (6) restore prior @ via jj edit <prior_change> regardless of exit code; (7) on zero exit, re-run jj show <target_change> and redraw Screen 1 against the new diff (start of next review cycle); (8) on non-zero exit, report error to stderr, leave working copy at target (do NOT roll back Claude's partial edits — explicit per spec §Invocation behavior), prompt user before resuming. Implementation pattern: WorkingCopyGuard RAII type that captures prior @ on construction and runs jj edit <prior_change> in Drop; restore on panic too. claude is invoked via std::process::Command (no shell, no injection vector). NamedTempFile auto-deletes on Drop. Open question from spec acknowledged: when Claude has questions or genuinely cannot address a comment, MVP has no back-and-forth channel — the next review cycle's diff (or its absence) is the implicit signal. Future direction sketched in spec §Open Questions but explicitly deferred. NOT in scope: stack-wide handoff, per-comment remediation, inter-cycle diff, conversational back-and-forth.

#### Delivers

- src/claude.rs subprocess wrapper for claude -p
- WorkingCopyGuard RAII type for jj edit + restore
- Screen 6 (send-to-Claude) with scope×severity grid, files-affected grid, stale 'excluded'
- C key from main view opening Screen 6; Enter sends; v shows full prompt; Esc cancels
- Post-claude redraw returning to Screen 1 against the new diff (next review cycle)
- Error reporting on non-zero exit with clear next-step guidance
- CLI: jjr claude @ and jjr claude <revset> (single-change handoff in MVP)

#### Done When

- C from the main view opens Screen 6 with comment counts in scope×severity grid (one row per non-zero pair, no continuation rows under blank labels), files-affected grid, stale count 'excluded'
- v shows the full rendered prompt (same content as jjr packet | less)
- Enter sends: jjr captures prior @, runs jj edit <change>, runs claude -p with packet on stdin, captures exit code
- On zero exit, jjr restores prior @ via jj edit, re-runs jj show <change>, redraws Screen 1 against the new diff
- On non-zero exit, jjr reports the error to stderr, leaves working copy at the target change (does not roll back Claude's edits), prompts the user before resuming
- WorkingCopyGuard restores prior @ even on panic
- jjr claude @ and jjr claude <revset> work from CLI without entering the TUI
- Stale comments excluded by default; --include-stale includes them

#### Depends On

- packet-generation

## Phase 8: Polish and packaging

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Final polish to fill out the EDD's full MVP scope. CLI commands: jjr export <revset> writes JSONL to stdout by default (the on-disk format) or markdown with --format markdown. Markdown format is human-readable sections per change for sharing comments out-of-band (still local; nothing networked). jjr clear <revset> clears all comments at all scopes for the revset (confirmation prompt unless --yes); --stale clears only stale; --orphaned clears only orphaned (this is the MVP path for cleaning up after abandoned changes — the full jjr orphans view is a Later Enhancement). TUI affordances: f opens a file picker modal listing files in the current change; selecting jumps. 1 / 2 / 3 filter the diff view to comments of the chosen severity (other lines visible, comment markers hidden). r re-runs jj show + reloads comments without exiting. Resize behavior verified across all screens at 80 cols per specs/jjr-tui-design.md §Resize behavior; main view footer drops bindings right-to-left in this priority order: ?, then C → Claude, then S stale, then s stack — preserve the irreducible four (Enter comment, Tab file, n/p revision, ↑↓ line). Stack overview's resize ladder was implemented in Phase 5; verify here. Below 60 cols the tool refuses to render and prints a message. Error UX: each error condition gets a specific message, not a panic. The strict lint posture (unwrap_used = deny) already enforces no panics-from-Result, but error UX deserves attention beyond just having a Result. Cases: jj missing on PATH → clear error message with install hint; claude missing on PATH for jjr claude → clear error; jjr packet still works (no claude dependency); diff parse failure on one file → that file is excluded from review with a logged warning, others remain reviewable per spec §Diff Source; UTF-8 invalid bytes → file excluded with warning; schema-version mismatch on a comment file → clear error suggesting jjr clear. Cargo.toml metadata for cargo install jjr: name 'jjr', description from README, repository URL placeholder, license TBD per README. AGENTS.md final: short, project-specific, points at specs/. README polish if needed. NOT in scope (deferred to post-MVP per specs/local-stack-review-edd.md §Later Enhancements): stack-wide Claude (jjr claude --stack), inter-cycle diff feature, jjr orphans full view, all spec rendering polish (difftastic, delta, syntax highlighting, side-by-side), GitHub integration, per-comment remediation (Ctrl-A), CI output in Claude prompt, multiple reviewers.

#### Delivers

- jjr export <revset> with default JSONL and --format markdown
- jjr clear <revset> (with confirmation prompt unless --yes), --stale, --orphaned variants
- File picker (f from main view) listing files in current change
- Severity filter (1 / 2 / 3 in main view)
- Refresh (r from main view, re-runs jj show + reloads comments)
- Resize behavior verified at 80 cols across all screens; main view footer drop-rules implemented
- Error UX paths with specific messages: jj missing, claude missing, parse failures, schema-version mismatches, UTF-8 invalid bytes
- Cargo.toml metadata for cargo install jjr
- AGENTS.md final, README polish if needed

#### Done When

- jjr export <revset> writes JSONL to stdout by default; --format markdown writes markdown
- jjr clear <revset> clears all comments at all scopes (with confirmation unless --yes); --stale clears only stale; --orphaned clears only orphaned
- f opens a file picker modal listing files in the current change; selecting jumps to that file in the diff
- 1 / 2 / 3 filter the diff view to comments of that severity
- r refreshes the diff and comment data without exiting
- All screens render correctly at 80 cols per specs/jjr-tui-design.md §Resize behavior
- Main view footer drops bindings right-to-left at narrow widths, preserving Enter comment / Tab file / n/p revision / ↑↓ line
- jj missing on PATH → clear error message with install hint; claude missing → clear error for jjr claude; jjr packet still works
- Diff parse failure on one file → that file excluded with a logged warning, others remain reviewable
- Schema-version mismatch on a comment file → clear error suggesting jjr clear
- cargo install --path . produces a working binary
- All just validate gates pass; coverage ≥ 90% for functional core (anchoring, packet, diff, stack)

#### Depends On

- claude-invocation-and-review-cycle
