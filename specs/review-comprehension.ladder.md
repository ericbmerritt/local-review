## Phase 1: Orientation header

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-09 | 2026-07-09 |

Tags: tui, core, jjr, ggr

Entity list gains the intent + scope block: description subject, body peek, and a stats row (entities / files / LOC / sig changes) computed in core. Lands in both tools.

#### Delivers

- DescriptionSummary.body_peek populated by both surfaces (controls stripped)
- Stats row computed in core from Vec<EntitySummary> + Diff
- Header rendering in tui/entity_list.rs within the 3-row budget
- Help screens updated in both binaries

#### Done When

- Opening a commit in ggr and a change in jjr shows subject, body peek (when body non-empty), and stats row above the entity list
- Empty description body omits the peek row without a blank line
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- (none)

## Phase 2: Refactor-vs-behavior classification

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-09 | 2026-07-09 |

Tags: semantic, differ, cache

differ.rs emits RefactorKind (Renamed / Moved / Extracted{from}) on EntityCoreData; behavior-preserving rows dim with tags; ';' widens from hide-cosmetic to hide-all-behavior-preserving (muscle-memory break — help screens must say so). ONE cache schema_version bump covering: refactor field, GraphEdge.call_sites: Vec<u32> (after-state line numbers in the caller's file, one entry per call occurrence), and unresolved-reference records (callee name + call-site line) for calls whose target entity does not exist at the graphed state — consumed later by risk tiers (deleted-entity survivors) and blast-radius.

#### Delivers

- RefactorKind enum + EntityCoreData.refactor field
- GraphEdge.call_sites: Vec<u32> + unresolved-reference records in GraphData; single schema_version bump for all of it
- Rename detection (scope-tail differs, body hash near-match) in differ.rs
- Extract detection via Jaccard containment against removed spans, threshold as named constant
- Fixture tests: true-positive extract, false-positive coincidental similarity, rename, rename+body
- Entity-list rendering: dimmed rows with 'extracted ← x' / 'renamed ← y' annotations; ';' hides behavior-preserving with footer count; help screens updated

#### Done When

- An extract-method fixture renders the new entity as 'extracted ← <source>' and dimmed
- A rename with an additional body change is NOT demoted and shows 'renamed +body'
- A fixture graph records call_sites lines per edge and an unresolved reference for a call to a deleted symbol
- Old cache entries (previous schema_version) re-extract instead of misparsing
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- (none)

## Phase 3: Risk tiers and ordering

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-09 | 2026-07-09 |

Tags: semantic, sort, tui

Pure risk_tier() in semantic/risk.rs implementing the TOTAL mapping from the spec (exhaustive match, no wildcard arm): High = Modified sig change with callers>0-or-unknown, Deleted with surviving-references>0-or-unknown; Medium = Added non-refactor, Modified behavior body change, sig change with zero callers, Deleted with zero survivors, fallback rows (unclassified); Low = behavior-preserving refactor + cosmetic. Unknown fan-out resolves UPWARD with 'unverified callers' clause. Surviving references for Deleted entities come from the phase-2 unresolved-reference records (after-state dangling calls), NOT the before-state graph. 'o' cycles risk → dependency → file with risk-tiered dependency order as default (session-persisted, not on disk); '!' badge on High rows; tier + one-clause justification in the entity-diff status bar.

#### Delivers

- semantic/risk.rs with the total tier mapping; exhaustive match enforced (no wildcard arm)
- Deleted-entity survivor counts wired to unresolved-reference records
- Order mode enum in sort.rs; risk-tiered dependency order implemented and set as default
- 'o' cycle binding with session persistence; footer shows active order
- High-tier '!' badge; status-bar tier clause (e.g. 'high · sig change · 11 callers'); degraded-tier notice when graph is unavailable

#### Done When

- Every (ChangeType, refactor, caller-availability) combination has a unit test asserting its tier — including Added, Deleted-with-zero-survivors, and fallback rows
- A sig-changed entity with callers sorts before all Medium/Low entities and carries '!'
- With no graph, a sig change tiers High with 'unverified callers' and the status bar says tiers are degraded
- 'o' cycles all three orders and the choice survives entry navigation within the session
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- refactor-vs-behavior-classification

## Phase 4: Ggr graph parity

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-10 | 2026-07-10 |

Tags: ggr, repo-cache, graph

repo_cache clone promoted from best-effort to first-class: eager at PR open on a background thread, fetched/checked out at the PR head SHA (a clone that cannot reach that SHA — fork PRs, GHE fetch limits — is treated as clone failure, never a silently wrong-state graph), progress in the status bar, failure rendered as visible degradation ('graph unavailable — <reason>; risk tiers degraded'). Opt-outs (--no-graph, GGR_NO_GRAPH_CLONE=1) preserved and routed through the same visible path. Caller-count status-bar segment un-forks between the tools.

#### Delivers

- Eager background clone at PR open, checked out at PR head SHA, with status-bar progress
- Visible degradation message on clone failure, unreachable head SHA, or opt-out
- Caller count rendered in ggr entity-diff status bar when graph exists
- Local file:// fixture remote for clone-path tests; --no-graph and GGR_NO_GRAPH_CLONE covered

#### Done When

- Against a local file:// fixture remote, opening a PR yields caller counts in ggr without visiting entries first, and the clone is at the fixture's head SHA
- Simulated clone failure (unreachable remote) and unreachable-SHA both show the degradation notice while the entity list still renders
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- (none)

## Phase 5: Concern clustering

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-13 | 2026-07-13 |

Tags: semantic, cluster, tui

semantic/cluster.rs: connected components over changed entities via call-graph edges + Extracted links. Edgeless entities resolve in order: (1) file affinity — shares a file with exactly one cluster's members → joins it; (2) nearest member by line distance when the file is shared with 2+ clusters (tiebreak: earlier cluster in render order); (3) singleton, rendered inline (no header) after labeled clusters. Heuristic labels (scope-chain prefix → highest-fanout member → file stem, controls stripped). Grouped rendering default, 'g' dissolves to flat; clusters sort by max member tier, members by active order mode. No hard ggr dependency: without a graph every entity is edgeless → all singletons → renders flat automatically (visible-degradation principle); full clustering in ggr arrives with graph parity.

#### Delivers

- semantic/cluster.rs pure module with component, file-affinity/nearest-member/singleton resolution, and label computation
- Group header rows '── <label> ─ <max-tier> ──' in entity list
- 'g' toggle grouped/flat; single-cluster changes render flat automatically
- Severity/';' filters compose with grouping; k/n counts exclude hidden rows exactly once

#### Done When

- A two-concern fixture change renders two labeled groups ordered by max tier
- An edgeless entity sharing a file with two clusters joins the one with the nearest member by line distance (unit test)
- 'g' flattens to the active order mode and back; a single-cluster change shows no group headers
- With no graph, the list renders flat with no group headers and no error
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- risk-tiers-and-ordering

## Phase 6: Blast-radius peek

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-10 | 2026-07-10 |

Tags: tui, graph, core

'x' on an entity with caller data overlays call sites (file:line + one context line), in-diff callers first then external; Enter jumps to in-diff callers, external rows display-only; affordance hidden ('x' = status-message no-op) when the graph cannot answer. Uses GraphEdge.call_sites (one overlay row per call occurrence — a caller invoking the target 5 times yields 5 rows) and, for Deleted entities, the unresolved-reference records. Context lines come from the graph's source checkout (jjr working copy / ggr clone at PR head); the overlay header names that state ('@ <short-sha>') because per-commit views may see call sites slightly ahead of the commit under review — bounded, named staleness, per the spec gotcha.

#### Delivers

- ReviewSurface::call_sites(entry_idx, entity_id) -> Vec<ResolvedCallSite> with default-empty impl
- Call-site overlay screen in core tui with in-diff/external partition and '@ <short-sha>' source-state header
- Jump-to-caller for in-diff rows; Deleted entities list dangling references
- Affordance hidden with status-message no-op when caller data is unavailable

#### Done When

- 'x' on a sig-changed entity lists its callers with context lines in both tools (graph present), one row per call occurrence
- 'x' on a Deleted entity with dangling references lists them; with zero survivors it reports so rather than opening an empty overlay
- Selecting an in-diff caller jumps to that row; external callers render but do not navigate
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- ggr-graph-parity
- risk-tiers-and-ordering

## Phase 7: Guided review path

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Tags: tui, reviewed-bit

Tab advances to the next UNREVIEWED entity in the active order (wrapping; status notice — not silence — when everything is reviewed); Shift-Tab stays PLAIN previous-entity, reviewed or not — it cannot mirror Tab because auto-mark-on-entry makes everything behind the reviewer reviewed by construction, so 'previous unreviewed' would near-always no-op. The asymmetry is deliberate and documented in help screens. Footer gains 'reviewed k/n' consistent with active filters.

#### Delivers

- Tab: skip-reviewed traversal in list and entity-diff screens; Shift-Tab: plain previous-entity, unchanged semantics
- 'reviewed k/n' footer segment consistent with active filters
- All-reviewed wrap no-op with status message
- Help screen updates in both binaries documenting the Tab change and the deliberate Tab/Shift-Tab asymmetry

#### Done When

- Tab from a reviewed entity lands on the next unreviewed one in the active order
- Shift-Tab moves to the previous entity regardless of reviewed state
- With every entity reviewed, Tab shows a status notice and stays put
- Footer k/n matches the visible (filtered) entity set
- cargo test --workspace and cargo clippy --workspace exit 0

#### Depends On

- risk-tiers-and-ordering

## Notes

### Sequencing rationale

Phases 1, 2, 4 are independent starts. Risk tiers (3) need the refactor classification (Low tier) and the unresolved-reference records (Deleted survivors) from 2. Clustering (5) needs tiers for group ordering only — it deliberately does NOT depend on ggr parity: without a graph it degrades to a flat list (all singletons), per the visible-degradation principle. Blast radius (6) needs call-site data (2, via 3's dependency), tiers for the caller-availability affordance rule (3), and parity (4) for ggr. The guided path (7) needs the default order to be worth guiding along.

### Deferred (see spec Later Enhancements)

Co-change omission hints, LLM 'why' summaries, within-entity structural diff, test pairing, editable clusters, hotspot badges.

### Formatting

Ladder files are exempt from prettier (.prettierignore): pgc owns this file's format, and prettier's rewrapping corrupts bullets on the next pgc round-trip.
