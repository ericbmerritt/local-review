## Phase 1: Semantic extraction layer

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-06-10 | 2026-06-10 |

Absorb sem-core's tree-sitter entity extraction into
crates/local-review-core/src/semantic/. License attribution (MIT/Apache-2.0)
preserved in crate-level docs. Once absorbed, we own the code — no upstream
sync, modify freely.

**Upstream source**

Upstream: https://github.com/Ataraxy-Labs/sem. Pin to a specific commit when
copying — vendor as a snapshot, not as a tracked dependency. Record the commit
hash in a NOTICE file or crate-level comment.

What to absorb (paths relative to the sem-core repo root, under its
`crates/sem-core/src/`):

- `parser/plugin.rs` — extractor trait (rename to SemanticExtractor)
- `parser/plugins/code/` — language-specific tree-sitter implementations
- `parser/differ.rs` — entity extraction + change classification
- `parser/registry.rs` — language detection and extractor dispatch
- `model/entity.rs` — SemanticEntity
- `model/change.rs` — SemanticChange, ChangeType
- `model/identity.rs` — Jaccard similarity matcher

Drop entirely: sem-core's git bridge, all formatters, CLI code, impact/blame/log
commands.

**Languages (v1, 13 total)**

The exact tree-sitter grammar crates to depend on, pinned to specific versions
in workspace Cargo.toml. Use `=` version pinning (e.g.,
`tree-sitter-rust = "=0.21.2"`), not `^` or `~`, per spec's grammar-pinning
gotcha.

- tree-sitter (the runtime itself)
- tree-sitter-rust
- tree-sitter-typescript (covers .ts and .tsx)
- tree-sitter-javascript (covers .js, .jsx, .mjs)
- tree-sitter-python
- tree-sitter-go
- tree-sitter-java
- tree-sitter-scala
- tree-sitter-kotlin
- tree-sitter-bash
- tree-sitter-yaml
- tree-sitter-json
- tree-sitter-toml
- tree-sitter-pgsql (for PostgreSQL — community grammar, see SQL section)

Resolve exact versions at implementation time using `cargo search` for the
latest stable. Each grammar gets its own feature flag (`lang-rust`,
`lang-typescript`, etc.); default feature set enables all 13.

**SQL is bespoke**

sem-core does not ship a SQL plugin. Write one against tree-sitter-pgsql
(community Postgres grammar). Extract:

- CREATE TABLE / ALTER TABLE → table entity
- CREATE INDEX → index entity
- CREATE OR REPLACE FUNCTION → function entity (the high-value case for plpgsql
  review)
- CREATE VIEW / CREATE MATERIALIZED VIEW → view entity
- CREATE TYPE → type entity
- CREATE TRIGGER → trigger entity
- CREATE POLICY → RLS policy entity
- CREATE SCHEMA / CREATE EXTENSION → schema / extension entities

Schema-qualify entity identity in the EntityCoreData scope information:
`public.recompute_balance()` and `private.recompute_balance()` are distinct
entities. `ALTER TABLE foo ADD COLUMN x` is annotated on the table entity as
`ALTER · ADD COLUMN x` rather than generic `body`. DO blocks (`DO $$ ... $$`)
appear as anonymous block rows; their PL/pgSQL contents are opaque at this
layer. Transaction wrappers (BEGIN/COMMIT) are structural noise; contents
extracted as if at top level.

**Why tree-sitter for SQL, not libpg_query**

libpg_query (the actual PostgreSQL parser) is strict — first syntax error aborts
the parse with no partial AST. Tree-sitter handles partial / malformed / WIP /
extension content gracefully via incremental parsing and ERROR nodes. For a
review tool where files may be WIP, may use Postgres features newer than the
libpg_query version, or may use extension-specific syntax (TimescaleDB,
pgvector, PostGIS), tree-sitter's resilience matters more than libpg_query's
fidelity. If tree-sitter-pgsql misses constructs we care about, contribute
upstream rather than swapping parsers — keeps the architectural assumption that
all extractors are tree-sitter.

**Container Rule (load-bearing for the extractor)**

Containers (class, struct, trait, impl, module, namespace) appear in extracted
EntityCoreData output ONLY when the container itself has changed (declaration,
signature, generic params, visibility, base class, trait bounds). When only
contents change, contained entities surface as themselves with the container
shown as scope chain context. When the container declaration changes AND
contents change, both surface. See spec section 'The Container Rule' for
per-language examples.

When entities move between containers (method moves from class A to class B),
neither A nor B appears in the entity list unless its own declaration also
changed — the reviewer sees one moved row with `moved from A.method`. This is an
explicit tradeoff in service of keeping the list focused on the most granular
change.

**Parse-with-ERROR-nodes is full failure**

A file whose tree-sitter parse contains ERROR nodes is treated as fully failed
extraction — surface no entities, mark the file for fallback row rendering
downstream. A partial entity list violates the reviewer's mental model that 'the
list is the set of things that changed in this file.'

**Jaccard noise mitigation**

The absorbed matcher links entities across boundaries at a 30%+ similarity
threshold. Trivial entities (under 20 tokens — getters, single-line stubs,
repetitive boilerplate) must NOT cross-file match. Enforce in the absorbed
matcher: under-threshold entities match only by exact identity, never by content
similarity.

**Project lints (per CLAUDE.md design defaults)**

Absorbed code adapts to project standards. No unwrap/expect/as/unsafe. Errors
flow through Result<T, \_>. Strip control characters at every external-string
boundary using existing strip_controls / strip_controls_preserve_newlines
helpers from local-review-core::util. Entity names from tree-sitter, file paths
from inputs, scope-chain elements — all pass through strip_controls before
flowing into EntityCoreData.

**Output types defined in this phase**

Define EntityCoreData (the public output interface from this module) inline:

```rust
pub struct EntityCoreData {
    pub id: PlaceholderEntityId,         // Phase 2 swaps this for EntityId without changing field shape
    pub kind: EntityKind,                // Function, Method, Class, Module, ConfigProperty, Index, Trigger, ...
    pub change: ChangeType,              // Added, Modified, Deleted, Moved
    pub annotation: ChangeAnnotation,    // sig changed / body / sig+body / value diff / 'ALTER · ADD COLUMN x' / ...
    pub line_range: (u32, u32),          // start/end line in file after state
    pub source_file: Option<PathBuf>,    // populated for ChangeType::Moved
    pub target_line: Option<u32>,
    pub structural_change: bool,         // false = cosmetic (parser heuristic, not classification of intent)
    pub content_hash: u64,
}
```

PlaceholderEntityId is a newtype `pub struct PlaceholderEntityId(pub String)`
holding a stable identifier derived from sem-core's existing identity logic
(file path + scope path + entity name). Phase 2 mechanically swaps this for the
structured EntityId without changing any other EntityCoreData field shape. The
swap is type-level only — downstream code consumes EntityCoreData; it does not
care whether `id` is Placeholder or structured.

**Golden corpus**

5–10 small source files per language with expected entity extractions checked
into
`crates/local-review-core/tests/semantic-golden/<lang>/<NN>-<descriptive-name>.<ext>`
paired with `<NN>-<descriptive-name>.expected.json`. Match semantics:
field-exact JSON equality of the extractor output against expected.json. Tests
fail loudly on extraction changes — grammar version bumps require explicit
golden regeneration. This is the principal defense against tree-sitter grammar
drift.

Fixture template (Rust example):

- `01-impl-block.rs` — small Rust file with one struct and one impl block, one
  method changed
- `01-impl-block.expected.json` — JSON array of EntityCoreData entries with the
  expected change, scope, annotation, content_hash, etc.

Write at least one fixture per language for v1; expand the corpus as bugs
surface.

**Out of scope for this phase**

No cache (Phase 2). No content fetching from jj/gh (Phase 2). No UI integration
(Phase 3). No dependency graph (Phase 5). Pure extraction layer with golden
tests.

**Reference**

specs/semantic-entity-navigation.md sections: 'Semantic Extraction Layer' (full
absorption details), 'The Container Rule' (principle), 'Cosmetic Filtering'
(parser-heuristic framing), 'Gotchas and Constraints' (tree-sitter handles
partials, grammar pinning, partial parses are full failures, Jaccard noise on
trivial entities, tree-sitter-pgsql is community grammar).

#### Delivers

- crates/local-review-core/src/semantic/ module with absorbed sem-core
- 13 tree-sitter grammar deps in workspace Cargo.toml, pinned to exact versions,
- Bespoke tree-sitter-pgsql plugin for SQL with Postgres entity rules
- Golden-test corpus per language under
- License attribution (MIT/Apache-2.0) for absorbed sem-core code in crate-level
- EntityCoreData public type as the layer's output interface, inline-defined in
- PlaceholderEntityId(String) newtype as the Phase 1 identity slot, replaced by

#### Done When

- All 13 languages extract entities from corpus fixtures; comparison is
- SQL plugin extracts tables, indexes, functions, views (and materialized
- ALTER TABLE foo ADD COLUMN x is annotated as 'ALTER · ADD COLUMN x' on the
- DO blocks appear as anonymous block entities with opaque body content
- Jaccard matcher links renamed entities across before/after content above 30%
- Entities below 20 tokens do not cross-file match (Jaccard noise mitigation)
- strip_controls applied to all string fields entering EntityCoreData (entity
- Files producing tree-sitter parses with ERROR nodes treated as full failure
- Container Rule holds: containers appear in output only when container
- just validate passes; clippy lints pass without unwrap/expect/as violations

#### Depends On

- (none)

## Phase 2: Identity, cache, surface plumbing

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-06-10 | 2026-06-10 |

Wire the extraction layer from Phase 1 through to the ReviewSurface trait. Build
the cache. Connect content fetching to both tools.

**EntityId — the structured identity tuple**

Phase 2 replaces Phase 1's PlaceholderEntityId across the codebase with:

```rust
pub struct EntityId {
    pub file_path: PathBuf,            // repo-relative, UTF-8, strip_controls applied
    pub scope_chain: Vec<String>,      // each segment UTF-8, strip_controls applied
    pub signature_key: Option<String>, // language-specific param signature
    pub ordinal: u32,                  // disambiguator for duplicate (file, scope, sig)
}
```

Serialization is JSON, NOT string concatenation. String concatenation with `::`
or `/` would collide with file paths (which may contain `::` in module syntax in
some languages, and Windows paths contain `:`). JSON sidesteps that entirely.
Cache files and comment storage use JSON serialization.

`signature_key` is the language-specific parameter signature in normalized form:

- Java/Kotlin: `(int)`, `(String)` — distinguishes overloads
- Rust: `(&self, &str) -> Result<Token>` — strips body, keeps params and return
- TypeScript: includes generic parameters and types
- Python: None (no typed signatures; falls back to ordinal for nested-function
  disambiguation)
- SQL: schema-qualified, e.g., `public.recompute_balance(integer)`
- Container entities (classes, modules, traits, impl blocks): None — containers
  identified by scope and ordinal alone
- Config properties (YAML, JSON, TOML, Markdown): None — property name is the
  identity

`ordinal` is the disambiguator used when
`(file_path, scope_chain, signature_key)` is not unique within a file.
Zero-based index in start_byte source order among entities sharing the same
first three fields. Common case: ordinal 0. Multiple Rust `impl Foo` blocks for
the same type produce ordinals 0, 1, 2 in source order. Java overload pairs
distinguished by `signature_key` keep ordinal 0.

Why ordinal not raw start_byte: when entities are inserted above an existing
entity, start_byte shifts but the entity's ordinal among its duplicates stays
the same (it's still the 'second impl Foo' in the file). Ordinal is more stable
than start_byte across edits.

The Phase 1 → Phase 2 swap: every EntityCoreData currently carries a
PlaceholderEntityId; Phase 2 swaps the `id` field type to EntityId and updates
the extractor to populate the structured fields. EntityCoreData's other field
shapes do not change. Downstream code that consumes EntityCoreData adapts only
at the `id` access point.

**Strip-controls discipline**

Every string field on EntityId construction passes through strip_controls
(project default per CLAUDE.md, applied at the new external-input boundaries):

- file_path (from sources like gh api responses, jj file show output)
- scope_chain elements (from tree-sitter extraction)
- signature_key string (from tree-sitter extraction)

UTF-8 preserved; only control characters stripped. No ASCII restriction.

**Cache**

- **ggr location:**
  `$XDG_DATA_HOME/ggr/cache/entities/<owner>/<repo>/<pr>/<commit_sha>.json`.
  Fallback when `XDG_DATA_HOME` is unset:
  `~/.local/share/ggr/cache/entities/<owner>/<repo>/<pr>/<commit_sha>.json`.
  (Same XDG-with-fallback pattern ggr already uses for its draft storage — reuse
  the existing helper, do not reimplement.)
- **jjr location:**
  `<repo_root>/.jj-review/entities/<change_id>-<content_hash_hex>.json` where
  `content_hash_hex` is `format!("{:016x}", content_hash_u64)` (zero-padded
  lowercase hex of the u64 content_hash).

Cache entry type:

```rust
pub struct CacheEntry {
    pub schema_version: u32,
    pub entities: Vec<EntityCoreData>,
    pub file_failures: Vec<FailedFile>,
    pub graph: Option<GraphData>,      // forward-compat slot; populated in Phase 5 for jjr
}
```

The `graph: Option<GraphData>` field is reserved in this phase as a
forward-compatibility slot. `GraphData` is defined as an empty/unit type for
Phase 2 purposes; Phase 5 fills in the real shape and starts populating it for
jjr. Phase 2 ships with `graph: None` everywhere.

**Invalidation**

- **ggr:** key by commit SHA. PR commits are immutable. Cache entries outside
  the current PR's commit list are unreachable but not actively pruned. No
  content_hash needed for ggr because commit SHA already pins content.
- **jjr:** key by `(change_id, content_hash)`. jj changes are mutable — the same
  change ID can have different content after `jj squash` or `jj edit`. The
  content_hash component is non-negotiable for jjr correctness; without it, the
  cache would return stale entity lists after re-edits.

No eviction in v1. Cache grows monotonically; small entries (JSON, kilobytes per
commit). Manual `rm -rf` of the cache directory recovers disk if needed.

**Content fetching**

jjr: for each file in the Diff, call `jj file show -r <parent_rev> <path>` for
before-content and `jj file show -r <rev> <path>` for after-content. Local
subprocess, fast (~10-50ms per call). Synchronous at extraction time.
Added/deleted files have one side empty. Subprocess error stderr passes through
strip_controls before entering error types.

ggr: SINGLE GraphQL query per commit returning HEAD and BASE blob OIDs and
content text for ALL changed files in that commit. Use `gh api graphql` via the
existing gh CLI wrapper in `crates/ggr/src/gh.rs`. The REST contents API is NOT
used — it base64-encodes responses, has stricter size limits, and requires
per-file HTTP calls. The blob/GraphQL approach makes typical PR review one query
per commit (5-10 per PR), well within the 5,000/hour authenticated rate limit.

GraphQL response size limit (v1 failure behavior): if the GraphQL endpoint
rejects the query due to response size, the surface returns an error that the
entity-list-construction layer translates into 'extraction failed for this
commit'. Every file in that commit renders as a fallback row in the entity list
(the existing fallback-row code path). The reviewer can still drill into files
via Enter. Pagination across multiple queries for very large commits is a Later
Enhancement — not required for v1.

Per-file content-fetch failure (one file errors but others succeed) causes that
file to fall back to the 'no entities' row. One file's failure does not abort
the rest of the extraction.

**ReviewSurface trait extensions**

The trait lives in `crates/local-review-core/src/tui.rs`. Existing
`fetch_views(entry_idx: usize) -> Result<Vec<DiffView>, Self::Error>` is
PRESERVED UNCHANGED for the file diff escape hatch path. Phase 3's
`Screen::FileDiff` will continue to call it.

Three new methods are added:

```rust
fn fetch_entity_list(&self, entry_idx: usize) -> Result<Vec<EntitySummary>, Self::Error>;
fn fetch_entity_diff(&self, entry_idx: usize, entity_idx: usize) -> Result<(DiffView, LineRange), Self::Error>;
fn fetch_description_summary(&self, entry_idx: usize) -> Result<DescriptionSummary, Self::Error>;
```

fetch_entity_list returns the entity list view data. Empty Vec means 'no
entities for this entry' — downstream renders file fallback rows.

fetch_entity_diff returns the file's complete DiffView (existing type) plus the
entity's line range for the entity diff view to use in Phase 3 for pre-scroll
and highlighting. The DiffView is the file's FULL diff — entity-scoping is
visual (pre-scroll + highlight), not structural slice. Line numbers in the
DiffView are real file line numbers; GitHub `(file, line)` line anchors and
Claude line edits remain correct.

fetch_description_summary returns the pinned description row's content. Separate
from entity list because the description is not an entity. The description-row
state on the App side is `Option<DescriptionSummary>` field (per CLAUDE.md
sentinel-state default: never a sentinel EntitySummary in the Vec).

**New types defined in this phase**

```rust
pub type LineRange = (u32, u32);     // start_line, end_line inclusive (1-based)

pub struct DescriptionSummary {
    pub subject: String,             // commit subject (ggr) or change description first line (jjr)
    pub comment_count: usize,        // change-scoped comments only
}

pub struct EntitySummary {
    pub id: EntityId,
    pub display_name: String,        // language-native scope path; computed at render time
    pub kind: EntityKind,
    pub change: ChangeType,
    pub annotation: ChangeAnnotation,
    pub file_path: PathBuf,
    pub source_file: Option<PathBuf>, // for ChangeType::Moved
    pub target_line: Option<u32>,
    pub line_range: LineRange,
    pub structural_change: bool,     // false = cosmetic (parser heuristic)
    pub content_hash: u64,
    pub comment_count: usize,        // computed at render time
}
```

`display_name` and `comment_count` are NOT stored in the cache — they are
computed at render time when the surface builds EntitySummary from cached
EntityCoreData + comment store lookups.

No UI changes in this phase. Phase 3 consumes the surface methods.

**Reference**

specs/semantic-entity-navigation.md sections: 'Entity Model' (entity_id
contract, EntitySummary shape), 'Caching & Loading' (cache location,
invalidation, schema versioning, no eviction), 'Semantic Extraction Layer'
(content fetching paths for jjr and ggr), 'Implementation Impact' (ReviewSurface
trait additions).

#### Delivers

- EntityId structured tuple type ({file_path, scope_chain, signature_key,
- Disk cache (CacheEntry with schema_version: u32) at
- Content fetching: jj file show for jjr; single gh api graphql query per commit
- ReviewSurface trait extensions in crates/local-review-core/src/tui.rs:
- Existing fetch_views preserved unchanged for the file diff escape hatch path
- Surface implementations in jjr and ggr wiring extractor → cache → response
- EntitySummary type (render-time view wrapping EntityCoreData + UI-computed

#### Done When

- EntityId JSON roundtrip preserves the structured tuple shape (file_path,
- Two distinct entities with same (file_path, scope_chain, signature_key) get
- Inserting new entities above an existing duplicate-signature entity does not
- Cache round-trips through disk: write → read → equal EntityCoreData
- CacheEntry includes graph: Option<GraphData> field as a forward-compatibility
- Schema version mismatch on read invalidates the entry and triggers
- jjr fetch_entity_list returns entities for a change ID; first call extracts
- ggr fetches HEAD and BASE blob content for all changed files of a commit via
- When GraphQL response exceeds size limits, the tool surfaces the error and
- Per-file content-fetch failure marks that file for fallback row downstream;
- jjr cache invalidates after jj squash modifies entity content (content_hash
- EntitySummary computed at render time from EntityCoreData; cache stores
- Cache filename hash formatting uses {:016x} (zero-padded lowercase hex)

#### Depends On

- semantic-extraction-layer

## Phase 3: Entity list and entity diff with loading

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-06-11 | 2026-06-11 |

The first user-visible phase. Phase 2 wired the surface; this phase delivers
entity navigation as a usable feature.

**Screen states**

The Screen enum lives in `crates/local-review-core/src/tui/app.rs`.

- Screen::Main was the file diff view. Becomes the entity list view. Layout at
  80x24: Stack bar (3 rows) + description row (1 row) + divider (1 row) + entity
  list body (18 rows) + footer (1 row).
- Screen::EntityDiff { entity_idx } — new state. Renders the file's full diff
  via existing rendering machinery. Pre-scrolls to the entity's anchor line
  (start_line of LineRange) on entry. Visually highlights lines within the
  entity's line range — implementation choice between subtle background tint,
  sidebar glyph, or both. Header bar:
  `authenticate() · auth.rs · 3 of 8 entities`.
- Screen::FileDiff { file_idx } — renamed from the previous main state. Reached
  only via F from the entity list. Header bar: `auth.rs · 3 of 12 files`. No
  entity highlighting.

**App state migration and index glossary**

App now juggles four distinct indices. Keeping them straight matters; encode the
distinction in field names and don't conflate.

- **stack_entry_idx** (existing): which entry in the stack/PR (e.g., which
  commit in ggr, which change in jjr). Driven by `n`/`p`.
- **entity_index** (new, primary): cursor within the entity list. Driven by
  `j`/`k` on Screen::Main.
- **file_index** (existing, secondary): index into the entry's file list. Used
  only in Screen::FileDiff. Driven by Tab on Screen::FileDiff (existing behavior
  preserved).
- **view_idx** (existing internal): index into `fetch_views` Vec<DiffView>.
  Still used by Screen::FileDiff via fetch_views.

`app.entity_index` is the primary cursor for the entity list. The
`file_index == 0 → description` special case throughout App is REMOVED —
description is rendered as the pinned first row of the entity list and lives in
separate state (Option<DescriptionSummary>).

**Entity diff view as focused full-file diff (load-bearing decision)**

The diff is the file's COMPLETE diff with REAL line numbers. Pre-scroll and
highlight provide visual focus, but line numbers are unchanged. This means
GitHub (file, line) line anchors and Claude line edits work without translation.
Do NOT synthesize a clipped diff — line anchors would diverge from GitHub's
view, breaking comment posting. The reviewer who scrolls outside the highlighted
range sees the rest of the file in context.

**Entity list row format**

- 2-char pad + 2-char sigil + space + entity name (up to 28 chars, truncate with
  …) + 2-char gap + file path (up to 15 chars, truncate leading dirs to keep
  filename — src/auth/login.ts → login.ts) + annotation (~18-20 chars; truncate
  moved-from source path first when over budget) + comment dot ● right-aligned
  with severity color

**Sigils and colors**

- ≡ description row (default color)
- Δ modified (yellow)
- ⊕ added (green)
- ⊖ deleted (red)
- ≈ moved (gray)
- ○ file fallback row (gray)

Cosmetic-entry modifier (composes on top of the table, applies in this phase):

- Entity name text dimmed to DarkGray
- Annotation column suffixed with ` [cosmetic]`, also dimmed
- Sigil glyph and its change-kind color (yellow / green / red / gray) remain
  unchanged — the change-kind signal is preserved at a glance; only the textual
  weight drops

The ≡ sigil for description was chosen because it visually reads as horizontal
lines (document body), distinct from the change-kind sigils which all reference
geometry of change.

**Sort order**

Entities sort by file path first, then by line position within file. Entities
from the same file end up adjacent; the repeating filename in the file column
visually groups them. NO explicit file headers — the file column on each row
carries the grouping. This is opinionated: changes in the same file usually
relate, and the diff is a per-file artifact; the reviewer naturally reads in
document order.

**Extraction worker thread + cancellation**

Extraction is potentially slow (jjr: subprocess per file; ggr: GraphQL
roundtrip). The TUI cannot block its render loop on it. This phase introduces a
small thread + channel design.

When the user navigates into a change/commit that requires extraction (cache
miss), spawn a `std::thread` that calls fetch_entity_list. The thread
communicates back via an `std::sync::mpsc::channel`:

```rust
enum ExtractionEvent {
    /// Counter update for the loading overlay.
    Progress {
        files_done: usize,
        files_total: usize,
        files_failed: usize,
    },
    /// One file's extraction finished — carries the entity summaries
    /// (possibly empty for a parse failure / unsupported language).
    /// Emitted before the matching Progress counter increment so the
    /// UI accumulates rows as they arrive.
    FileExtracted {
        file_path: PathBuf,
        entities: Vec<EntitySummary>,
    },
    /// All files processed without cancellation.
    Complete,
    /// Cancellation flag was set; remaining files unprocessed.
    Cancelled,
    /// Fatal error (e.g., commit-list fetch failed in ggr).
    Error(String),
}
```

The TUI main loop polls the channel on each render tick (non-blocking try_recv).
`FileExtracted` events append rows to the in-progress entity list; `Progress`
events update the loading-overlay counters; `Complete` transitions to the
rendered entity list; `Cancelled` keeps the partial entity list (from any
`FileExtracted` events already received) and renders cancelled / unstarted files
as fallback rows; `Error` transitions with all files as fallback rows.

Cancellation: the spawned thread holds an `Arc<AtomicBool>` cancellation flag.
The thread checks the flag between processing each file and exits early if set.
The Esc handler on the loading overlay calls
`flag.store(true, Ordering::Relaxed)` and waits (briefly, on the channel) for
the thread to acknowledge via Cancelled or Complete.

This is a contained threading addition — one worker thread per extraction
operation, no broader async runtime introduced. The rest of the TUI remains
synchronous.

**Tiered loading indicators**

- 0-300ms: nothing
- 300ms-1s: Braille spinner (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) in status bar; ASCII fallback (| / - \)
  selected by Unicode-support signal — NOT by NO_COLOR (which only disables
  color). Use an explicit env var (e.g., JJR_ASCII_SPINNER / GGR_ASCII_SPINNER)
  or terminal-capability detection where reliably available
- 1s+: modal overlay (centered, bordered, single-line): ⠋ Extracting entities ·
  23 / 50 files · 3 failed Esc to cancel

Spinner animates at 100ms/frame. Overlay shows progress + failed counts as
Progress events arrive. Terse microcopy — no 'Loading…', no exclamations, no
'Please wait.'

**Cancellation UX**

Esc during the 1s+ overlay: set the cancellation flag, wait briefly for the
thread to ack. Already-completed files retain their data and show as entity
rows. Cancelled files become fallback rows. Reviewer is returned to (or remains
on) the entity list with the partial result. R re-triggers full extraction.

**Bindings on the entity list screen**

j / ↓ move down k / ↑ move up Enter open entity diff (or description screen if
on description row) F open file list (escape hatch) Tab open next entity's diff
Shift-Tab open previous entity's diff c open comment composer (scope follows
cursor) 1 / 2 / 3 severity filter (existing) ; cosmetic visibility toggle —
wired in Phase 4 (no-op stub in Phase 3; cosmetic entries always visible by
default) n / p stack navigation: next / previous commit / change (existing) R
refresh current commit (re-fetch, re-extract, clear cache); subsumes any
existing R binding ? help q quit

f (lowercase): no-op on the entity list screen (file picker is F now). No error,
no beep, no status message.

**Bindings on the entity diff view** (preserves existing diff bindings, changes
Esc destination)

- Esc/q returns to entity list (NOT to file list)
- Tab advances to next entity's diff; Shift-Tab retreats — both in list order
- F opens file list (escape hatch)
- Existing c, e, d, r, severity filters, scroll keys work unchanged

**Graceful degradation**

If fetch_entity_list returns empty Vec (extraction failed for the whole entry,
or the surface chose not to extract), surface every changed file as a fallback
row (○ src/foo.rs). Selecting a fallback row drills directly to that file's diff
via the existing fetch_views path. Same path, same UI — no separate 'extraction
failed' UI mode. The tool never fails to show a change; it may fail to annotate
it semantically.

**Reference**

specs/semantic-entity-navigation.md sections: 'TUI Design' (full layout, sigil
table, bindings), 'Navigation Model' (hierarchy and bindings), 'Caching &
Loading' (loading indicators, overlay format, cancellation, single block point),
'Implementation Impact' (App state changes, Screen states, ReviewSurface
callers), 'Cosmetic Filtering' (default-view visual treatment that ships in this
phase; the toggle ships in Phase 4).

#### Delivers

- Screen::Main renders the entity list view (replaces previous file diff entry
- Screen::EntityDiff state — focused full-file diff with pre-scroll + entity
- Screen::FileDiff state — relabeled escape hatch behind F
- Description row as separate Option<DescriptionSummary> state on App, pinned at
- Background extraction worker: std::thread + mpsc::channel for progress
- Tiered loading indicators (silent <300ms, status-bar spinner 300ms-1s, modal
- Cosmetic visual treatment: dimmed foreground + [cosmetic] suffix in the
- Bindings per spec: j/k, Enter, F, Tab/Shift-Tab, c, n/p, R, ?, q. Lowercase f

#### Done When

- Opening a change in jjr or a commit in ggr lands on the entity list screen at
- Entity rows render with correct sigils (Δ/⊕/⊖/≈/○), file path column,
- Cosmetic entries (structural_change=false) render with DarkGray foreground and
- Enter on an entity opens Screen::EntityDiff: file diff renders, pre-scrolled
- Esc/q from entity diff returns to entity list
- Tab from entity diff opens next entity's diff; Shift-Tab previous; both in
- F opens the existing file picker; selecting a file opens Screen::FileDiff
- Description row shows commit subject; Enter on description row opens the full
- Loading overlay appears with progress counts (files done / total / failed)
- Esc during overlay cancels the extraction worker thread (cancellation flag
- R subsumes any existing refresh binding — invalidates current commit cache,
- Lowercase f on the entity list screen is a no-op (no error, no beep)
- Files with extraction failures (binary, unsupported language, content-fetch
- Long entity names truncate with …; annotation column truncates moved-from
- Stack bar continues to show commit/change identity above the entity list;
- Manual smoke test: open a real change in jjr and a real PR commit in ggr,

#### Depends On

- identity-cache-surface-plumbing

## Phase 4: Comment and reviewed-bit migration

| Status         | Started | Completed |
| -------------- | ------- | --------- |
| ⬜ not-started |         |           |

Migrate comment storage to be entity-aware. Migrate reviewed-bit storage to
per-entity with content hashing. Wire the cosmetic visibility toggle. Phase 3
already shipped the default cosmetic visual treatment (dimmed + suffix); this
phase adds the toggle that hides cosmetic entries.

Bundled here because the storage migrations share a principle
(`storage serves posting; display serves understanding`) and because they're
roughly the same shape of work (extend an existing schema with new optional
fields, build re-anchor / re-key logic, preserve backward compatibility). The
cosmetic toggle is small and rides along.

**Comment schema extension**

jjr's comments live in `crates/jjr/src/comment.rs`. ggr's drafts live in
`crates/ggr/src/draft.rs`. Both files gain two new fields on the existing
comment / draft struct:

```rust
entity_id: Option<EntityId>,                     // None when line is outside any entity, or for old drafts
anchor_fingerprint: Option<AnchorFingerprint>,   // None for old drafts; Some for new drafts
```

Both fields use `Option<T>` to represent "not present" — this is the codified
project default (CLAUDE.md design defaults: `Option<T>` for absent state, never
a typed sentinel). Old drafts predating this phase deserialize with both fields
`None` and use the legacy re-anchor path until re-saved, at which point both
fields populate.

```rust
pub struct AnchorFingerprint {
    pub line_hash: u64,    // whitespace-normalized hash of the anchored line content
    pub before_hash: u64,  // line above (0 if at top of file)
    pub after_hash: u64,   // line below (0 if at bottom of file)
}
```

The line remains the source of truth (GitHub posting and Claude line edits both
require (file, line)). entity_id is a re-anchor hint and a display key.
anchor_fingerprint protects against silent mis-anchoring when an entity has
repeated lines like multiple return Err(e) statements.

Backward compatibility: old drafts predating this phase have no entity_id and no
anchor_fingerprint. They use the legacy re-anchor path until the user re-saves
them, at which point the new fields are populated.

**Re-anchor pipeline**

Replaces the existing file-wide fuzzy line matching in jjr and ggr (the
Phase-4-era re-anchoring already shipped):

1. **Locate target entity.** If stored entity_id is present and the extractor
   finds a matching entity in the new revision (exact match by entity_id, OR
   Jaccard rename match above threshold), use that entity's new line range for
   step 2. If no entity match (entity gone with no Jaccard link), skip directly
   to step 3.

2. **Find line within entity by fingerprint.** Scan the matched entity's line
   range. For each candidate line, compute its AnchorFingerprint and a match
   score against the stored fingerprint:

- 3 points: line_hash matches
- 1 point: before_hash matches
- 1 point: after_hash matches
- Max score: 5 Accept highest-scoring candidate if its score is ≥3 (line_hash
  match is mandatory). On tie, prefer the line at the same relative offset
  within the entity. If no candidate scores ≥3, the comment is stale. **Step 2
  does NOT fall through to step 3** — an entity match plus a fingerprint miss is
  a strong signal that the line itself was deleted or radically changed, even
  though the entity persists. Falling through to file-wide search risks finding
  a coincidental match elsewhere.

3. **Fallback file-wide search.** Only reached when no entity match in step 1.
   Same fingerprint scoring across the whole file. Same threshold: ≥3. Below
   threshold → stale.

The output of re-anchoring is always a new (file, line, entity_id,
anchor_fingerprint). The stored entity_id is OVERWRITTEN with whichever entity
now contains the matched line. The stored anchor_fingerprint is RECOMPUTED from
the new line and its new neighbors, so subsequent re-anchors are scored against
fresh context — not against drift-accumulated history.

**ReviewedBit storage**

Extended schema in both tools' reviewed-state files (jjr:
`crates/jjr/src/reviewed.rs`, ggr: equivalent; both presently store
Set<(CommitId, PathBuf)> as file-level bits):

```rust
pub struct ReviewedBit {
    pub commit_id: CommitId,
    pub entity_id: EntityId,
    pub content_hash: u64,    // entity's content_hash at the time of marking
}
```

On-disk format: extend the existing reviewed.json schema (jjr) and ggr's
equivalent. Use a `schema_version: u32` field at the top level of the on-disk
JSON. The file structure becomes:

```json
{
  "schema_version": <N>,
  "file_bits": [/* existing per-file entries */],
  "entity_bits": [/* new ReviewedBit entries */]
}
```

Reading older files (no schema_version field, or schema_version=0): treat as
legacy file-bits-only and migrate forward on first write. Reading
schema_version > the code's known version: refuse to load (don't truncate the
file) and surface an error.

File-level bits remain valid for fallback rows and the file diff escape hatch.
New entity bits accumulate. A file with old per-file bits and new per-entity
bits: per-entity bits drive the entity list display; the per-file bit drives the
file diff view independently.

**Trigger**

Entering an entity's diff auto-marks it reviewed at its current content_hash
(matches existing per-file auto-mark behavior). Tab-cycling through entities
marks each as the reviewer lands on it. Fallback rows mark via the existing
per-file path: entering the file diff view marks the file.

**Visual**

✓ next to reviewed entity rows in the list. Treatment: glyph adjacent to (not
replacing) the change-kind sigil. Fallback rows use the existing ✓. A commit is
fully reviewed when every entity row and every fallback row is marked. Stack
overview aggregation uses the existing per-commit rollup, with entity rows
feeding the rollup instead of file rows.

**Jaccard-matched reviewed-bit carryforward**

When the Jaccard matcher links entity_id_old to entity_id_new across a re-edit
(jjr) or force-push (ggr), check old bit's content_hash against new entity's
content_hash:

- Match (content unchanged, only identity moved) → carry forward under new
  entity_id (write a new ReviewedBit with the new identity)
- Mismatch (content materially changed) → don't carry; reviewer must re-review

**Cosmetic visibility toggle**

Phase 3 already ships the default visual treatment (dimmed + [cosmetic] suffix).
This phase adds the binary visibility toggle:

- `;` key on the entity list screen toggles cosmetic visibility
- Default: shown (with demoted styling)
- Toggled off: cosmetic entries hidden entirely from the rendered list
- Footer hint reflects current state: append `[cosmetic: shown]` or
  `[cosmetic: hidden]` to the existing footer text alongside any active severity
  filter badge

Persist the toggle state per spec's Open Question 2 default: persist across
sessions (alongside severity filter state if that exists; otherwise use the same
persistence mechanism as the severity filter — store in the existing per-tool
state file).

**Composition with severity filter**

The cosmetic toggle is orthogonal AND to the existing severity filters (1/2/3).
Concrete example: severity=required filter active AND ; toggled to hidden shows
only entities where severity=required AND structural_change=true (real
required-severity items, no cosmetic noise).

**Testing**

Property-level re-anchoring tests per spec's Testing Strategy. Generate edit
scripts on a base file fixture and assert:

- Pure body edits preserve comment line anchoring within entity
- Renames matched by Jaccard carry comments to new entity_id
- Renames below Jaccard threshold (or entities removed without rename) cause
  file-wide fallback search
- Step 2 does not fall through to step 3 — comment with matched entity but no
  fingerprint match marks stale, even if file-wide search would find a
  coincidental match
- Repeated-line content (multiple return Err(e)) does not cause silent migration
- Confidence threshold: scores <3 mark stale rather than picking weak
  best-candidate
- Fingerprint refresh: a comment that re-anchors successfully stores a new
  fingerprint computed from the new neighbors

**Reference**

specs/semantic-entity-navigation.md sections: 'Comment Model' (schema, re-anchor
pipeline, posting to GitHub side/position mapping), 'Reviewed-Bit Model'
(storage, trigger, re-anchoring with Jaccard), 'Cosmetic Filtering' (toggle and
composition with severity), 'Testing Strategy' (property-level re-anchoring
tests).

#### Delivers

- entity_id: Option<EntityId> and anchor_fingerprint: AnchorFingerprint added to
- Re-anchor pipeline updated in both tools: entity-aware lookup with Jaccard
- ReviewedBit { commit_id, entity_id, content_hash } storage in jjr
- Auto-mark on entity diff entry; ✓ glyph on reviewed entity rows in the entity
- Cosmetic filter toggle: ; key hides cosmetic entries entirely; footer shows
- Property-level re-anchoring fuzz tests per spec's Testing Strategy

#### Done When

- New comments capture entity_id (when line is inside an entity) and
- Comment on body line of authenticate() survives jj squash that preserves the
- Renames matched by Jaccard (above 30% similarity) carry comments to the new
- Renames below Jaccard threshold leave comments without an entity match;
- Step 2 of re-anchor pipeline (search within matched entity) does NOT fall
- Repeated-line content (multiple return Err(e) lines) does not cause silent
- Below-threshold fingerprint matches (score <3) mark stale rather than picking
- Comments that auto-re-anchor under re-anchor pipeline acquire a fresh
- Per-entity reviewed bit for authenticate() at content_hash A clears when
- Sibling untouched entities in the same file retain their reviewed bits when
- Jaccard rename authenticate() → verifyAuth() with unchanged content
- Jaccard rename with changed content does not carry reviewed bit — entity
- Reviewed-bit storage file gains a schema_version: u32 field; mismatched
- ; toggle hides cosmetic entities; footer hint reflects current state
- Severity filter required + ; (cosmetic hidden) shows only entities where
- Property-level fuzz tests: edit scripts (insert lines, rename function, move

#### Depends On

- entity-list-and-entity-diff-with-loading

## Phase 5: Claude context bundle for jjr

| Status         | Started | Completed |
| -------------- | ------- | --------- |
| ⬜ not-started |         |           |

Replace the raw-hunk Claude prompt in jjr with a semantic bundle. This is the
'machine helps you understand the change' payoff — Claude gets entity-shaped
context rather than line-shaped context, which closes the AI-accuracy gap on
tasks like 'address this comment without breaking adjacent code.'

**Scope: jjr only**

This phase delivers the jjr Claude integration. ggr+Claude is explicitly out of
scope — see spec section 'Claude Context Enrichment (jjr only)' and Later
Enhancements. The 'ggr — entity-only bundle' the spec previously implied was a
mistaken framing; ggr+Claude is a different feature entirely (the reviewer does
not own the code; 'Claude addresses the comment by editing' does not apply).
When that feature is designed, it will be its own phase.

**Absorbed sem-core context logic**

The dependency-graph walk plus token budgeting from sem-core's `context`
command. Source paths in the sem-core repo (under `crates/sem-core/src/`):

- The dependency-graph extraction logic (cross-file symbol resolution) lives
  within the parser plugins and registry — extending Phase 1's absorbed code
  rather than introducing a new module from scratch. Where Phase 1 absorbed
  entity-level extraction, Phase 5 adds the graph layer.
- The context-budgeting logic from sem-core's CLI `context` command — extract
  the bundle-assembly + budgeting code, drop the CLI wrapping.

Lives under `crates/local-review-core/src/semantic/context/`. Adapts to project
lints. No subprocess to sem-core — direct in-process call.

**The dependency graph**

Built at extraction time, jjr only (since it requires the local repo). Phase 2's
CacheEntry.graph: Option<GraphData> slot is populated here.

GraphData shape (defined here; Phase 2 reserved the slot as a unit type):

```rust
pub struct GraphData {
    pub nodes: Vec<GraphNode>,    // every entity in the repo, keyed by EntityId
    pub edges: Vec<GraphEdge>,    // calls
}

pub struct GraphNode {
    pub id: EntityId,
    pub kind: EntityKind,
}

pub struct GraphEdge {
    pub from: EntityId,           // caller
    pub to: EntityId,             // callee
}
```

From this, direct callers and callees of any entity are computed by filtering
edges.

Performance: graph build parses every source file in the local repo, not just
the changed files. The spec's Gotchas section flags this as a watch-item for
large repos (>1000 source files). For v1: build per-commit, cache alongside
entity list. Best-effort under 1 second on typical repos; if observed to exceed
the 1-second loading-overlay threshold on large repos, the user sees the loading
overlay during the build (existing Phase 3 mechanism handles this gracefully).
The per-repo-snapshot graph cache optimization the spec lists is deferred to
Later Enhancements.

If graph construction fails (parse errors across the repo, etc.), the cache
entry's graph stays None and the Claude bundle falls back to comment + target
entity + hunk (no deps/dependents). The reviewer is not blocked from using
Claude; they just get less context.

**Schema versioning**

Phase 2 reserved `graph: Option<GraphData>` in CacheEntry but the GraphData type
was a unit type. Phase 5 fills in the real GraphData shape. Two options on the
schema_version handling:

- Option A: Bump schema_version when GraphData gains real fields. Old cache
  entries with None graph still load; old cache entries that were written by
  Phase-2-era code with the unit-type GraphData fail the version check and
  re-extract.
- Option B: Design GraphData to be serde-compatible across the type evolution
  (e.g., use a #[serde(default)] tagged enum). No version bump needed.

Go with Option A — clean break is simpler than serde gymnastics; cache
re-extraction is cheap when it happens.

**Bundle composition**

Per comment in jjr:

1. **Comment text and severity** (existing) — required
2. **Target entity** — full body of the function / method / class containing the
   comment, current state — required
3. **Direct dependencies** — entities the target calls (so Claude doesn't break
   contracts it relies on) — budget-bounded
4. **Direct dependents** — entities that call the target (so Claude preserves
   the API surface) — budget-bounded
5. **Diff hunk** for the commented line (Claude still needs to see what changed
   at the line, not just the entity's current state) — required

All token-budgeted, packed in priority order. Replaces the current raw-hunk text
prompt.

**Token budget**

Default 16,000 tokens per comment. Override via env var `JJR_CONTEXT_BUDGET`
(integer token count).

16k is more generous than sem-core's 8k default — Claude's context window is
large, and review benefits from richer context than sem's CLI-agent case.

Token counting: char count / 4 heuristic. Fast, simple, accurate enough for
budgeting. Within ~20% of actual token count for English/code mix. Replace with
sem-core's exact tokenizer later if the heuristic produces visibly bad
truncation decisions; not needed for v1.

**Truncation rule when over budget**

Items packed in priority order 1 → 5. Items 1 (comment), 2 (target entity), 5
(diff hunk) are REQUIRED. Always packed. Never dropped even if individually
large (in the rare case where a single comment + target + hunk exceeds the
budget, ship them anyway — exceeding budget is preferable to dropping required
items).

Items 3 (deps) and 4 (dependents) are budget-bounded. Drop progressively when
over budget. Order: dependents first (lower information density per token in
most cases), then dependencies. Within either category, drop whole entries — a
half-entity is worse than a missing entity.

Truncation telemetry: append a one-line note to the Claude prompt: 'context
truncated to budget; 4 of 7 dependents omitted'. Claude knows the picture is
partial. (User-facing telemetry — logging truncation events to the user — is
OPEN in the spec; default is no user-facing log, only the Claude prompt note.)

**Replaces the raw-hunk Claude prompt**

Find every call site in `crates/jjr/src/claude.rs` where Claude is invoked with
diff hunk text; rewrite to use the new bundle assembly. The bundle is serialized
in a Markdown format Claude can parse efficiently:

````markdown
## Comment (severity: required)

<body>

## Target: `auth::AuthService::authenticate` at src/auth.rs lines 42-78

<full entity body>

## Direct dependencies (entities called by target)

### `db::Session::parse` at src/db.rs lines 12-30

<body>

## Direct dependents (entities that call target)

### `LoginHandler::run` at src/handlers/login.rs lines 8-22

<body>

## Diff hunk at src/auth.rs line 56

```diff
<hunk lines>
```

context truncated to budget; 3 of 8 dependents omitted
````

The exact format is implementation choice; the structured Markdown above is the
recommended shape.

No UI changes. The bundle is constructed inside the Claude-invocation path; the
user sees the same flow (C to hand off, etc.). Quality improvement is felt
downstream when Claude's responses get better.

**Reference**

specs/semantic-entity-navigation.md sections: 'Claude Context Enrichment (jjr
only)' (full bundle composition, budget, truncation rule, why jjr-only), 'Scope:
jjr vs ggr' (Claude integration is jjr-only in v1), 'Gotchas and Constraints'
(caller-count computation cost — graph build is the expensive step), 'Later
Enhancements' (ggr+Claude is a different feature, deferred).

#### Delivers

- Absorbed sem-core dependency-graph + context-budgeting logic in
- Phase 2's CacheEntry graph: Option<GraphData> field populated for jjr at
- jjr Claude-handoff path replaced (in crates/jjr/src/claude.rs): each comment
- Token budget via JJR_CONTEXT_BUDGET env var, default 16000 tokens per comment
- Truncation rule: comment + target entity + diff hunk always packed; dependents
- Truncation note appended to Claude prompt when over budget
- Token counter: char count / 4 heuristic (sufficient for v1; tuneable later)

#### Done When

- Absorbed dependency-graph code resolves cross-file calls within the local
- GraphData populates jjr's CacheEntry.graph at extraction time; persists across
- jjr handoff includes deps and dependents in the bundle when within budget
- Bundle progressively truncates when over budget: drop entire dependents
- Setting JJR_CONTEXT_BUDGET=8000 constrains the bundle to approximately that
- Over-budget bundle includes 'context truncated to budget; N of M dependents
- Bundle replaces the raw-hunk Claude prompt at every Claude invocation call
- ggr Claude path is NOT modified in this phase — ggr+Claude is deferred (see
- Cache schema version is bumped from Phase 2's value to a new value when

#### Depends On

- comment-and-reviewed-bit-migration

## Phase 6: Caller count in status bar

| Status         | Started | Completed |
| -------------- | ------- | --------- |
| ⬜ not-started |         |           |

Wire the dependency graph from Phase 5 into the entity diff view's status-bar
context line. This is the final piece of the 'machine helps you understand the
change' promise — the reviewer sees blast-radius information in their peripheral
vision while reading the diff.

**Status-bar format**

When cursor is within an entity's line range in jjr entity diff view:

authenticate() modified · sig+body · called from 8 places

Persistent while cursor is in range. Yields briefly to transient messages (save
confirmations, etc.) — they display per their existing timeout in the status-bar
mechanism, then context returns. Cursor outside any entity range →
entity-context portion clears; only the change-annotation portion stays if
applicable (it doesn't, because we're outside any entity); other status content
can still display.

**ggr counterpart**

ggr entity diff view shows entity name + annotation in the status bar but does
NOT show 'called from N places'. ggr lacks the local repo, so no graph data. ggr
stops at 'authenticate() modified · sig+body'. This is the only difference
between jjr and ggr in this phase.

**Caller count source**

Phase 5's absorbed dependency graph, stored in CacheEntry.graph for jjr. For
each entity, count of direct callers = count of GraphEdges where edge.to ==
entity.id. Direct callers only — transitive callers explicitly out of scope per
the spec.

**Cursor tracking ownership**

Phase 3's entity diff view already knows the entity's LineRange (passed in from
fetch_entity_diff's return value). Phase 6 wires that into the cursor-tracking
pipeline: on cursor movement events, check whether the new cursor line falls
within any entity's LineRange in the current file (the entity list for the file
is in the entity list cache). If yes, fire a status-bar context update; if no,
clear the entity-context portion.

This means Phase 6 does NOT add cursor position tracking — Phase 3 already has
it for scroll positioning. Phase 6 adds the entity-range lookup on each cursor
event and the status-bar formatting.

**Status-bar mechanism**

The existing status-bar slot in local-review-core::tui (specifically the App
state's status message field, currently used for transient confirmations like
'comment saved') is repurposed for persistent entity context. The priority rule:

- Transient messages display for their existing timeout duration
- After the timeout (or if no transient is showing), the entity context (if
  cursor is in an entity range) displays
- Outside any entity range, the slot shows whatever it would show today

If the App state currently has no concept of 'persistent vs transient' status
messages, add it as part of this phase: a status-bar enum with two variants
(TransientMessage { text, expires_at } | PersistentEntityContext { entity_id,
text }). The render layer prefers Transient if not expired, otherwise
Persistent.

**Performance**

The graph is built once at extraction time (Phase 5) and cached. Per-focus
lookup is a count over edges — a hashmap probe (if edges are indexed by `to`) or
a small linear scan. Under 50ms per focus event is generous; expectation is
sub-1ms.

**Cache scope**

Per-commit graph cache (alongside entity list) for v1. The spec calls out
per-repo-snapshot graph cache as a future optimization if extraction time
becomes a problem with deep stacks — out of scope for this phase.

No new bindings, no new screens. Status-bar enhancement on the existing entity
diff view from Phase 3.

**Reference**

specs/semantic-entity-navigation.md sections: 'Passive context in the status
bar' (format and behavior), 'Scope: jjr vs ggr' (caller count is jjr only),
'Gotchas and Constraints' (caller-count computation cost — direct callers only).

#### Delivers

- jjr entity diff view shows passive status-bar context when cursor is within an
- Status bar updates as cursor moves between entities in the diff view
- ggr entity diff view shows entity context (name + annotation) but no caller

#### Done When

- Opening an entity diff in jjr places cursor on entity anchor line; status bar
- Scrolling cursor to a different entity in the same file updates the status-bar
- Scrolling cursor outside any entity range clears the entity-context portion of
- ggr entity diff view shows 'authenticate() modified · sig+body' (entity name +
- Caller count lookup per focus event completes in under 50ms (cached graph +
- Transient messages (save confirmations, etc.) display briefly per their

#### Depends On

- claude-context-bundle-for-jjr

## Notes

### Index glossary

The codebase juggles four distinct indices after this migration. Naming them
explicitly:

- **stack_entry_idx** (existing): which entry in the stack/PR. In ggr: which
  commit. In jjr: which change. Driven by `n` / `p` for stack navigation.
- **entity_index** (new, primary on Screen::Main): cursor within the entity
  list. Driven by `j` / `k`.
- **file_index** (existing, secondary): index into the entry's file list. Used
  only by Screen::FileDiff (the escape hatch reached via `F`).
- **view_idx** (existing internal): index into `fetch_views`' Vec<DiffView>.
  Still used by Screen::FileDiff via fetch_views.

When a phase says 'entry_idx' in a trait signature, that's stack_entry_idx.

### Non-goals

See `specs/semantic-entity-navigation.md` section 'Non-Goals' for the full list.
Briefly: this ladder does NOT deliver a dependency-graph viewer or
impact-analysis dashboard (graph data is used internally for Claude context and
caller count, never as a navigation tier or panel), a language server / static
analyzer / refactoring tool, sem-core upstream sync (we own the absorbed code),
or async extraction beyond the bounded worker-thread design in Phase 3.

**ggr + Claude integration is also a non-goal of this ladder.** It is a
different feature than jjr + Claude (the reviewer does not own the code in ggr;
the 'Claude addresses the comment by editing' semantic doesn't apply). The
plausible use cases for ggr+Claude (compose review text, pre-review pass, polish
submission, on-demand explanation, suggested-change blocks) each warrant their
own design pass. Deferred to its own future spec — see Later Enhancements in
`specs/semantic-entity-navigation.md`.

### Phasing notes

- Phases 1 and 2 are infrastructure-only (no user-visible UI). Phase 3 is the
  first user-visible payoff.
- Strict linear dependencies: 1 → 2 → 3 → 4 → 5 → 6. Each phase is a
  self-contained PR.
- Phase 5 forward-dependencies on Phase 2's CacheEntry shape: Phase 2 reserves a
  `graph: Option<GraphData>` field with GraphData as a unit type; Phase 5 fills
  in the real shape and bumps schema_version. Phase 2 must not skip the
  reservation.
- Testing per spec's Testing Strategy ships incrementally: golden corpus arrives
  in Phase 1; unit, property, and integration tests land alongside the features
  they cover in each subsequent phase.

### Backward compatibility

Old comment drafts without `entity_id` / `anchor_fingerprint` fall back to the
existing file-wide fuzzy line matching from the base specs — no forced
migration. Old per-file reviewed bits remain valid for fallback rows and the
file diff escape hatch. The reviewed-state file gains a `schema_version` field
but reads of old (no-version) files succeed with legacy-format interpretation.
No data migration scripts required.

### Spec open questions still open

The spec leaves these explicitly open. The ladder does not resolve them. The
implementer should follow the spec's defaults but watch for evidence to revisit:

1. `signature_key` shape for languages without typed signatures (Python, untyped
   JavaScript). Default: ordinal-only disambiguation; revisit if observed
   collisions cause real problems.
2. Cosmetic filter persistence across sessions. Default: persist; revisit if
   confusing.
3. Truncation telemetry to the user. Default: no user-facing log, only the
   Claude prompt note.
4. Reviewed-bit garbage collection. Default: no reaping (matches cache eviction
   posture).
5. AnchorFingerprint scoring threshold. Default: fixed at ≥3; revisit if
   observed stale-vs-mis-anchored rates warrant tuning.

### Spec reference

Full domain model: `specs/semantic-entity-navigation.md`. Each phase description
quotes relevant principles, but the spec is the source of truth for any question
not directly answered by the phase description.
