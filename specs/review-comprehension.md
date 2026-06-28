# Review Comprehension — Engineering Design Document

## User Narrative

### ggr (someone else's PR)

Priya opens Marco's PR #912 — "Rework session expiry" — at 9:40 on a Tuesday,
between two meetings. `ggr 912` drops her onto commit 1 of 5. She does not see a
diff. She sees:

```
 PR #912 · commit 1 of 5 · c41f2ea
 ─────────────────────────────────────────────────────────────────
 ≡ Extract expiry check from Session.validate
   "validate() was doing three jobs; expiry needs to be
    callable from the sweeper (#889)"              [Enter: full]
 Σ 5 entities · 2 files · ~140 LOC · 1 sig change
 ─────────────────────────────────────────────────────────────────
 ── expiry check ──────────────────────────────────── high ──
 !Δ Session.validate()      session.rs   sig+body · 11 callers
  ⊕ Session.is_expired()    session.rs   extracted ← validate()
 ── sweeper wiring ─────────────────────────────────────────
  Δ Sweeper.run()           sweeper.rs   body
  ≈ expiry_margin()         sweeper.rs   moved from session.rs
  Δ EXPIRY_SLACK            config.rs    30 → 45
```

Ten seconds in, she knows the story: one function was split, the sweeper now
calls the extracted half, a constant moved and changed. The header told her
_why_ before she read a line of code — the sweeper needs `is_expired()`
standalone. The `extracted ←` tag tells her `is_expired()` is Marco's editor
doing surgery, not new logic; she'll skim it, not study it. The `!` and
`11 callers` on `validate()` tell her where the actual risk lives: a signature
change with eleven call sites.

She hits Enter on `validate()`. In the diff, the status bar reads
`validate() · high · sig+body · 11 callers`. She presses `x`: an overlay lists
the eleven call sites with one line of context each. Nine pass the new argument.
Two — both in `middleware.rs`, which is _not in this commit_ — still call the
old shape. She writes a required comment: "middleware.rs callers not updated —
does commit 3 cover this, or is it missed?"

`Tab`. Instead of the next row in file order, the cursor lands on
`Sweeper.run()` — the next unreviewed entity by risk. The footer reads
`reviewed 2/5`. The two refactor rows she never opens; the tags already said
what they were. She finishes the commit in six minutes and `n`s to commit 2.

### jjr (your own stack)

That afternoon Eric reviews a Claude-generated three-change stack. The middle
change opens on its header — `≡ Add retry to token refresh`, body peek
`"wraps refresh() in backoff per #204"`,
`Σ 4 entities · 2 files · ~90 LOC · 0 sig changes` — and two clusters. No High
tier anywhere: nothing here changes a signature, and he registers that before
reading a single hunk. What the file list would have buried, the cluster rule
catches: the second cluster is labeled `logging` and holds one Medium entity
that has nothing to do with the change description. Claude touched an unrelated
logging helper while fixing the auth flow. He comments "unrelated — split this
out," presses `g` to flatten the groups when he wants the raw topo order back,
and batches his comments to Claude.

### When the machinery fails

On Thursday Priya opens a PR against a repo whose clone step fails (the GHE
instance is mid-upgrade). ggr says so in the status bar —
`graph unavailable — clone failed; risk tiers degraded` — and the entity list
renders anyway: no caller counts, no `x` overlay, tiers computed without
fan-out. Every entity is still there. Degradation is visible, never silent.

## Purpose

Reduce the cost of _understanding_ a change during review. The evidence is
consistent across three bodies of research: comprehension — not defect hunting —
is the dominant reviewer effort (Bacchelli & Bird ICSE'13; Sadowski et al.
ICSE-SEIP'18), reviewers orient before they analyze (Wurzel Gonçalves et al.
EMSE'25), presentation order causally changes what gets found (Fregnan et al.
ESEC/FSE'22: a defect shown last is ~64% less likely to be caught), and
pre-decomposed changes measurably reduce wrongly-reported issues (di Biase et
al. PeerJ CS'19).

The entity model (see `semantic-entity-navigation.md`) already made the central
bet: the unit of review is the named semantic thing, not the file. This spec
spends the remaining comprehension budget on five levers that exploit data the
tools already extract — the entity delta, the Jaccard matcher, the call graph,
and the per-entity reviewed bit.

## Goals

When this spec ships:

1. Entering a commit/change lands on an entity list headed by an **orientation
   block**: the change's intent (subject + description peek) and a scope line
   (entity/file/LOC counts, signature-change count).
2. Every entity row carries a **refactor-vs-behavior classification**:
   behavior-preserving changes (`renamed`, `moved`, `extracted ←`, `cosmetic`)
   are visually demoted; behavior changes are not.
3. Entities are ordered **risk-tier-first**: High (signature change with
   callers; deletion with surviving references), Medium (added or changed
   behavior), Low (behavior-preserving, cosmetic) — the total mapping lives in
   Design. Within a tier, dependency order (callees before callers). `o` cycles
   risk → dependency → file order.
4. The entity list groups into **concern clusters** — connected components of
   the changed-entity subgraph — rendered as labeled groups by default,
   dissolvable with `g`. Single-cluster changes render flat automatically.
5. A **blast-radius peek** (`x`) on any entity with callers overlays its call
   sites with one context line each, including callers outside the current diff.
6. `Tab` becomes a **guided path**: next unreviewed entity in the current order,
   with `reviewed k/n` progress in the footer.
7. ggr reaches **graph parity** with jjr: the repo clone is first-class (eager,
   with progress feedback and visible failure), so caller counts, risk fan-out,
   clustering, and the blast-radius peek work in both tools.

## Non-Goals

- **Not a RefactoringMiner port.** The refactor taxonomy is limited to what the
  existing Jaccard/AST-hash machinery can derive: rename, move, extract,
  cosmetic. No inline-method, no pull-up/push-down, no 100-type catalogue.
- **Not a numeric risk score.** Tiers only. Every tier assignment must be
  explainable in one status-bar clause ("sig change · 11 callers").
- **Not co-change/omission mining.** "This entity usually changes with X"
  requires entity-level history through renames — its own design. Deferred.
- **Not an LLM feature.** Every lever here is heuristic. Optional Claude
  enrichment (per-entity "why" summaries) is deferred to its own pass.
- **Not editable clusters.** v1 clusters are dissolvable, not mergeable or
  splittable.

## Decisions

Settled; not subject to MVP re-litigation.

1. **Orientation before analysis.** The entity list is headed by intent + scope.
   Reviewers orient (who/why/scope) before reading implementation; the tool
   serves that order instead of skipping it. [CRDM, EMSE'25]
2. **Heuristic core, LLM on top.** Every comprehension feature works without a
   network or model call. LLM output may later _enrich_ (never gate) a surface.
   Preserves the functional-core posture and offline guarantee.
3. **ggr invests in graph parity.** The repo clone (`repo_cache`) is promoted
   from best-effort to first-class: attempted eagerly, progress shown, failure
   surfaced as visible degradation. `--no-graph` / `GGR_NO_GRAPH_CLONE=1` remain
   as opt-outs and imply degraded tiers.
4. **Clusters are the default view, dissolvable, not editable.** Automatic
   untangling is imperfect (UTango, ChangeBeadsThreader); the escape hatch is
   dissolving to flat order (`g`), not editing group membership.
5. **Default order is risk-tiered dependency order.** High tier first (Fregnan:
   order is causal), topo within tier (callees before callers, the existing
   comprehension-building order). `o` cycles risk → dependency → file; the
   choice persists for the session.
6. **Refactor classification is a new field, not new ChangeTypes.**
   `EntityCoreData` gains `refactor: Option<RefactorKind>` (`Renamed`, `Moved`,
   `Extracted { from: EntityId }`). `ChangeType` is untouched. Cache
   `schema_version` bumps.
7. **Tier computation is pure.**
   `risk_tier(core_data, caller_count) -> RiskTier` lives in
   `local-review-core/src/semantic/`, takes data, returns data. Fan-out
   unavailable (no graph) degrades the input to `None`, never fails.
8. **Cluster labels are heuristic.** Longest common scope-chain prefix of the
   cluster's members; falls back to the highest-fan-out member's name; falls
   back to the dominant file stem. No LLM labeling in v1.

## Principles

### Answer "why" before "what"

The most-cited reviewer struggle is understanding the _reason_ for a change. The
orientation header is not chrome; it converts exploratory reading into
confirmatory reading (worked-example effect). Two to three rows is the budget —
at 80×24 the list must still show ~14 entity rows.

### Attention is the scarce resource; ordering spends it

A reviewer's defect-finding collapses on later items regardless of content.
Front-loading High-tier entities is the cheapest measurable win in this spec.
The corollary: anything that inflates the list with undemoted
behavior-preserving rows spends attention on rows that don't need it.

### Degradation is visible, never silent

Missing graph → tiers computed without fan-out, caller affordances hidden, one
status-bar notice. Missing extraction → fallback rows (existing rule). A
reviewer must always be able to answer "am I seeing the full picture?" from the
screen.

### The classification is a hint, not a verdict

`extracted ←` and `cosmetic` are parser heuristics. They demote, they never hide
by default, and the toggle to reveal everything is one key. Same posture as the
existing cosmetic flag.

## Design

### Orientation header

Rendered above the entity list (both tools), replacing the bare description row:

- Row 1: `≡` + subject (existing description row, unchanged binding — Enter
  opens the full description screen).
- Row 2 (new): first non-empty body line, dimmed, truncated; omitted when the
  body is empty.
- Row 3 (new): `Σ N entities · M files · ~L LOC · K sig changes`, computed in
  core from the entity list + diff. No new surface method: LOC from the diff,
  the rest from `Vec<EntitySummary>`.

`DescriptionSummary` gains `body_peek: Option<String>` (populated by each
surface via existing description sources; controls stripped).

### Refactor classification (`differ.rs`)

Emitted per entity during `diff_entities`:

- **Renamed** — Jaccard match across before/after where the scope-chain tail
  differs but file and normalized body hash match or near-match.
- **Moved** — existing move detection (cross-file Jaccard), now also carried in
  `refactor` for uniform rendering.
- **Extracted { from }** — an Added entity whose normalized token set is largely
  (threshold, tuned in tests) contained in the _removed_ span of a Modified
  sibling in the same before-file.
- **Cosmetic** — existing `structural_change == false`.

Behavior-preserving = any of the above with no additional body delta beyond the
refactor itself. `sig+body` on a renamed entity means the rename _plus_ a real
change — classified as behavior change, tagged `renamed +body`.

Rendering: behavior-preserving rows dim (same treatment as cosmetic today),
annotation shows the tag (`extracted ← validate()`, `renamed ← old_name`). `;`
(existing cosmetic toggle) grows to hide all behavior-preserving rows; footer
states the count hidden.

### Risk tiers (`semantic/risk.rs`, new)

The tier function is **total** over
`(ChangeType, Option<RefactorKind>, Option<usize>)`. Every entity — including
fallback rows — lands in exactly one tier; the implementation is an exhaustive
match, no wildcard arm. The description row is not an entity and is exempt
(existing rule).

```
High    — Modified, sig changed, caller_count > 0 or unknown (None)
        — Deleted, surviving references > 0 or unknown (None)
Medium  — Added, non-refactor (new behavior is unreviewed by definition)
        — Modified, behavior body change (incl. config value changes)
        — Modified, sig changed, caller_count == 0
        — Deleted, zero surviving references
        — fallback row (extraction failed — cannot be shown low-risk;
          no badge; status clause "unclassified — extraction failed")
Low     — behavior-preserving refactor (Renamed / Moved / Extracted with
          no additional body delta), cosmetic
```

**Surviving references** (Deleted entities): after-state call sites that still
name the deleted symbol — dangling references. These are the actual breakage;
before-state callers may already have been updated, so the before-state graph is
the wrong instrument. The call graph must therefore record unresolved call
references (callee name + call-site line) alongside resolved edges; the exact
encoding is the implementation's choice, covered by the shared schema bump.

Unknown fan-out always resolves the tier **upward**, never silently down: `None`
caller data on a sig change or deletion yields High with the status clause
"unverified callers", and the degraded-tiers notice appears once per session.

Badge: `!` glyph before the sigil on High rows; tier name in the entity-diff
status bar with its one-clause justification ("high · sig change · 11 callers").

### Ordering (`semantic/sort.rs`)

`o` cycles three modes (persisted for the session):

1. **risk** (default) — tier descending; topo within tier; file+line as the
   final tiebreak.
2. **dependency** — existing topo sort.
3. **file** — existing file+line order.

Clustered rendering composes: clusters sort by max member tier; members sort by
the active mode within the cluster.

### Concern clusters (`semantic/cluster.rs`, new)

Connected components over the changed-entity set: an undirected edge between two
changed entities when the call graph links them (either direction) or when one
is the `Extracted` source of the other.

Entities with no edges to any other changed entity resolve in order:

1. **File affinity.** If the entity shares a file with members of exactly one
   cluster, it joins that cluster.
2. **Nearest member.** If it shares a file with members of two or more clusters,
   it joins the cluster containing the member nearest by line distance in that
   file (deterministic tiebreak: the earlier cluster in render order).
3. **Singleton.** Otherwise it stands alone; singletons render inline — no group
   header — after the labeled clusters, in the active order mode.

Group header row: `── <label> ─── <max-tier> ──`. `g` toggles grouped/flat.
Single-cluster changes render flat (grouping adds nothing). Fallback rows sort
with their file's cluster when one exists.

### Blast-radius peek

`x` on an entity (list or diff) with caller data opens an overlay: each call
site as `file:line · one context line`, sorted callers-in-this-diff first, then
external callers. Selecting a row inside the current diff jumps to it; external
rows are display-only in v1. The affordance is hidden (and `x` is a
status-message no-op) when the graph cannot answer for this entity.

**Call-site data shape.** `GraphEdge` gains `call_sites: Vec<u32>` — after-state
line numbers in the caller's file (`edge.from`'s `file_path`), one entry per
call occurrence, so a caller that invokes the target five times yields five
overlay rows. Deleted entities resolve through the unresolved-reference records
defined in Risk tiers. Both land in the shared schema bump.

**Staleness.** Context lines come from the graph's source checkout (jjr: working
copy; ggr: the clone at PR head — see graph parity). Per-commit views in a
multi-commit PR may therefore show call sites slightly ahead of the commit under
review. Accepted v1 tradeoff; the overlay header names the state it reads from
(`@ <short-sha>`).

### Guided path

`Tab` in the entity list and entity diff advances to the next **unreviewed**
entity in the current order (wrapping; no-op with a status notice when all are
reviewed). Footer gains `reviewed k/n`. The reviewed bit itself is unchanged
(auto-mark on entry, content-hash keyed).

`Shift-Tab` remains **plain previous-entity**, reviewed or not. It cannot mirror
Tab: auto-mark-on-entry means everything behind the reviewer is reviewed by
construction, so "previous unreviewed" would be a near-guaranteed no-op or a
surprising far-wrap. Backward motion is re-examination; forward motion is
progress. The asymmetry is deliberate and documented in the help screens.

### ggr graph parity (`repo_cache.rs`)

- Clone attempted eagerly at PR open (background thread; startup spinner already
  exists), not lazily at first entity fetch.
- The clone is fetched/checked out at the **PR head SHA**. A clone that cannot
  reach that SHA (fork PRs, GHE fetch limits) is treated as clone failure —
  degraded, never a silently wrong-state graph.
- Clone progress and failure surface in the status bar; failure sets a session
  flag rendered as `graph unavailable — <reason>; risk tiers degraded`.
- Existing opt-outs preserved. Opting out routes through the same visible-
  degradation path.
- After parity, the jjr-only caller-count status-bar segment un-forks: both
  tools render it when the graph exists.

## Implementation Impact

Seams touched, so this is treated as a model extension, not feature sprinkles:

- `semantic/entity.rs` — `RefactorKind`, `EntityCoreData.refactor`,
  `DescriptionSummary.body_peek`. One cache `schema_version` bump covering
  `GraphEdge.call_sites: Vec<u32>` and the unresolved-reference records.
- `semantic/differ.rs` — rename/extract detection passes.
- `semantic/risk.rs`, `semantic/cluster.rs` — new pure modules.
- `semantic/sort.rs` — mode enum + risk-tiered ordering.
- `tui/entity_list.rs` — header rows, group rendering, badges, `g`/`o`/`x` keys,
  Tab semantics, footer progress.
- `tui/app.rs` — order/group mode state; blast-radius overlay screen.
- `ReviewSurface` — `call_sites(entry_idx, entity_id) -> Vec<ResolvedCallSite>`
  (default empty); `body_peek` sourcing.
- `ggr/repo_cache.rs` — eager clone + progress/failure surfacing.
- Help screens and footer hints in both binaries.

## Gotchas

- **Cache schema bump is one bump, not three.** `refactor`, edge call-site
  positions, and any core-data change land in a single `schema_version`
  increment; mismatched caches re-extract (existing rule).
- **`strip_controls` at new boundaries.** Cluster labels and `body_peek` derive
  from external input; strip at construction (project default).
- **Extract-detection thresholds live in tests.** The containment threshold is a
  named constant with fixture-based tests for the true-positive (extract method)
  and false-positive (coincidental similarity) cases.
- **Tab semantics change is a behavior break.** Existing muscle memory: Tab =
  next entity. New: next _unreviewed_. The help screen and footer must say so;
  wrapping no-op gets a status message, not silence. Shift-Tab deliberately does
  not mirror it (see Guided path).
- **`;` semantics change is the second behavior break.** Existing: hide cosmetic
  entities. New: hide all behavior-preserving entities (cosmetic +
  refactor-tagged). Help screens and the footer hint must reflect the wider
  scope; the hidden count in the footer keeps the reviewer aware of what they
  are not seeing.
- **Graph staleness is bounded, not eliminated.** One graph per PR (at head SHA)
  / per working copy serves every commit's tiers and call sites. The overlay
  names its source state; do not present graph-derived facts as facts about the
  commit under review.
- **80×24 budget.** Header (3 rows) + group headers cost list rows. Group
  headers render only in grouped mode; the `;` and severity filters compose with
  grouping without double-counting hidden rows in `k/n`.

## Later Enhancements

- Co-change omission hints ("usually changes with X — absent here") — requires
  entity-level history mining through renames.
- Per-entity LLM "why" summaries via the existing Claude bundle (jjr) — optional
  enrichment layer per Decision 2.
- Within-entity structural diff (difftastic-style noise suppression).
- Test-entity pairing (render the test covering a changed entity adjacent).
- Editable clusters (merge/split).
- Hotspot badges from churn history.

## Evidence

Bacchelli & Bird ICSE'13 · Sadowski et al. ICSE-SEIP'18 · Czerwonka et al.
ICSE-SEIP'15 · Bosu et al. MSR'15 · Wurzel Gonçalves et al. EMSE'25
(arXiv:2507.09637) and arXiv:2503.21455 · Fregnan et al. ESEC/FSE'22 · Barnett
et al. ICSE'15 (ClusterChanges) · di Biase et al. PeerJ CS'19 · Tsantalis et al.
ICSE'18 (RefactoringMiner) · Falleri et al. ASE'14 (GumTree) · Chandler &
Sweller 1991 · Cowan BBS 2001 · Cohen et al. 2006.
