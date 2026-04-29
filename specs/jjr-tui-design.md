# jjr — TUI Design

Companion to the Local Stack Review EDD. The EDD specifies what the tool does. This document specifies what the reviewer sees and how they move through it.

## Design philosophy

Every TUI worth using makes a strong claim about what the user is doing. Lazygit claims you are a git mechanic standing in front of a workbench with all the tools laid out. Magit claims your repository is a single composable document you fold and unfold. Delta claims a diff is a thing you read, not a thing you scroll past.

`jjr` claims one thing: **you are reading a diff and leaving notes on it.** Everything else — stack navigation, Claude handoff, stale comment management — is in service of that loop.

This drives the layout. There is no four-panel dashboard. There is no command palette. There is no tree of branches and commits. There is a diff, large and centered, with the smallest possible chrome above and below it. Other views exist, but they are **modes** the user enters and exits, not panels competing for attention.

Six principles, in priority order:

1. **The diff is the page.** It dominates. Stack context is one row. File context is one row. Action keys are one row. Everything else is the diff.
2. **One focus at a time.** No tab cycling between panels in the main view. The diff has the focus, always. Other views are reached by entering a mode and exited by `Esc` or `q`.
3. **Verbs are letters, and a verb is one action.** `c` opens the comment composer. `d` deletes. `s` stack. `S` stale. `C` Claude. Scope (line / change / stack) is an attribute of the comment, not its own verb — the composer's scope picker handles that. Modifier keys (Ctrl, Shift) are for editor-style actions inside modals only — saving, cancelling, severity selection, scope toggle.
4. **The footer tells you what's possible right now.** It changes as the mode changes. It is not decoration; it is the user's discoverability surface alongside `?`.
5. **Comments are inline where they can be, in chrome where they can't.** Line-scoped comments appear immediately below the line they target, indented and bordered. Change-scoped comments appear in the change's section header. Stack-scoped comments live in the stack overview. A reviewer never hunts for them.
6. **Severity is color, not text.** Three severity dots — red, yellow, gray — read at a glance. Text labels (`REQUIRED`, `SUGGESTION`, `NOTE`) appear inside expanded comment bodies but never as a column or a prefix the eye has to parse.

### Comment scopes

A comment can attach at three levels. The composer is one screen with a scope picker; the default scope follows the cursor.

- **Line-scoped.** Anchored to a specific line in a specific file. The default when the cursor is on a diff line. Use for "this `.unwrap()` will panic," "rename this variable."
- **Change-scoped.** Anchored to a whole change. The default when the cursor is on a change row in the stack overview. Use for "this commit does too much, split it" or "the description doesn't match the code."
- **Stack-scoped.** Anchored to the resolved revset. The default when the cursor is on the stack-level header in the stack overview. Use for "rename `retry_wrapper` throughout" or "don't introduce new public APIs in this stack."

`c` (or `Enter`) opens the composer. The picker defaults intelligently from context. Inside the composer, `^L` / `^C` / `^K` flip scope explicitly. Severity semantics are uniform across all three scopes.

## Screen 1 — Main review view

This is `jjr`'s home. The diff viewer with stack context.

```
┌─ Stack ─────────────────────────────────────────────────────────────────────────┐
│ ████░░░░░░░░░░░░░░░  3/18  abc333  Add retry policy to client requests          │
└─────────────────────────────────────────────────────────────────────────────────┘
┌─ src/client.rs ──────────────────────── 2 of 2 files · 3 comments ──────────────┐
│       │ @@ -138,8 +138,14 @@ impl Client {                                      │
│   138 │     pub async fn send(&self, req: Request) -> Result<Response> {        │
│   139 │         let req = self.prepare(req)?;                                   │
│   140 │ -       let resp = self.inner.request(req).await?;                      │
│   141 │ +       let resp = self.retry_wrapper                                   │
│   142 │ +           .execute(|| self.inner.request(req.clone()))                │
│   143 │ +           .await?;                                                    │
│ ▌● 142│ ┃ required · 2 min ago                                                  │
│       │ ┃ This bypasses the retry policy. Use self.retry_wrapper.execute        │
│       │ ┃ instead — match the pattern used in fetch() above.                    │
│   144 │         Ok(resp)                                                        │
│   145 │     }                                                                   │
│       │                                                                         │
│   146 │     pub async fn fetch(&self, id: Id) -> Result<Item> {                 │
│ ▌● 146│ ┃ note · just now                                                       │
│       │ ┃ Reference implementation for the comment above.                       │
│   147 │         self.retry_wrapper                                              │
│   148 │             .execute(|| self.inner.fetch(id))                           │
│   149 │             .await                                                      │
│       │                                                                         │
│       │ @@ -201,3 +207,8 @@ impl Client {                                       │
│   207 │     fn prepare(&self, req: Request) -> Result<Request> {                │
│   208 │         req.with_auth(&self.token)                                      │
│ ▌● 208│ ┃ suggestion · 1 min ago                                                │
│       │ ┃ Consider validating the token isn't expired before signing.           │
│   209 │     }                                                                   │
└─────────────────────────────────────────────────────────────────────────────────┘
 ↑↓ line  Tab file  n/p revision  Enter comment  s stack  S stale  C → Claude  ?
```

### What's doing the work

**The stack bar (top).** A progress bar showing position in the stack. At any depth, the reviewer reads "I'm 3 of 18, ~17% in" at a glance. Position, change ID, description on the right. One row.

The stack is reviewed sequentially, oldest to newest. The bar reinforces that — there's no random-access affordance in the main view. Reviewers walk the stack with `n` and `p`. To revisit a specific change later, they invoke `jjr <change-id>` from the command line.

**The file header.** Borders carry the file path on the left and the position-in-files / comment-count on the right. Magit's section headers do this; it's denser than a status line and the eye finds the right edge naturally.

**The gutter.** Two columns: an indicator column (`▌●` for a comment marker, blank otherwise) and a line number column. Diff sign (`+`/`-`) is the first character of the line content, not the gutter, because we want the gutter to be a stable narrow band and the diff sign to read as part of the content (delta does this).

**Inline comments.** When a comment exists on a line, it appears immediately below that line, prefixed with `┃` to mark the comment column visually. The first line of the comment carries metadata (severity, age). The body wraps. No "click to expand" — comments are short by nature; if they're long, the reviewer scrolls. Tab on a focused comment toggles fold.

**Hunk separators.** Blank gutter rows between hunks, with the `@@` header indented in the diff column. No bold, no boxes — `@@` is enough.

**The footer.** Context-sensitive. The bindings shown are exactly the ones that work *right now*. When the cursor is on a comment, the footer changes:

```
 ↑↓ line  Enter edit  d delete  Tab fold  e expand thread  Esc deselect  ? help
```

This is the lazygit pattern: the footer is your peripheral-vision discoverability layer.

### What I rejected

- **A side panel showing the file tree.** The reviewer is reading one file at a time. A persistent file tree is dead pixels. Tab and Shift-Tab cycle files; if the reviewer wants the list, `f` opens a modal file picker.
- **A side panel showing comment list.** Comments are inline. A separate list is for stale comments only.
- **A side panel showing the stack.** Even at depth, the workflow is sequential — `n` advances, `p` retreats. The reviewer doesn't browse the stack list; they walk through it. A persistent stack pane is dead pixels in service of an interaction the reviewer doesn't perform.
- **`/` filter and `g` jump-by-index.** Random-access affordances for a sequential workflow. When a reviewer needs to revisit a specific change, they exit and run `jjr <change-id>`. That's rare enough that command-line invocation is the right surface.
- **Tabs across the top for changes.** The progress bar already shows where you are. Tabs would be redundant and unworkable at depth.
- **A persistent help line.** `?` is one keystroke. The footer is enough at all times.

## Screen 2 — Comment composer (modal overlay)

When the reviewer presses `Enter` or `c`. Renders as a centered modal over a dimmed main view. One composer handles all three scopes; the picker defaults from cursor context.

```
                                                                                  
┌─ Stack ─────────────────────────────────────────────────────────────────────────┐
│ ████░░░░░░░░░░░░░░░  3/18  abc333  Add retry policy to client requests          │
└─────────────────────────────────────────────────────────────────────────────────┘
                                                                                  
       ╔═══════════════ Comment · src/client.rs:142 ════════════════════════╗    
       ║                                                                     ║    
       ║   140 │ -   let resp = self.inner.request(req).await?;             ║    
       ║   141 │ +   let resp = self.retry_wrapper                          ║    
       ║ ▶ 142 │ +       .execute(|| self.inner.request(req.clone()))       ║    
       ║   143 │ +       .await?;                                            ║    
       ║                                                                     ║    
       ║   scope     [x] line    [ ] change · abc333    [ ] stack           ║    
       ║                                                                     ║    
       ║   severity  [ ] note    [ ] suggestion    [x] required             ║    
       ║                                                                     ║    
       ║   ┌─────────────────────────────────────────────────────────────┐ ║    
       ║   │ This bypasses the retry policy. Use self.retry_wrapper      │ ║    
       ║   │ .execute instead — match the pattern used in fetch() above.█│ ║    
       ║   │                                                              │ ║    
       ║   │                                                              │ ║    
       ║   └─────────────────────────────────────────────────────────────┘ ║    
       ║                                                                     ║    
       ╠═════════════════════════════════════════════════════════════════════╣    
       ║  ^L line  ^C change  ^K stack    ^1 note  ^2 suggestion  ^3 required║    
       ║                                                       ^X save  Esc  ║    
       ╚═════════════════════════════════════════════════════════════════════╝    
                                                                                  
```

### What's doing the work

**One composer, three scopes.** The picker is always present. The reviewer picks scope and severity, types the body, saves. There is no separate "meta-comment" verb to remember; scope is just an attribute of the comment.

**The default scope follows the cursor.** Press `c` on a diff line: scope defaults to *line* and the line context shows three lines around the target with `▶` marking it. Press `c` on a change row in the stack overview: scope defaults to *change* and the context block shows the change ID and description. Press `c` on the stack-level header in the stack overview: scope defaults to *stack* and the context shows the revset. Flipping scope inside the composer swaps the context block to match.

**Severity is a radio in the chrome, not in the body.** Picked with `^1` / `^2` / `^3`. The active selection is `[x]`, others `[ ]`. Default on first comment of a session is `suggestion`; subsequent comments default to whatever the reviewer last picked.

**Scope is also a radio.** Picked with `^L` / `^C` / `^K` — single keystroke per option, no toggling. The chord family avoids `^S` (POSIX flow control) and `^Tab` (eaten by some terminals); see Keybind notes below.

**The body editor is a real text area.** Multi-line. Word wrap. `^X` saves (deliberately not `^S`). `Esc` cancels. No vim modes inside the editor — that's a fight nobody wants.

**Radio glyphs are `[x]` / `[ ]`.** Plain ASCII, monospace-bombproof, survives terminal fonts that fall back outside the box-drawing block. Color (none in the radios themselves) is not load-bearing here.

**Footer keys use `^` for Ctrl** (compact, terminal-native convention).

### Keybind notes

- **`^X` for save**, not `^S`. POSIX terminals eat `^S` for XOFF flow control unless `stty -ixon` is set. `^X` ("eXit + write," Emacs `^X^S` cousin) doesn't carry that hazard.
- **`^L`/`^C`/`^K` for scope**, not `^Tab`. `^Tab` is remapped or eaten by tmux configs, kitty without explicit mapping, and some Windows terminals. Single-keystroke radios match the severity grammar (`^1`/`^2`/`^3`), so the reviewer learns one chord pattern.
- **No collision risk.** `^L` (terminal "redraw screen") and `^K` ("kill to end of line") are readline conventions; in a TUI modal that captures keys, neither default fires. `^C` inside a modal is captured as the scope key, not as SIGINT — `Esc` is the cancel/exit path. This is a deliberate departure from shell convention and worth calling out in the help screen.

### Editing existing comments

Same modal, prepopulated. Title reflects the scope:

- Line: `Edit comment · src/client.rs:142`
- Change: `Edit comment · change abc333`
- Stack: `Edit comment · stack`

Adds `^D delete` to the footer.

## Screen 3 — Change transition (deep-stack handoff)

When the reviewer presses `n` to advance, the tool can show a brief transition screen between changes. Default-on for stacks of depth ≥ 8, default-off below.

```
                                                                                  
                       ╔══════════════════════════════════════╗                   
                       ║                                       ║                   
                       ║   Reviewed                            ║                   
                       ║   3/18  abc333                        ║                   
                       ║   Add retry policy to client requests ║                   
                       ║                                       ║                   
                       ║   ●●●  3 comments left                ║                   
                       ║                                       ║                   
                       ║   ──────────────                      ║                   
                       ║                                       ║                   
                       ║   Next                                ║                   
                       ║   4/18  abc444                        ║                   
                       ║   Wire retry policy through Client    ║                   
                       ║   constructor                         ║                   
                       ║                                       ║                   
                       ╠═══════════════════════════════════════╣                   
                       ║   Enter continue   p back   q quit    ║                   
                       ╚═══════════════════════════════════════╝                   
                                                                                  
```

### What's doing the work

**It's a beat, not a screen.** At change 11/18, the reviewer's brain needs a moment to file what they just read before opening the next diff. Without the beat, deep-stack review becomes a blur.

**It records progress.** Pressing `Enter` advances the cursor (saved to `.jj-review/cursor.json`). If the reviewer quits here, `jjr --stack` resumes at change 4 next time.

**It's quiet.** No animations, no celebration. Just "you finished that, here comes the next one." Pressing `Enter` immediately is fine — it's a checkpoint, not a wall.

**Off by default.** The transition screen is friction the first time you encounter it on a small stack, and surprise is the wrong first impression. Default is `never`. Reviewers who want the beat on deep stacks can opt in:

```toml
[ui]
transition_screen = "never"  # "auto" | "always" | "never"; "auto" threshold is 8
```

## Screen 4 — Stack overview

Press `s` from the main view.

```
┌─ Stack: trunk()..@ ─────────────────────────────────────────────────────────────┐
│                                                                                  │
│   STACK-LEVEL COMMENTS                                                  ●●  2  │
│      ● required   Don't introduce new public APIs in this stack.               │
│      ● suggestion Consider folding the trait into the impl module.             │
│   ─────────────────────────────────────────────────────────────────────────    │
│                                                                                  │
│    1  abc11111  Refactor Client::send to extract request preparation       ✓   │
│    2  abc22222  Introduce RetryPolicy trait and default impl             ●●  2 │
│       ◆ change · suggestion · Naming overlaps with the existing Policy enum.   │
│ ▶  3  abc33333  Add retry policy to client requests                     ●●●  3 │
│       ◆ change · required · Split this — too many concerns in one commit.      │
│    4  abc44444  Wire retry policy through Client constructor               ✓   │
│    5  abc55555  Update client tests for retry behavior                    ●  1 │
│                                                                                  │
│   ─────────────────────────────────────────────────────────────────────────    │
│                                                                                  │
│   Stale comments    2   (press S)                                               │
│   Total comments    9   across 4 changes plus 2 stack-level                    │
│                                                                                  │
└─────────────────────────────────────────────────────────────────────────────────┘
 ↑↓ select  Enter open  c new comment  C → Claude (current)  q back   ?
```

### What's doing the work

**The revset is the title.** What you're looking at, named the way jj names it. No friendly translation.

**Stack-level comments live at the top.** They apply to the whole stack and are read first by both the reviewer and Claude. Listed inline with severity dot and a one-line preview; full body opens with `Enter`.

**Change-level comments are inset under their change.** Marked with a `◆` prefix to distinguish from line-comment counts. The change row's right-edge dot count includes change-level comments alongside line-level — a change with one required line comment and one required change comment shows `●●  2`. The reader can scan for hot spots without distinguishing scope at a glance; the inset row shows the change-level body when it matters.

**Comment counts use the same severity dots.** `●●●` means three required. The reader can scan the stack for "where are the hot spots." A change with three required comments looks different from a change with three notes.

**Done indicator (`✓`) for changes with no comments at any scope.** Not "approved" — `jjr` doesn't model approval. Just "nothing flagged here." This is the strongest "no comments" signal the tool ever shows; it does not mean the change is reviewed enough to ship.

**`c` from this view** opens the comment composer with scope defaulting from the cursor: a change row → scope=*change* on that change; the stack header → scope=*stack*. Pre-existing line/change/stack comments still open with `Enter` for editing.

### Column budget and truncation (80 cols)

Row format inside the borders, character budget at exactly 80-col terminal width:

```
sp(2) idx(2) sp(2) change-id(8) sp(2) description(filled) sp(2) dots(3) sp(2) count(2) sp(1)
```

That leaves the description column with whatever remains after the fixed-width fields and inset spacing. Concretely: at 80 cols (76 inside borders), description gets ~50 columns. Descriptions longer than the budget are truncated with `…` at the column where the dot column begins.

Inset comment preview rows (stack-level comment lines and `◆ change · …` lines) follow the same right-edge convention:

- **First line of body only.** Multi-line bodies are truncated at the first newline.
- **Hard truncate at the dots column** with `…` if the preview would collide.
- **Strip `│` and other box-drawing characters** from the body before rendering. Prevents a rogue character in a comment body from breaking the right border.
- **Strip ANSI escapes**, if any made it into a comment body, before rendering.
- **No quotes around the body.** The `· ` prefix is enough delimiter; quotes inside a quoted preview are visual noise and read inconsistently when the body itself contains quotes.

Stack-level comment bodies and change-level inset bodies share this rendering rule. A multi-line note still has a single-line preview here; the full body opens with `Enter` on the row.

### Resize behavior for this screen

- **120+ cols:** layout above. Description has plenty of room; previews rarely truncate.
- **100–119 cols:** description truncates, previews truncate. No structural change.
- **80–99 cols:** drop the `idx` column (the cursor `▶` already conveys position). Description and previews still subject to truncation.
- **<80 cols:** drop change-level inset preview text entirely; keep the `◆ change · severity` prefix only. The body is one keystroke away on `Enter`. Stack-level comment previews follow the same rule.

Below 60 cols, the tool refuses to render (per the global rule).

## Screen 5 — Stale comments

Press `S` (capital) from any view.

```
┌─ Stale comments · 2 ────────────────────────────────────────────────────────────┐
│                                                                                  │
│ ▶ src/client.rs · was line 87                            target_text changed    │
│   ╶─────────────────────────────────────────────────────────────────────────╴   │
│   ● required                                                                     │
│   "Replace .unwrap() with proper error handling — this will panic on a missing  │
│    config file in production."                                                  │
│                                                                                  │
│   was:    let cfg = read_config().unwrap();                                     │
│   now:    let cfg = read_config().expect("config required");                    │
│                                                                                  │
│   ─────────────────────────────────────────────────────────────────────────    │
│                                                                                  │
│   src/retry.rs · was line 23                                anchor not found    │
│   ╶─────────────────────────────────────────────────────────────────────────╴   │
│   ● suggestion                                                                   │
│   "Consider exponential backoff vs fixed delay."                                │
│                                                                                  │
│   was:    thread::sleep(Duration::from_millis(100));                            │
│   now:    (line not present in current diff)                                    │
│                                                                                  │
└─────────────────────────────────────────────────────────────────────────────────┘
 ↑↓ select  Enter view in source  d delete  e edit & re-anchor  q back
```

### What's doing the work

**Each entry shows the *delta* of why it's stale.** `was:` and `now:` make the mismatch concrete. This is the single most important affordance in the whole tool, because stale comments are where the reviewer loses trust if the UX is bad. Show them exactly what changed.

**Mismatch reason is the right-edge label.** Three values: `target_text changed`, `anchor not found`, `file not in diff`. Each has a different recovery flow.

**`R` reanchor manually.** Future affordance: drop into a "click a line to reanchor" mode. For MVP, only `e edit & re-anchor` (which opens the comment composer at a user-selected line). The binding is reserved but not surfaced in the footer until the feature ships — a grayed-out key in the discoverability surface is noise without action.

## Screen 6 — Send to Claude (confirmation)

Press `C` from the main view. Modal.

```
                                                                                  
       ╔═══════════════════════ Send to Claude ═══════════════════════════╗      
       ║                                                                   ║      
       ║   Change      abc333 — Add retry policy to client requests       ║      
       ║   Scope       current change                                      ║      
       ║                                                                   ║      
       ║   ────────────────────────────────────────────────────────────   ║      
       ║                                                                   ║      
       ║   Comments to send                                                ║      
       ║       scope    severity     count                                 ║      
       ║       stack    suggestion       1                                 ║      
       ║       change   required         1                                 ║      
       ║       line     required         2                                 ║      
       ║       line     suggestion       1                                 ║      
       ║       line     note             1                                 ║      
       ║                                                                   ║      
       ║   Files affected                                                  ║      
       ║       src/client.rs             3                                 ║      
       ║       src/retry.rs              1                                 ║      
       ║                                                                   ║      
       ║   Stale comments    2  excluded                                   ║      
       ║                                                                   ║      
       ╠═══════════════════════════════════════════════════════════════════╣      
       ║   v view full prompt    Enter send    Esc cancel                  ║      
       ╚═══════════════════════════════════════════════════════════════════╝      
                                                                                  
```

### What's doing the work

**Comment counts are a fixed grid.** One row per (scope, severity) pair with count > 0. Empty pairs are omitted, no continuation rows under blank labels. The reviewer scans down a column to know what they're sending. Modeled on `gh pr view`'s summary tables and lazygit's branch list.

**No teaching microcopy in the modal.** The grid is the summary; what each severity means lives in the help screen and the README. The reviewer has internalized it by cycle two. The same goes for stale exclusion: `excluded` is enough; the `--include-stale` CLI flag is documented in `--help` and isn't actionable from this modal anyway.

**Files-affected uses the same grid grammar** — file path in one column, count in the next. No "3 comments" / "1 comment" prose; the column header (count) does the work.

**Stack-level comments are always included**, even when sending only the current change. They give Claude the cross-cutting context. The grid row makes that visible without needing a separate explanatory line.

**`v` shows the full rendered prompt.** Same content as `jjr packet | less`. The reviewer can audit exactly what Claude will see, including the order: stack-level first, then per-change with change-level above line-level. This is non-negotiable for a tool whose contract is "this gets handed to an agent."

**Prompts are generated, not authored.** No edit-the-prompt path from this modal. To change what Claude sees, `Esc` to cancel, edit the comments, re-open.

## Screen 7 — Help

Press `?` from any view. Overlay.

```
┌─ jjr · keybindings ─────────────────────────────────────────────────────────────┐
│                                                                                  │
│  Movement                                                                        │
│      ↑ ↓     k j           line                                                 │
│      PgUp PgDn  ^u ^d      page                                                 │
│      Home End   g G        top / bottom                                         │
│      Tab        S-Tab      next / previous file                                 │
│      n          p          next / previous change                               │
│                                                                                  │
│  Comments                                                                        │
│      Enter      c          new / edit comment (scope defaults from cursor)     │
│      d                     delete (cursor on comment)                           │
│      e                     edit (cursor on comment)                             │
│      Tab                   fold / unfold (cursor on comment)                    │
│      1 2 3                 filter view by severity                              │
│                                                                                  │
│  Views                                                                           │
│      s                     stack overview                                        │
│      S                     stale comments                                        │
│      f                     file picker                                           │
│      ?                     this help                                            │
│                                                                                  │
│  Actions                                                                         │
│      C                     send current change to Claude                        │
│      x                     export comments                                       │
│      r                     refresh diff (re-run jj show)                        │
│      q                     quit                                                  │
│                                                                                  │
│  In comment composer                                                             │
│      ^L ^C ^K              scope:    line / change / stack                      │
│      ^1 ^2 ^3              severity: note / suggestion / required               │
│      ^X                    save                                                 │
│      Esc                   cancel  (^C inside the composer is captured as scope)│
│                                                                                  │
└─ Esc to close ──────────────────────────────────────────────────────────────────┘
```

### What's doing the work

**Categorized, not alphabetized.** The reviewer learns the tool by category. "Where do I edit a comment?" → Comments section.

**Both arrow and vim keys shown side by side.** Two columns of input convention, no judgment about which the reviewer should prefer.

**Composer keys are in their own section.** The composer is a different mode with different bindings; the help reflects that.

## Color and severity

Three colors carry semantic weight in the whole tool:

| Severity   | Dot | Use                                                |
|------------|-----|----------------------------------------------------|
| required   | red    | lines Claude must address                       |
| suggestion | yellow | lines Claude should address if safe             |
| note       | gray   | informational; Claude does not act on these     |

Plus four chrome colors:

| Element            | Color                                             |
|--------------------|---------------------------------------------------|
| Diff added line    | green text on default bg, no fill                 |
| Diff removed line  | red text on default bg, no fill                   |
| Stack current      | reverse-video on the current row only             |
| Comment border `┃` | dim cyan to separate visually from diff content   |

No color is load-bearing for color-blind users — every signal also has a shape (`●`, `▶`, `◐`, `+`/`-` prefix) or position (which row is the current change).

## Modes vs. modeless

The main review view is **modeless**: arrow keys move, `c` comments, `n` advances, all without entering a mode. This is the lazygit model and it works because there are few enough verbs to fit on one footer.

The composer is **modal**: while it's open, the main view's keys don't fire. Esc returns. This is the magit transient model.

Stack, stale, help, and Claude-confirm are also modal. Each has its own footer and exits with `Esc` or `q`.

The two interaction grammars never mix. The reviewer always knows: "am I on the diff, or am I in a thing." The chrome makes it visually obvious — modals have double-lined borders (`╔═╗`), the main view has single-lined borders (`┌─┐`).

## Resize behavior

The diff is the page; everything else gets out of the way at narrow widths.

- **120+ cols:** layout above.
- **80–119 cols:** stack bar abbreviates description with `…` if needed; file header drops the file count, keeps file path and comment count.
- **<80 cols:** comment metadata wraps to a second line; stack bar drops the dots and keeps just `3/5  abc333  Add retry…`.

Below 60 cols the tool refuses to render and prints a message. Reviewing diffs in a 50-column terminal is not a use case worth designing for.

### Main view footer

The footer at Screen 1 is the discoverability layer. At 80 cols it fits exactly:

```
 ↑↓ line  Tab file  n/p revision  Enter comment  s stack  S stale  C → Claude  ?
```

At narrower widths, drop bindings from the right edge with a trailing `…`, in this priority (least essential first):

1. `?` — replace with nothing; help is also reachable from any view.
2. `C → Claude`
3. `S stale`
4. `s stack`

Always preserve `Enter comment`, `Tab file`, `n/p revision`, `↑↓ line`. These are the irreducible surface; without them the reviewer can't do their job. A reviewer in a narrow terminal who needs the dropped bindings consults `?`.

Stack-overview screen has its own resize rules in Screen 4.

## What I want to defer

- **Side-by-side diff mode.** Real estate cost is too high for narrow terminals. Unified is the default for a reason. Add later if reviewers ask for it.
- **Syntax highlighting.** Nice to have, not load-bearing. The diff sign and color carry the necessary semantics. Add via `tree-sitter` later.
- **Search within diff.** `/` to search is a magit/lazygit staple but the diffs being reviewed are typically a few hundred lines. Add when someone hits the wall.
- **Multi-cursor / multi-select for bulk comment operations.** Premature.
- **Mouse support.** The whole point is keyboard fluency. Mouse can come later as a non-default.

## What this design is not

It is not a code editor. It is not a git client. It is not lazygit-for-jj.

It is a diff viewer with comment affordances and a stack-aware backbone, sized for the specific job of reviewing agent-generated changes before they leave the workstation. Keep that frame and the design stays honest.
