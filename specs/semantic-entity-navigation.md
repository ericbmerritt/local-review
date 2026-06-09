# Semantic Entity Navigation — Engineering Design Document

## User Narrative

### jjr (your own stack, pre-PR)

Eric finishes a session where Claude generated a five-change jj stack adding
OAuth refresh support. He opens `jjr` to review before pushing. The bottom of
the stack lands on the entity list for change `qvwktlml`:

```
 ≡ Add OAuth refresh tokens                        ● 1 comment
 ──────────────────────────────────────────────────────────────
 Δ AuthService.authenticate()    auth.rs   modified · sig+body
 ⊕ AuthService.refreshToken()    auth.rs   added
 ≈ Session.refresh()             session.rs  moved from auth.rs
 Δ parseToken()                  jwt.rs    modified · body
 Δ pool_size                     database.yml  5 → 20
```

He immediately sees the shape of the change without scrolling through 800 diff
lines. The signature on `AuthService.authenticate()` changed — sig+body — that's
where API-breaking risk lives. He drills in with `Enter`. The status bar reads
`authenticate()  modified · sig+body  ·  called from 8 places`. Eric knows
changing the signature here is broad-impact. He scans the line diff, sees that
Claude inverted the error-handling shape, and types `c` for a required comment:
"this swallows errors that should bubble — restore the early-return."

He `Esc`s back, `Tab`s through the rest of the entities. Twenty minutes later he
batches the comments to Claude via `C`. Claude addresses each by editing the
change in place; the reply is the codebase update.

### ggr (someone else's PR)

Priya is reviewing Marco's PR #847: "Refactor session management." She opens it:

```
ggr 847
```

PR description, then `n` to commit 1: "Extract token validation." The entity
list shows the shape of this commit immediately — two modifications, one new
function, one moved function — without the file dimension dominating. She sees
`Δ Session.parse()  session.rs  modified · sig+body` and knows the signature
change is the headline. She `Tab`s through entities, dropping line-scoped
comments where she has questions. `n` advances to commit 2 — cached extraction
means the entity list appears instantly. By the time she finishes commit 4 and
hits `S` to submit, she has accumulated comments spanning multiple entities in
multiple files, all routed back to the right GitHub review-comment line anchors.

## Purpose

Replace file-as-primary-navigation-atom with entity-as-primary-navigation-atom
across `jjr` and `ggr`. Surface what the reviewer most needs to see — _what
changed_ in semantic terms (functions, classes, modules, config properties) —
before the line diff.

This is one document covering both tools because the change is architectural,
not surface-level. The data model, the surface trait, the reviewed-bit storage,
the comment-anchor schema, and the Claude context pipeline all shift in
lockstep. Treating it as two parallel feature additions would invite drift.

## Goals

When this spec ships:

1. A reviewer entering a change in `jjr` or a commit in `ggr` lands on a list of
   semantic entities (functions, methods, classes, traits, modules, config
   properties) rather than a list of files. The file dimension is metadata on
   each row.
2. Each entity row shows the change kind (`Δ`, `⊕`, `⊖`, `≈`) and a semantic
   annotation (`modified · body`, `sig changed`, `5 → 20`, `moved from auth.rs`)
   telling the reviewer at a glance what risk class the change belongs to.
3. The reviewer drills from an entity to a focused file diff with `Enter` and
   walks adjacent entities with `Tab` / `Shift-Tab`. The unfocused full-file
   diff remains reachable as an escape hatch under `F`.
4. Comments stay line-anchored for posting (GitHub line comments; Claude line
   edits) but gain entity context and a re-anchor fingerprint for robust
   re-anchoring under renames and edits.
5. Reviewed-bit tracking shifts from per-file to per-entity, keyed by content
   hash so review status correctly resets when entity content changes.
6. Claude's review-remediation context in `jjr` upgrades from raw hunk text to a
   semantic bundle: target entity, direct dependencies, direct dependents, plus
   the hunk for the commented line.

## Non-Goals

This is not a file browser with entity annotations. The entity list is not a
secondary index bolted onto the existing file picker. It is the primary view
after entering a change. File-level access remains, demoted to an escape hatch.

This is not a dependency-graph viewer or an impact-analysis dashboard. We absorb
the cross-file dependency walk from sem-core for the Claude context bundle, but
we do not surface it as a navigation tier or render it as a panel.

This is not a replacement for the line diff. The line diff is the authoritative
view; it becomes a drill-down focused on an entity rather than the entry point.

This is not a language server, a static analyzer, or a refactoring tool. We
extract entities to organize a code review. We do not validate code, suggest
edits, or report cross-file correctness.

This is not a vendored library boundary. We absorb the relevant parts of
sem-core into `local-review-core` and own them. There is no upstream tracking,
no shim layer, no "wrapped sem-core" abstraction.

## Scope: jjr vs ggr

The capability surface diverges where the underlying tool surface diverges.
Capturing the asymmetry up front so later sections can refer to it.

| Capability                             | jjr                                         | ggr                                                       |
| -------------------------------------- | ------------------------------------------- | --------------------------------------------------------- |
| Entity list as primary view            | ✓                                           | ✓                                                         |
| Focused-diff drill-down                | ✓                                           | ✓                                                         |
| Container Rule, Jaccard move detection | ✓                                           | ✓                                                         |
| Per-entity reviewed-bit                | ✓                                           | ✓                                                         |
| Cosmetic-vs-structural tagging         | ✓                                           | ✓                                                         |
| File content via subprocess            | `jj file show`                              | `gh api graphql` (blob OIDs + text)                       |
| Graceful fallback row                  | ✓                                           | ✓                                                         |
| Description row                        | ✓ (change description)                      | ✓ (commit subject)                                        |
| Per-PR-scope comments                  | n/a                                         | ✓                                                         |
| File escape hatch (`F`)                | ✓                                           | ✓                                                         |
| Caller count in status bar             | ✓                                           | —                                                         |
| Claude context bundle                  | ✓ (full: entity + deps + dependents + hunk) | — (out of scope; ggr+Claude is its own feature, deferred) |

Caller count requires the local checkout, which `jjr` has and `ggr` does not.

The Claude bundle is `jjr`-only for a deeper reason than checkout access:
`jjr`'s Claude loop is "comment → Claude edits the code → codebase change is the
reply." That semantic does not translate to `ggr`, where the reviewer does not
own the code. Any ggr+Claude integration is a different feature with its own
design space (compose review text, pre-review pass, polish submission, on-demand
explanation, suggested-change blocks). It is not the "entity-only context
bundle" the earlier framing implied. Deferred to its own design pass — see Later
Enhancements.

## Decisions

These questions are settled. They are not subject to MVP re-litigation.

1. **Entities are the unit of review; files are the escape hatch.** Navigation
   leads with entities, not files. Files remain reachable via `F`.
2. **We absorb sem-core, we do not link it.** MIT/Apache-2.0 source ported into
   `crates/local-review-core/src/semantic/`. No git dependency, no subprocess,
   no upstream sync.
3. **Tree-sitter is the universal extractor.** Including for SQL —
   `tree-sitter-pgsql`, not `libpg_query` — because tree-sitter handles partial
   content gracefully and libpg_query does not.
4. **V1 supports thirteen languages:** Rust, TypeScript, JavaScript, Python, Go,
   Java, Scala, Kotlin, Bash, PostgreSQL, YAML, JSON, TOML. Others get the
   fallback row.
5. **Identity is structural, not a flat string.** `entity_id` is a tuple
   `(file_path, scope_chain, signature_key, ordinal)` — see Entity Model.
6. **Storage carries `(file, line, anchor_fingerprint)` plus optional
   `entity_id`.** Line is the irreducible handle. Entity id is a re-anchor hint
   and a display key. Anchor fingerprint protects against silent mis-anchoring
   on repeated content.
7. **Reviewed-bit is keyed by `(commit_id, entity_id, content_hash)`.** Bit
   resets when entity content materially changes, even if the entity itself
   persists.
8. **Caching is aggressive; eviction is deferred.** Per-commit cache files,
   storing extraction core data only (not UI-derived fields). Schema versioned.
   `R` refreshes the current commit only.
9. **Extraction is synchronous with progress feedback.** A tiered indicator
   model: silent under 300 ms, spinner glyph 300 ms - 1 s, modal overlay 1 s+.
10. **The cosmetic flag is a parser heuristic, not truth.** Cosmetic entries are
    shown in the default list with a `[cosmetic]` tag and dimmed style; `;`
    toggles them out entirely. Never auto-filtered.
11. **The entity diff view is a focused full-file diff, not a synthesized
    slice.** Renders the file's complete diff but pre-scrolls to the entity's
    anchor line and visually highlights the entity range.

## Principles

These are load-bearing truths. They shape every implementation decision.

### Entities are the unit of review; files are the escape hatch

The reviewer's first question after picking a commit is "what changed?" Phrased
semantically the answer is `authenticate()`, `validateToken()`, `legacyAuth()` —
not `auth.rs`, `auth.rs`, `auth.rs`. The file is metadata; the entity is the
unit.

This is the most load-bearing claim of the document. Every other principle is in
service of it.

### Storage serves posting; display serves understanding

Comments must round-trip back to the real world. GitHub's review-comment API
takes `(file, line, side, position)`. Claude's edit instructions in `jjr` say
"change this line." A comment that loses its line is a comment that cannot be
sent. So `(file, line)` is irreducible at storage.

Display reorganizes and enriches that data — entity grouping, scope paths,
semantic annotations, change-kind sigils, caller counts, cosmetic tags. The
display is allowed to be different shape from storage because they serve
different purposes.

This principle resolves edge cases: when re-anchoring fails for a comment, the
comment goes stale rather than silently demoting to entity-scoped. A stale
comment with a line is recoverable; an entity-scoped comment with no line is
not.

### The Container Rule

An entity that contains other entities — class, struct, trait, impl block,
module, namespace — appears in the entity list **only when the container itself
has changed**. Changed means: added, deleted, or its declaration modified
(visibility, signature, generic parameters, base class, trait bounds).

When only the _contents_ of a container change, the contained entities appear as
themselves, with the container surfaced as context via a scope-path prefix using
the language's native syntax.

This rule applies uniformly across languages:

- A Java class with three modified methods → three method rows, no class row.
- A Java class whose declaration changed (new generic param) → one class row.
- A Rust `impl Foo` whose block declaration changes (new trait bound, new
  associated type) → one impl row. A Rust `impl Foo` whose methods change but
  whose block declaration is unchanged → method rows only.
- A class whose declaration changes **and** whose methods also change → both
  rows appear. The class row says "the declaration changed"; the method rows say
  "these specific methods also changed."

**Tradeoff for moves between containers:** when a method moves from class A to
class B, neither A nor B appears in the entity list unless its own declaration
also changed. The reviewer sees one moved row (`≈ B.method moved from A.method`)
but does not see "class A lost one method" or "class B gained one method" as
structural rows. Reviewers needing the class-level view in that case use `F` to
reach the full file diff. This is an explicit tradeoff in service of keeping the
list focused on the most granular change.

### Absorption is the starting point, not the destination

We absorb sem-core's working code into `local-review-core` as v1. License
attribution stays (MIT / Apache-2.0). We own it from the moment we absorb it.

The goal is our own tuned implementation; we shortcut by starting from a working
codebase rather than reinventing it. No ongoing upstream sync. Sem's updates are
not our problem; if we want a fix from sem-core, we manually port it.

This is non-obvious. Teams treat vendored code as an external dependency to
preserve. We treat absorbed code as code we wrote that happened to start from
elsewhere.

### Graceful degradation is non-negotiable

If semantic extraction fails for a file — unsupported language, parse error,
ERROR-node-bearing parse, content fetch failure — that file appears in the
entity list as a single fallback row (`○ src/foo.rs`). The reviewer reaches its
diff via `Enter`. The tool never fails to show a change; it may fail to annotate
it semantically.

A partial parse (parse succeeds but contains ERROR nodes from syntax recovery)
counts as failure: the file becomes a fallback row, not a partial entity list. A
partial list would violate the reviewer's mental model that the list is the set
of changes.

Extraction failures are silent in the UI. The log captures them (`stderr_log`
machinery). The reviewer sees the fallback row and can investigate via the file
diff escape hatch.

## Navigation Model

The hierarchy is:

```
PR / jj stack
  └── commit (ggr) / change (jjr)
        └── entity list                ← primary navigation level
              └── entity diff view     ← focused file diff (pre-scrolled, highlighted)
```

File access remains as an escape hatch:

```
commit / change
  └── entity list
        └── [F] file list              ← demoted; opens existing file picker
              └── file diff view       ← unfocused full file diff
```

The reviewer enters a change and lands on the entity list. From there:

- `Enter` drills into the selected entity's diff (focused file diff)
- `Esc` or `q` from the entity diff returns to the entity list
- `Tab` / `Shift-Tab` cycles to the next / previous entity's diff without
  returning to the list, in list order (file then line position)
- `F` (uppercase) opens the file list — the escape hatch
- `n` / `p` advance / retreat in the stack to the next / previous commit /
  change (existing stack-navigation contract from the jjr and ggr specs)

The **entity diff view** and **file diff view** share rendering machinery (both
render the file's full diff via the existing `DiffView`) but differ in how they
enter:

- **Entity diff view:** pre-scrolled to the entity's anchor line, with the
  entity's line range visually highlighted (subtle background tint, sidebar
  glyph, or both — implementation choice). Header bar names the entity.
- **File diff view:** pre-scrolled to top, no entity highlighting, header bar
  names the file. Used as the escape hatch only.

This means "scoped to an entity" is a visual focus, not a structural slice. Line
numbers in the diff remain the file's real line numbers — so GitHub / Claude
line anchors work unchanged. A reviewer who scrolls outside the highlighted
range sees the rest of the file in context.

## Entity Model

The entity is the new central abstraction. This section defines it precisely.

### Terms

**Entity.** A semantically meaningful unit of code or data extracted by the
parser: a function, method, class, struct, trait, module, type alias, top-level
config property, or markdown section. Defined by the absorbed extractor's
tree-sitter queries per language; the working definition is "what someone
reviewing this change would recognize as a thing they can name."

**Container entity.** An entity that contains other entities: class, struct,
trait, impl block, module, namespace. Subject to the Container Rule.

**Scope chain.** The ordered list of container names enclosing an entity, from
outermost to innermost (e.g., `["auth", "Session", "parse"]` for a method
`parse` on class `Session` in module `auth`). Used to compute the display form.
Stored as part of `entity_id`.

**Display name.** Language-native rendering of the scope chain plus the entity
name: `AuthService.authenticate()` (Java / TypeScript dot notation),
`auth::session::refresh()` (Rust `::` notation), `Module::Class#method` (Ruby).
UI-only; the storage identity is `entity_id`.

**Change kind.** The relation between an entity's before and after state:
**Added** (entity present only after), **Deleted** (entity present only before),
**Modified** (entity present in both, content differs), **Moved** (entity
matched across file or scope boundaries via Jaccard similarity).

**Change annotation.** A short tag indicating _what kind of modification_
occurred. For code entities: `sig changed`, `body`, `sig+body`. For container
declarations: `sig changed`. For config properties: a value diff (`5 → 20`). For
SQL: more specific (`ALTER · ADD COLUMN x`).

**Cosmetic change.** A parser heuristic: the entity's AST hash, after the
grammar's normalization of whitespace and comments, is identical to its prior
version. Inherited from sem-core's `structural_change` flag. Not authoritative —
grammar-dependent. Visible in the default entity list, visually demoted; hidden
when `;` toggles cosmetic filter on.

### `entity_id`: the identity contract

`entity_id` is the structured identity for an entity. Every reference to "the
entity" by storage key refers to this tuple.

```rust
pub struct EntityId {
    pub file_path: PathBuf,        // repo-relative, UTF-8, controls stripped
    pub scope_chain: Vec<String>,  // each segment UTF-8, controls stripped
    pub signature_key: Option<String>, // language-specific param signature
    pub ordinal: u32,              // disambiguator for duplicate (file, scope, sig)
}
```

**`signature_key`** is the parameter signature in a language-normalized form:

- Java/Kotlin: `(int)` or `(String)` — distinguishes overloads
- Rust: `(&self, &str) -> Result<Token>` — strips body, keeps params and return
- TypeScript: includes generic parameters and types
- Python: omitted (`None`) — Python doesn't have signature-based identity in the
  same sense; falls back to ordinal for nested-function disambiguation
- SQL: `public.recompute_balance(integer)` — schema-qualified
- Container entities (classes, modules): `None` — containers are identified by
  scope and ordinal alone
- Config properties: `None` — the property name is the identity

**`ordinal`** is the disambiguator used when
`(file_path, scope_chain, signature_key)` is not unique within a file. It is the
zero-based index of the entity in `start_byte` order among entities sharing the
same first three fields. For the common case (no duplicates), ordinal is `0`.
Multiple Rust `impl Foo` blocks for the same type produce ordinals `0`, `1`, `2`
based on source order. Java overload pairs distinguished by `signature_key` keep
ordinal `0`.

Ordinal is more stable than a raw `start_byte`: when entities are inserted above
an existing entity, start_byte shifts but the entity's ordinal among its
duplicates stays the same (it's still the "second `impl Foo`" in the file).

**Stability across edits.** `entity_id` is stable across re-extractions when the
entity's identity-defining fields are unchanged. Renaming changes `scope_chain`
(the last element) → new `entity_id`. Moving across files changes `file_path` →
new `entity_id`. The absorbed Jaccard matcher links old and new `entity_id`s
when content similarity exceeds threshold; this linkage is metadata, not
identity.

**Serialization.** `EntityId` is serialized as JSON for cache files and comment
storage — not as a string concatenation. String concatenation with `::` or `/`
would collide with file paths (which may contain `::` in some languages' module
syntax) and Windows path separators. JSON sidesteps that entirely. Cache files
store the JSON tuple; the display name is computed fresh from the scope_chain at
render time.

**Control-character discipline.** All string fields in `entity_id` pass through
`strip_controls` at construction. External inputs that feed `entity_id` — file
paths from `gh api`, entity names from source code via tree-sitter — are
sanitized before tuple assembly. UTF-8 is preserved; only control characters are
stripped. This is the project default applied at the new boundaries (see
Gotchas).

### Invariants

- Every navigable entity has a non-empty name and a defined line range in its
  file.
- Every comment has a `(file, line)` pair, even if its `entity_id` is `None`.
  Line is the post-state line (the `+` side in unified diff terms) when
  available, the pre-state line otherwise.
- A container entity appears in the entity list **only if its own declaration
  changed**. Container entities never appear in the list solely because their
  contents changed.
- Cosmetic-only entities have `structural_change = false`. They appear in the
  default entity list, visually demoted, and disappear when `;` is toggled.
- The description "row" is not an entity. It is a separate piece of state on the
  entity-list view, not a sentinel in the `Vec<EntitySummary>`. (Project
  default: prefer `Option<T>` or a separate field over a typed sentinel.)
- A file with a parse that contains ERROR nodes appears as a fallback row, not
  as a partial entity list. Partial extraction is treated as full failure.

### `EntitySummary`

```rust
pub struct EntitySummary {
    pub id: EntityId,
    pub display_name: String,        // language-native scope path; computed
    pub kind: EntityKind,
    pub change: ChangeType,
    pub annotation: ChangeAnnotation,
    pub file_path: PathBuf,          // copy of id.file_path for display
    pub source_file: Option<PathBuf>, // populated for ChangeType::Moved
    pub target_line: Option<u32>,
    pub line_range: (u32, u32),      // start/end line in the file's after state
    pub structural_change: bool,     // false = cosmetic (parser heuristic)
    pub content_hash: u64,           // hash of entity's after-state content
    pub comment_count: usize,        // computed at render time
}
```

`EntitySummary` is the rendered view of an entity. The cache stores only the
**extraction core** (id, kind, change, annotation, line_range,
structural_change, content_hash, source_file, target_line). UI-derived fields
(`display_name`, `comment_count`, `file_path`) are computed at render time. This
decouples the cache from UI versioning.

The description row is not an `EntitySummary`. It is a separate `Option<...>` on
the parent view state holding the commit subject and change-scoped comment
count.

## Comment Model

Comments are line-anchored (irreducible), entity-aware (enhancement), and
display-organized by entity wherever possible. The model below extends the
existing `GgrAnchor::Line` and `jjr` anchor schemas — see those crates' specs
for the unchanged base behavior.

### Schema

A comment captures at creation time:

- `file: PathBuf` (required)
- `line: u32` (required) — post-state line where available, pre-state otherwise
- `entity_id: Option<EntityId>` (new; `None` for old drafts or for comments on
  lines outside any entity)
- `anchor_fingerprint: AnchorFingerprint` (new; required) — confidence-based
  re-anchoring metadata

```rust
pub struct AnchorFingerprint {
    pub line_hash: u64,        // hash of the anchored line's content (whitespace-normalized)
    pub before_hash: u64,      // hash of the line above
    pub after_hash: u64,       // hash of the line below
}
```

The line is the source of truth. The entity_id is a re-anchoring hint and a
display key. The fingerprint protects against silent mis-anchoring when an
entity contains repeated lines (e.g., multiple `return Err(e)` statements).

### Re-anchoring

When the underlying commit content changes (jjr re-edit, ggr force-push), the
re-anchoring pipeline runs:

1. **Locate target entity.** If the stored `entity_id` is present and the
   extractor finds a matching entity in the new revision (exact match by
   `entity_id`, or Jaccard rename match), use that entity's new line range for
   step 2. If no entity match, skip to step 3.

2. **Find line within entity by fingerprint.** Scan the matched entity's line
   range. For each candidate line, compute its `AnchorFingerprint` and a match
   score against the stored fingerprint:
   - 3 points: line_hash matches
   - 1 point each: before_hash, after_hash matches
   - Maximum score: 5

   Accept the highest-scoring line if its score is **≥3** (line match required).
   On tie, prefer the line at the same relative offset within the entity. If no
   candidate scores ≥3, the comment is stale.

3. **Fallback: file-wide search.** Only reached when `entity_id` is `None`
   originally (orphan comment) or the entity is missing entirely. Same
   fingerprint scoring across the whole file. Same threshold: ≥3. Below
   threshold → stale.

The output is always a new `(file, line, entity_id)`. The stored `entity_id` is
overwritten with whichever entity now contains the matched line. The pre-rewrite
`entity_id` is not preserved — its job was to guide re-anchoring.

This pipeline replaces the previous file-wide fuzzy line matching from the jjr
and ggr base specs. The fingerprint discipline is new; old drafts without
fingerprints fall back to the previous behavior until they are re-saved.

### Display

The entity that contains the comment's current line gets a `●` dot on its row in
the entity list, colored by severity. Comments on lines outside any entity
attach to a fallback row — the file appears in the entity list with a `●` even
when no entities in that file changed.

The three comment scopes from the base specs map onto this display:

- **Line-scoped, line inside entity X:** `●` dot on entity X's row.
- **Line-scoped, line outside any entity:** `●` dot on the file's fallback row.
- **Change-scoped (commit-level):** `●` dot on the description row.
- **PR-scoped (ggr only):** displayed on the PR-level stack overview.
- **Reply:** displayed nested under its parent in the existing thread UI.

### Posting to GitHub (ggr)

GitHub's review-comment API requires a `side` and a `position` for line
comments:

- Line in the added or context region of the diff → `side: RIGHT`, `position`
  computed from the HEAD diff offset
- Line in the removed region → `side: LEFT`, `position` computed from the BASE
  diff offset

The stored `line` is the resolved line in the current state. The `side` and
exact GitHub `position` are computed at posting time from the line's role in the
diff hunk. The reviewer never sees these details — they are posting-layer
concerns. The `gh api` PR-review-comment endpoints accept the
`(file, side, position, body)` shape directly.

### Handoff to Claude (jjr)

Claude receives `(file, line)` plus entity context as understanding-enriching
prose ("this comment is on line X inside `authenticate()`, which is called from
8 places"). Claude's edits target the line; the entity context is for its
reasoning. See the Claude Context Enrichment section for the full bundle.

## TUI Design

The visual surface across the entity list, entity diff, and supporting chrome.

### Entity list screen

Replaces the file diff as the entry point for a commit / change. Layout at
80×24:

- Stack bar: 3 rows (existing)
- Description row + divider: 2 rows
- Entity list body: 18 rows
- Footer: 1 row

Example body content:

```
 ≡ Implement OAuth refresh tokens                    ● 3 comments
 ─────────────────────────────────────────────────────────────────
 Δ AuthService.authenticate()    auth.rs       modified · body
 ⊕ AuthService.refreshToken()    auth.rs       added
 Δ AuthService                   auth.rs       sig changed
 ≈ Session.refresh()             session.rs    moved from auth.rs
 Δ parseToken()                  jwt.rs        modified · sig+body
 Δ pool_size                     database.yml  5 → 20
 ○ src/utils/imports.rs                        ● 1 comment
```

#### Row format

- 2-char pad + 2-char sigil + space = 5 chars consumed
- Entity name (with scope path): up to 28 chars, truncate with `…`
- File path: up to 15 chars in a fixed-start column; leading directory segments
  truncated to keep the filename (`src/auth/login.ts` → `login.ts`)
- Annotation: remaining width (~18-20 chars); when annotation overflows,
  truncate the source path of moves first, keeping the prefix
- Comment dot `●` right-aligned with severity color when present

#### Sort order

Entities sort by file path, then by line position within file. Entities from the
same file end up adjacent; the repeating filename in the file column visually
groups them. No explicit file headers — the file column on each row carries the
grouping.

This matters: changes in the same file usually relate, and the diff is a
per-file artifact. The reviewer naturally reads in document order. Sorting any
other way (by change kind, by entity type, by severity) would impose a structure
the diff itself does not have.

#### Sigils, colors, and modifiers

| Sigil | Meaning           | Color                       |
| ----- | ----------------- | --------------------------- |
| `≡`   | Description row   | Default (no semantic color) |
| `Δ`   | Modified          | Yellow                      |
| `⊕`   | Added             | Green                       |
| `⊖`   | Deleted           | Red                         |
| `≈`   | Moved             | Gray                        |
| `○`   | File fallback row | Gray                        |

Modifiers (compose on top of the table):

- **Cosmetic entity:** annotation column suffixed with `[cosmetic]`, foreground
  dimmed to DarkGray. Sigil and change-kind color unchanged.
- **Reviewed entity:** `✓` overlay or adjacent glyph next to the sigil. Exact
  treatment defined during implementation.
- **Has comments:** `●` dot right-aligned, severity color.

The `≡` sigil for the description row was chosen because it visually reads as
horizontal lines (document body), distinct from the change-kind sigils which all
reference geometry of change.

#### Description row

The pinned first row of the entity list. Sigil is `≡`. Text is the commit
subject (jjr: change description first line; ggr: commit message subject).
Annotation column shows change-scoped comment count if any.

`j`/`k` lands on it like any row. `Enter` opens the full description screen. `c`
here creates a change-scoped comment attached to this row.

#### Bindings on the entity list screen

```
j / ↓          move down
k / ↑          move up
Enter          open entity diff (or description screen if on description row)
F              open file list (escape hatch)
Tab            open next entity's diff
Shift-Tab      open previous entity's diff
c              open comment composer (scope follows cursor)
1 / 2 / 3      severity filter (existing)
;              toggle cosmetic filter (hide / show cosmetic entities)
n / p          stack navigation: next / previous commit / change (existing)
R              refresh current commit (re-fetch, re-extract, clear cache)
?              help
q              quit
```

`f` (lowercase) is unbound on the entity list screen. The file picker is reached
via `F` (uppercase).

### Entity diff view

Renders the file's full diff (existing rendering machinery), with three changes:

1. **Header row.** Becomes `authenticate() · auth.rs · 3 of 8 entities`.
2. **Initial scroll.** Pre-scrolled to the entity's anchor line on entry.
3. **Entity range highlight.** Lines within the entity's line range (computed
   from the entity's extraction) are visually marked. Default treatment: a
   subtle background tint and a sidebar glyph. The reviewer sees the entity in
   context with the rest of the file legible above and below.
4. **Status bar.** Shows passive entity context — see below.

All other existing bindings (`c`, `e`, `d`, `r`, navigation keys, severity
filters) work unchanged. `Esc` or `q` returns to the entity list. `Tab` /
`Shift-Tab` advance / retreat to the next / previous entity's diff in list
order. `F` opens the file list (escape hatch).

Because the diff is the file's real diff with real line numbers, comment anchors
and `gh api` line positions are correct without translation. The "focus" is
purely visual.

### Passive context in the status bar

When the cursor is within a changed entity's line range, the status bar shows:

```
authenticate()  modified · sig+body  ·  called from 8 places
```

The status bar is the existing slot for transient and persistent messages. This
context is persistent while the cursor is in range, yielding briefly to
transient messages (save confirmations, etc.) and returning when they clear.

The "called from N places" segment appears in `jjr` only. In `ggr` the context
stops at the change annotation.

### File list (escape hatch)

Reached via `F`. Renders the existing file picker UI (no change). Selecting a
file opens the file diff view, unchanged (full diff, no entity highlighting).
This path retains whole-file access for cases where the focused view obscures
information the reviewer needs (a hunk spanning multiple entities, broader
context, "this class lost 3 methods" structural view via the source file).

The file diff view continues to use the existing per-file reviewed bit. The
entity diff view uses the new per-entity bit. The two are independent.

## Semantic Extraction Layer

The new layer that turns file content into `Vec<EntitySummary>`.

### Sem-core absorption

We absorb the relevant portions of
[sem-core](https://github.com/Ataraxy-Labs/sem) (MIT / Apache-2.0) directly into
`crates/local-review-core/src/semantic/`. No library dependency, no git ref, no
subprocess.

Absorbed modules (adapted to our types and lints):

- `parser/plugin.rs` — `SemanticExtractor` trait (renamed from
  `SemanticParserPlugin`; same shape, language-neutral name to fit any future
  non-tree-sitter extractor)
- `parser/plugins/code/` — language-specific tree-sitter query implementations,
  one per v1 language except SQL (see below)
- `parser/differ.rs` — entity extraction + change classification
- `parser/registry.rs` — language detection and extractor dispatch
- `model/entity.rs`, `model/change.rs`, `model/identity.rs` — `SemanticEntity`,
  `SemanticChange`, `ChangeType`, Jaccard similarity matcher
- Dependency-graph + context-budgeting logic from sem-core's `context` command
  (used in the jjr Claude integration only)

Dropped entirely: git bridge, all sem-core formatters, CLI code,
impact/blame/log commands.

The absorbed code is adapted to our lints — no `unwrap`, no `as` casts, errors
flow through `Result<T, JjrError>` (jjr) or `Result<T, GgrError>` (ggr) at the
crate boundaries, and through `LocalReviewError` inside `local-review-core`.
`strip_controls` applied at every external-input boundary (see Gotchas).

### Languages in v1

Thirteen languages, all via tree-sitter:

- **Code:** Rust, TypeScript, JavaScript, Python, Go, Java, Scala, Kotlin, Bash.
  Absorbed plugin shims from sem-core's existing implementations.
- **Config:** YAML, JSON, TOML. Each plugin extracts top-level properties as
  entities; nested objects/maps surface their keys as nested entities (so
  `database.pool.size` is one entity, not three). Comments and value-only
  changes are tracked per property.
- **SQL:** PostgreSQL via `tree-sitter-pgsql`. Bespoke plugin we write ourselves
  (sem-core does not ship a SQL plugin).

Other languages and other SQL dialects get the fallback row.

### SQL specifically

We chose `tree-sitter-pgsql` over `pg_query` (libpg_query bindings — the actual
PostgreSQL parser) because tree-sitter handles partial and malformed input
gracefully via incremental parsing and error nodes. libpg_query is strict — the
first syntax error aborts the parse, no partial AST. For a review tool where
files may be WIP, may use Postgres features newer than the libpg_query version,
or may use extension-specific syntax (TimescaleDB, pgvector, PostGIS),
tree-sitter's resilience matters more than libpg_query's fidelity.

Entities the SQL plugin extracts:

- `CREATE TABLE` / `ALTER TABLE` → table entity
- `CREATE INDEX` → index entity
- `CREATE OR REPLACE FUNCTION` → function entity (the high-value case for
  plpgsql review)
- `CREATE VIEW` / `CREATE MATERIALIZED VIEW` → view entity
- `CREATE TYPE` → type entity
- `CREATE TRIGGER` → trigger entity
- `CREATE POLICY` → RLS policy entity
- `CREATE SCHEMA` / `CREATE EXTENSION` → schema / extension entities

**SQL-specific rules:**

- **Schema qualification.** `public.recompute_balance()` and
  `private.recompute_balance()` are distinct `entity_id`s. The schema is part of
  `signature_key` where applicable.
- **DO blocks.** `DO $$ ... $$` anonymous blocks appear as `anonymous block`
  rows when changed. Their internal contents are not parsed for sub-entities
  (PL/pgSQL inside DO blocks is opaque at this layer).
- **Multiple statements per file.** Migration files commonly contain 10–50
  top-level statements. Each becomes an entity row; the entity list reflects
  that.
- **Transaction wrappers.** `BEGIN; ... COMMIT;` wrappers are structural noise;
  their contents are extracted as if at top level.
- **Same-name in same file.** When the same entity is created and dropped in one
  file (e.g., a setup/teardown pattern), both operations appear as separate rows
  with different ordinals.
- **ALTER TABLE specificity.** `ALTER TABLE foo ADD COLUMN x` is annotated on
  the table entity as `ALTER · ADD COLUMN x` rather than generic `body`.

If `tree-sitter-pgsql` misses Postgres-specific constructs we care about, the
right move is to contribute upstream rather than swap to libpg_query — keeps the
architectural assumption (all extractors are tree-sitter) intact.

### Adding languages later

A new language ships as: tree-sitter grammar crate (dep), a plugin file defining
the entity-extraction queries, and a feature-flag entry. One-PR change with no
design impact. We do not carry sem-core's 28+ grammars by default — no evidence
of demand for languages outside the v1 set.

### Content fetching

The extractor takes **full file content** for before and after states. Partial
content yields unreliable parses.

**jjr.** For each file in the `Diff`, call `jj file show -r <parent_rev> <path>`
and `jj file show -r <rev> <path>`. Local subprocess, fast. Synchronous at
extraction time. Added and deleted files have one side empty.

**ggr.** A single GraphQL query per commit fetches all changed files'
before/after blob OIDs and text content:

```graphql
query($owner:String!, $repo:String!, $base:String!, $head:String!) {
  base: repository(owner:$owner, name:$repo) {
    object(expression:$base) {
      ... on Commit {
        # walk the tree for each changed file path
      }
    }
  }
  head: repository(owner:$owner, name:$repo) {
    object(expression:$head) {
      ... on Commit { ... }
    }
  }
}
```

One round trip per commit, regardless of file count (within GraphQL response
size limits). Fallback for very large commits: split into multiple GraphQL
queries by file batch. The contents API is **not** used — it base64-encodes
responses, has stricter size limits, and requires one HTTP call per file.

In both tools, content-fetch failure for a single file causes that file to fall
back to the "no entities" row. One file's failure does not abort the rest of the
extraction.

## Caching & Loading

### Loading is a design surface

A TUI without explicit feedback feels stuck. Every operation exceeding ~200 ms
must communicate three things: that work is happening, where in the work we are,
and how to bail out. Loading indicators are the only signal the reviewer has.
They are designed explicitly here.

### Cache location

Per-commit extraction results are cached to disk. Read-then-decode is much
faster than re-extracting.

- **ggr:**
  `$XDG_DATA_HOME/ggr/cache/entities/<owner>/<repo>/<pr>/<commit_sha>.json`
- **jjr:** `<repo_root>/.jj-review/entities/<change_id>-<content_hash>.json`

### Cache contents and schema versioning

The cache stores **extraction core data only**, not UI summaries:

```rust
pub struct CacheEntry {
    pub schema_version: u32,
    pub entities: Vec<EntityCoreData>,
    pub file_failures: Vec<FailedFile>,
}

pub struct EntityCoreData {
    pub id: EntityId,
    pub kind: EntityKind,
    pub change: ChangeType,
    pub annotation: ChangeAnnotation,
    pub line_range: (u32, u32),
    pub source_file: Option<PathBuf>,
    pub target_line: Option<u32>,
    pub structural_change: bool,
    pub content_hash: u64,
}
```

UI-derived fields (`display_name`, `comment_count`, `file_path`) are computed at
render time from `EntityCoreData`.

`schema_version` bumps on any breaking change to `EntityCoreData`. Mismatched
versions invalidate the entry and re-extract.

### Invalidation

- **ggr:** key by commit SHA. PR commits are immutable. Cache entries outside
  the current PR's commit list are unreachable but not actively pruned.
- **jjr:** key by `(change_id, content_hash)`. jj changes are mutable; same
  change ID can have different content after a re-edit. The content hash detects
  this.

When `R` is pressed: invalidate the cache entry for the current commit / change
only, then re-fetch and re-extract. Whole-PR refresh is deferred.

### No eviction in v1

Cache grows monotonically. Each entry is small (JSON, kilobytes per commit).
Manual `rm -rf` of the cache directory recovers disk if needed. Pruning command
deferred.

### Loading indicators by tier

| Duration                 | Indicator                            | Where                     |
| ------------------------ | ------------------------------------ | ------------------------- |
| 0–300 ms                 | Nothing                              | —                         |
| 300 ms – 1 s             | Braille spinner glyph (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) | Status bar                |
| 1 s+                     | Modal overlay with progress + counts | Centered, blocks viewport |
| Background work (future) | Spinner glyph in status bar          | Non-blocking              |

Spinner uses Braille (TUI canon — gh, nix, cargo). ASCII fallback (`| / - \`) is
selected by a separate signal — Unicode glyph support — not by `NO_COLOR`.
`NO_COLOR` only disables color; users with `NO_COLOR=1` may still have full
Unicode rendering and should keep the Braille spinner. The fallback signal is
either an explicit env var (e.g., `JJR_ASCII_SPINNER=1` / `GGR_ASCII_SPINNER=1`)
or, where reliably detectable, the locale / terminal capability (`LANG`,
`TERM`). Defer to a single env var unless terminal capability detection lands
cleanly.

### Overlay format

```
 ⠋ Extracting entities  ·  23 / 50 files  ·  3 failed
   Esc to cancel
```

### Per-file errors

Per-file failures do not abort. Counter shows `(N failed)`. Failed files appear
as fallback rows. Log captures details.

### Cancellation

`Esc` during extraction cancels remaining work. Already-extracted files keep
their data; cancelled files become fallback rows. Reviewer returns to the entity
list with partial result and can re-trigger via `R`.

### Single block point

Extraction blocks at one transition: entering a commit / change for the first
time. Cached commits are instant. Navigation within a loaded commit is instant.
Stack overview / PR commit list does not block on extraction.

## Reviewed-Bit Model

The per-file reviewed bit becomes a per-entity reviewed bit. The two coexist.

### Storage

```rust
pub struct ReviewedBit {
    pub commit_id: CommitId,
    pub entity_id: EntityId,
    pub content_hash: u64,        // entity's content hash at review time
}

// Stored as Set<ReviewedBit> for entity-level, Set<(CommitId, PathBuf)> for file-level
```

The `content_hash` field is essential: review status applies to the _content the
reviewer saw_, not the entity's identity in isolation. When the entity's content
changes (a jj squash modifies the body, a force-push introduces new code), the
lookup for `(commit_id, entity_id, new_hash)` misses — the entity appears
unreviewed again.

Old per-file bits remain valid for fallback rows and the file diff escape hatch.
New entity bits accumulate. A file with old per-file bits and new per-entity
bits: per-entity bits drive the entity list display; the per-file bit drives the
file diff view independently.

### Trigger

Entering an entity's diff marks that entity reviewed at its current content_hash
(auto, matching existing per-file auto-mark). `Tab`-cycling through entities
marks each as the reviewer lands on it.

Fallback rows mark via the existing per-file path: entering the file diff view
marks the file.

### Visual

`✓` next to reviewed entity rows. Fallback rows use the existing `✓` next to the
file. A commit is fully reviewed when every entity row and every fallback row is
marked. Stack overview rolls up the same way the existing per-commit aggregation
does, with entities feeding the rollup instead of files.

### Re-anchoring of reviewed bits

When the Jaccard matcher links `entity_id_old` to `entity_id_new` across a
re-edit or force-push:

1. Look up reviewed bits for the new `entity_id_new` at its new `content_hash`.
   If present → mark reviewed.
2. Look up reviewed bits for the old `entity_id_old` at its old `content_hash`.
   If the new entity's `content_hash` matches the old one (content didn't
   change, only identity moved), carry forward the reviewed bit under the new
   `entity_id`.
3. If content changed, the bit doesn't carry — reviewer must re-review.

This is strictly better than the per-file model: addressing one comment in a
file resets that entity's bit (because content changed for that entity) but
leaves sibling entities marked.

## Cosmetic Filtering

Cosmetic detection is a **parser heuristic**, not a classification of intent.
Tree-sitter grammars normalize whitespace and comments differently per language;
some changes will be flagged cosmetic when a careful reviewer would disagree,
and vice versa. The flag is useful as a default-visible signal, never as
authoritative truth.

### Default view

All entities show in the list. Cosmetic entities are visually demoted:

- Foreground dimmed (DarkGray)
- Annotation suffixed with `[cosmetic]`
- Sigil and change-kind color unchanged

The reviewer's eye learns to skip them when scanning. They remain visible; the
reviewer always knows what is in the commit.

### Toggle

`;` toggles cosmetic visibility. Off → cosmetic entities disappear. Footer hint
shows current state.

This is the reformatter-PR workflow: open a 200-entity list, hit `;`, see the 4
entries that actually changed.

### Composition with severity filter

The cosmetic toggle is orthogonal to severity filters (`1`/`2`/`3`). They
compose with AND. Concrete: severity `required` with cosmetic hidden shows only
entities whose severity is `required` and `structural_change = true`.

## Claude Context Enrichment (jjr only)

### Scope: why jjr only

The Claude integration in `jjr` is shaped by the loop it closes: reviewer leaves
comments → Claude edits the working copy → the codebase change is the reply.
Claude is _addressing_ comments by editing code the reviewer owns.

`ggr` reviews other people's PRs. The reviewer does not own the code. A
ggr+Claude integration would not be "Claude addresses comments by editing" —
that semantic doesn't translate to a remote-PR review tool where the author
addresses comments on their own branch.

ggr+Claude is therefore a different feature, not a context-bundle variation of
jjr+Claude. It has its own design space (compose review text, pre-review pass,
polish submission, on-demand explanation, suggested-change blocks) and is
deferred until that design is done. See Later Enhancements.

### jjr — full bundle

Per comment:

1. Comment text and severity (existing)
2. Target entity — full body, current state
3. Direct dependencies — entities the target calls
4. Direct dependents — entities that call the target
5. Diff hunk for the commented line

Token-budgeted, packed in priority order. Replaces raw-hunk prompt.

### Budget

Default 16 000 tokens per comment. Override via `JJR_CONTEXT_BUDGET` (integer
token count).

More generous than sem-core's 8k default because Claude's context window is
large and review benefits from richer context.

### Truncation rule

Items packed in priority order (1 → 5). Items 1 (comment) and 2 (target entity)
and 5 (diff hunk) are required and never dropped. Items 3 (deps) and 4
(dependents) are dropped progressively when over budget — dependents first, then
dependencies. Within either, drop whole entries rather than truncating
individual ones.

The Claude prompt includes a one-line note when truncation happened ("context
truncated to budget; 4 of 7 dependents omitted") so Claude knows the picture is
partial.

## Implementation Impact

The existing codebase encodes file-as-atom at every major seam. Naming the seams
here prevents the implementation from treating this as a feature addition rather
than a model change.

### `App` state

`app.file_index` is currently the primary navigation cursor. Under the new
model, `app.entity_index` is the primary cursor within the entity list;
`app.file_index` is secondary (file diff escape hatch only).

`Screen::Main` renders the entity list. New state
`Screen::EntityDiff { entity_idx }` represents the drill-down (focused full file
diff). Existing file diff becomes `Screen::FileDiff { file_idx }`, reached only
via `F`.

### `ReviewSurface` trait

`fetch_views` currently returns `Vec<DiffView>` indexed by file. Two new methods
become the primary surface:

- `fetch_entity_list(entry_idx) → Vec<EntitySummary>` — entity list view data.
- `fetch_entity_diff(entry_idx, entity_idx) → DiffView` — the file's `DiffView`
  plus the entity's line range for highlighting / scrolling.

Description-row content lives in a separate method (`fetch_description_summary`)
since it is not an entity.

`fetch_views` is retained for the file diff view path. Surfaces that cannot
produce entity data return an empty `Vec<EntitySummary>`; the tool surfaces
every changed file as a fallback row.

### Reviewed-bit storage

`is_view_reviewed(view_idx)` → file index. New contracts:

- `is_entity_reviewed(commit_id, entity_id, content_hash) → bool`
- `is_file_reviewed(commit_id, file_path) → bool` (existing, preserved)

The on-disk schema gains an entity layer with content_hash; the existing file
layer is preserved for fallback rows.

### Description view

Description was view index 0. Now: the description is the pinned first row of
the entity list with content from `fetch_description_summary`. `Enter` opens the
existing description screen. The `file_index == 0 → description` special case is
removed from `App`.

### `Tab` / `Shift-Tab` semantics

Cycles entities in list order. Within entity diff, advances to next / previous
entity's diff. Stack-level `n` / `p` unchanged.

### `fetch_views` callers

Every call site needs evaluation. Some are file diff path (keep). Some are the
entry path (replace with `fetch_entity_list` + `fetch_entity_diff`). Migration
is mechanical but spans both crates.

### File-header chrome

Dropped on the entity list screen. On the entity diff view becomes
`authenticate() · auth.rs · 3 of 8 entities`. On the file diff view stays as-is.

## Testing Strategy

The migration touches identity, persistence, re-anchoring, and rendering. Each
layer needs its own test discipline.

### Unit-level

- **Identity uniqueness.** For each v1 language, fixtures with overloads,
  multi-impl blocks, nested scopes, and config properties produce distinct
  `entity_id`s.
- **Identity stability.** Modifying body without changing signature preserves
  `entity_id`; renaming the entity changes `entity_id` (Jaccard match is
  separate).
- **Ordinal determinism.** Multiple `impl Foo` blocks in source order produce
  ordinals 0, 1, 2 — and adding a third entity above does not reshuffle ordinals
  0 and 1.
- **Container Rule.** Fixtures with body-only changes don't surface container
  rows. Declaration-only changes surface only container rows. Both changing
  surfaces both.
- **Cosmetic detection.** Per-language fixtures: formatter-only changes flagged
  `structural_change = false`; semantic edits flagged `true`. Python indentation
  changes are flagged structural (semantic).
- **Strip-controls discipline.** External strings with control characters (from
  synthetic `gh api` responses, synthetic file paths) produce `entity_id`s with
  controls stripped.

### Property-level (re-anchoring)

Generate edit scripts on a base file and assert behaviors:

- Pure body edits preserve comment line anchoring within the entity.
- Renames matched by Jaccard carry comments to the new `entity_id`.
- Renames below Jaccard threshold mark comments stale, not silently migrated.
- Repeated-line content (multiple `return Err(e)` lines) does not cause silent
  comment migration to the wrong instance — the `AnchorFingerprint` confidence
  threshold catches this.
- Confidence-threshold tests: a comment with fingerprint scoring below 3 against
  any candidate marks stale rather than picking the highest-scoring weak match.

### Integration

End-to-end fixture per tool:

1. Commit with N entity changes → entity list shows N rows
2. Drill into one → focused file diff opens at correct line
3. Leave a comment → comment captured with correct
   `(file, line, entity_id, anchor_fingerprint)`
4. Re-edit the commit (jjr: `jj squash`; ggr: synthetic force-push)
5. Re-open: comment is correctly re-anchored or correctly marked stale; reviewed
   bits clear for entities whose content changed.

### Performance budgets

Reported by CI; regressions trigger a triage task, not a build failure.

- **jjr extraction cold path:** typical change (3–10 files) under 500 ms.
- **jjr extraction cache hit:** under 100 ms.
- **ggr extraction cold path:** typical commit (5–20 files) under 2 s via single
  GraphQL batch.
- **ggr extraction cache hit:** under 100 ms.
- **jjr caller count for direct dependents:** under 200 ms after extraction
  completes (per entity, on focus).

## Gotchas and Constraints

### Tree-sitter handles partials; libpg_query does not

We pick tree-sitter for SQL because of this. Anyone evaluating "should we use
the real PostgreSQL parser instead" needs to remember: a strict parser cannot
survive WIP commits, version skew, or extension syntax. Resilience matters more
than fidelity in a review tool.

### Tree-sitter grammar pinning and drift detection

Tree-sitter grammar updates can change node names, structure, or query
semantics. An update that works in isolation can silently break our extraction
queries — what was `function_definition` becomes `function_declaration`.

Policy:

- Pin every grammar to an exact version in `Cargo.toml` (no `^` or `~` ranges).
  Use `=0.21.0` style.
- Maintain a golden-test corpus per language: 5–10 representative source files
  with expected entity extractions checked in.
- Grammar version bumps require golden tests to pass; any drift surfaces as test
  failure.
- Grammar updates are deliberate, not automatic. We do not subscribe to
  `cargo update` for tree-sitter grammar crates.

### Cache invalidation on jjr re-edits requires content hashing

jj change IDs are stable across re-edits. Without `content_hash` in the cache
key, the cache would return stale entity lists after `jj squash` / `jj edit`.
The `(change_id, content_hash)` keying is non-negotiable for jjr correctness.

### Strip control characters at every new string boundary

The project default applies: any string flowing from external input through a
public error variant or a user-visible TUI string is passed through
`strip_controls` at the boundary. New boundaries this spec introduces:

- Entity names extracted from source content via tree-sitter
- File paths flowing back from `gh api graphql` responses (ggr)
- Error messages from `jj file show` and `gh api` subprocesses
- Scope-chain elements assembled from container names in source content
- The display form computed from `EntityId` for status-bar / list rendering

Apply `strip_controls` at construction of every `EntityId` field, at
construction of `display_name`, and at every `JjrError` / `GgrError` variant
carrying an external-derived string. Recurring finding in reviews of this
codebase; rule codified in `CLAUDE.md`.

### gh API rate limits

ggr's single-GraphQL-query-per-commit approach makes the typical PR review cost
one query per commit (5–10 queries per PR). Authenticated rate limit is 5 000 /
hour; remote possibility of hitting it only with extreme review volume.
Mitigation: the cache shoulders most repeated work. Pagination of huge commits
across multiple queries is a contingency, not a v1 requirement.

### Sem-core's Jaccard matching threshold

The absorbed Jaccard matcher links entities at a similarity threshold (30%+ in
the reference implementation). Below threshold an entity that looked "renamed"
appears as "deleted + added". Surface this as a knob later if observed false
matches or splits become noisy.

### Jaccard noise on trivial entities

The Jaccard matcher can produce surprising matches on very short entities —
getters, single-line stubs, repetitive boilerplate. A short function that "looks
like" several other short functions can match anywhere.

Policy: do not attempt cross-file Jaccard matches for entities below a
minimum-token threshold (e.g., 20 tokens). Single-line entities match only by
exact `entity_id`, never by content similarity. Tune the threshold based on
observation.

### `tree-sitter-pgsql` is a community grammar

Quality and dialect coverage are not guaranteed at the sem-core-bundled grammar
level. If we hit Postgres-specific constructs that don't parse, contribute
upstream — keeps the architectural assumption (all extractors are tree-sitter)
intact. Falling back to libpg_query for "some" SQL files would fragment the
extractor layer and is explicitly out of scope.

### Description-row is state, not sentinel

The description row is not an entity. It must not appear in `Vec<EntitySummary>`
as a sentinel variant. Implementations hold the description in a separate field
on the entity-list view state (e.g., `description: Option<DescriptionSummary>`).
Project default — codified in `CLAUDE.md`: `Option<T>` for "no real value yet"
states, never a sentinel variant.

### Partial parses are full failures

When tree-sitter parses a file and produces ERROR nodes (syntax errors the
parser recovered from), we do not extract entities from that file. The file
appears as a fallback row.

A partial entity list would violate the reviewer's mental model that "the list
is the set of things that changed in this file." A partial parse showing some
entities but not others is harder to reason about than a clean fallback row. The
reviewer's interpretation: "the parser couldn't handle this file" → drill via
Enter → review as line diff.

Conservatism here is deliberate.

### Caller-count computation cost in jjr

`called from N places` requires walking the absorbed dependency graph for the
focused entity. The walk is bounded by graph depth (direct callers only, not
transitive) — fast. But the graph must be built, which means parsing every
source file in the local repo, not only the changed files.

For a 1 000-file repo this might exceed the 1-second loading threshold on first
entry. The cache stores graph data alongside the entity list per commit, but the
graph is mostly per-repo-state; rebuilding per commit in a stack is wasteful. v1
keeps per-commit cache for simplicity (correctness over performance) and
surfaces this in Open Questions for a per-repo-snapshot graph cache later.

## MVP Scope

What ships in v1, in implementation order:

1. **Semantic extraction layer.** Absorb sem-core's tree-sitter extractors and
   the `SemanticExtractor` trait into `crates/local-review-core/src/semantic/`.
   Adapt to project lints and error types. Wire content fetching from
   `jj file show` and (for ggr) GraphQL blob queries. Bash, YAML, JSON, TOML
   included from day one (sem-core plugins or thin shims). SQL via bespoke
   `tree-sitter-pgsql` plugin.
2. **`EntityId` and identity discipline.** Structured tuple, JSON serialization,
   ordinal disambiguation, `strip_controls` boundary.
3. **Cache.** Disk-backed JSON cache. Stores core extraction data only;
   schema-versioned. Hash-based invalidation for jjr. No eviction.
4. **`EntitySummary` plumbing.** New types in `local-review-core`, surface-trait
   additions for `fetch_entity_list`, `fetch_entity_diff`,
   `fetch_description_summary`. Surface implementations in both `jjr` and `ggr`.
5. **Entity list screen.** New screen state in `App`. Renders entities per the
   TUI Design layout. Bindings as specified. Description row stitched in as
   separate state, not a sentinel entity.
6. **Entity diff view.** `Screen::EntityDiff`. Focused file diff — pre-scroll +
   entity-range highlight. Header bar changes. Status-bar context (entity name +
   annotation; caller count wired later).
7. **Loading overlay and spinner.** Tiered indicators per Caching & Loading.
   Esc-cancel behavior.
8. **Comment model migration.** Add `entity_id` and `anchor_fingerprint` to
   comment-storage schemas in `jjr` and `ggr`. Old drafts unchanged (fingerprint
   absent → fall back to existing re-anchor). Re-anchor pipeline uses
   fingerprint confidence.
9. **Reviewed-bit migration.** Per-entity bits with content_hash alongside
   per-file bits. Auto-mark on entity diff entry.
10. **Cosmetic filtering.** Default display with `[cosmetic]` tag. `;` toggle.
    Composition with severity filter.
11. **Claude context bundle (jjr only).** Absorbed dependency-graph walk; bundle
    assembly; truncation rule. Replaces raw-hunk prompt. ggr+Claude is deferred
    — see Later Enhancements.
12. **Caller count in status bar (jjr only).** Wires dependency-graph walk into
    entity diff view.
13. **Testing.** Golden corpus per language; unit, property, and integration
    tests per Testing Strategy.

Steps 1–7 deliver entity navigation as a usable feature. Steps 8–10 layer on the
comment and reviewed-bit migrations. Steps 11–12 deliver the jjr Claude payoff.
Step 13 catches regressions across the lot.

## Later Enhancements

Explicitly out of v1.

- **Background prefetch** of the next commit's entities while the reviewer is on
  the current one.
- **Whole-PR refresh** (`Shift-R` or similar) — currently `R` refreshes the
  current commit only.
- **Search within entity list** (`/` to filter by entity name) for 500+ entity
  commits.
- **Directory grouping** when file count is large.
- **Cache pruning command** (`ggr cache prune` / `jjr cache prune`).
- **Per-repo-snapshot graph cache** for caller-count optimization.
- **Caller count in ggr** when local clone is available (opt-in).
- **Jaccard threshold tuning** — surface as a knob if observed false-positive /
  false-negative rates warrant.
- **Cross-file rename detection in non-code languages** — current scope is code
  only; config-file renames are rarer and less load-bearing.
- **ggr + Claude integration.** A different feature than jjr+Claude. The
  reviewer does not own the code in ggr, so "Claude addresses the comment by
  editing" does not apply. The plausible use cases each warrant their own design
  pass:

  | Use case              | What Claude does                                                         | Triggered when                      |
  | --------------------- | ------------------------------------------------------------------------ | ----------------------------------- |
  | Compose review text   | Suggest comment text given the diff + reviewer intent                    | Reviewer is in the composer         |
  | Pre-review pass       | Read the PR and surface "things worth a look"                            | Reviewer opens the PR               |
  | Polish the submission | Check severity calibration, redundancy, missing structure                | Before `S` submits                  |
  | Explain on demand     | Answer "what does this do? what are its callers?"                        | Reviewer presses a key on an entity |
  | Suggest as code       | Convert "this is wrong, here's why" into a GitHub suggested-change block | When composing a comment            |

  Pick a use case before designing the integration. The earlier framing
  ("entity-only context bundle for ggr") was a context-shape question inherited
  from jjr; the actual blocker is _what does Claude do in ggr_, which is a
  feature question.

## Success Criteria

V1 succeeds when:

1. A reviewer opens a typical change / commit in either tool and reaches the
   entity that interests them in two keypresses (`Enter` from list; optionally
   `j`/`k` to position first).
2. Cosmetic-heavy commits (post-`cargo fmt`) can be triaged in under 30 seconds
   via `;` filter.
3. Renamed functions retain reviewed status across a `jj squash` that does not
   modify body content. Sibling untouched functions in the same file retain
   reviewed status when one method is edited.
4. Claude-remediation cycles in jjr show measurably fewer "Claude broke an
   unrelated thing while addressing a comment" incidents than today's raw-hunk
   prompt. Measured by the user's experience; no formal benchmark.
5. Files in unsupported languages or with parse failures appear as fallback
   rows; the reviewer can drill into them and leave comments using the same
   workflow. No code path crashes or silently drops a file.
6. Cache-hit entry to a previously-extracted commit returns under 100 ms.
7. The unit + property + integration test suite passes; the golden per-language
   corpus catches grammar drift on every CI run.

## Open Questions

1. **`signature_key` shape for languages without typed signatures.** Python has
   no parameter types in the signature; identity falls back to ordinal among
   same-name same-scope functions. JavaScript without TypeScript has the same
   issue. Open: is positional-arity (`(2)`, `(3)`) worth including as a partial
   signature, or does ordinal alone suffice? Default: ordinal only; surface as a
   tuning concern if observed collisions cause real problems.

2. **Filter persistence.** Should the cosmetic toggle persist across sessions
   like severity? Default: persist; revisit if confusing.

3. **Truncation telemetry to Claude.** Bundle truncation tells Claude what's
   omitted. Open: also log it for the user? Default: no; log volume noise
   outweighs signal.

4. **Reviewed-bit garbage collection.** Per-entity bits accumulate indefinitely.
   Old `entity_id`s for entities no longer present anywhere are harmless but
   consume disk. Default: no reaping (matches cache eviction posture).

5. **AnchorFingerprint scoring threshold.** The spec uses ≥3 as the minimum
   match score. Open: should this be tunable based on observed
   stale-vs-mis-anchored rates? Default: fixed; revisit if observed incorrect.
