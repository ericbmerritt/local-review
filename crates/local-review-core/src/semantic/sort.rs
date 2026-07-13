//! Entity-list ordering: order modes and the topological sort.
//!
//! `sort_entities` dispatches on [`OrderMode`]. The default **risk** mode
//! front-loads High-tier entities (ordering is causal for review quality —
//! Fregnan et al.), keeping dependency order within each tier so
//! comprehension still builds callees-first. `topo_sort_entities` reorders
//! a `Vec<EntitySummary>` so that callees (dependencies) appear before
//! their callers, using Kahn's algorithm on the reversed call graph —
//! entities with no outgoing edges into the changed set (pure callees) go
//! first; pure callers go last.
//!
//! When the graph has cycles or disconnected nodes, those entities are appended
//! in the existing file-then-line order so none are dropped.

use std::collections::VecDeque;

use crate::semantic::cache::GraphData;
use crate::semantic::entity::EntitySummary;
use crate::semantic::risk::RiskTier;

/// Entity-list order, cycled by `o`. Session-persisted (a field on the
/// running app), never written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderMode {
    /// Tier descending; dependency order within a tier; file+line as the
    /// final tiebreak. The default.
    Risk,
    /// Callees before callers (topological sort over the call graph).
    Dependency,
    /// File path, then start line.
    File,
}

impl OrderMode {
    /// The next mode in the `o` cycle: risk → dependency → file → risk.
    pub fn next(self) -> Self {
        match self {
            Self::Risk => Self::Dependency,
            Self::Dependency => Self::File,
            Self::File => Self::Risk,
        }
    }

    /// Lowercase mode name for the footer and status messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Risk => "risk",
            Self::Dependency => "dependency",
            Self::File => "file",
        }
    }
}

/// Sort `entities` according to `mode`. Modes that need the call graph
/// (dependency order, and risk's within-tier ordering) fall back to
/// file+line order when `graph` is `None`, so cycling modes is always
/// deterministic even on graph-less surfaces.
///
/// Risk mode expects tiers to be present (`compute_risk_tiers` has run);
/// entities with no computed tier sort with Medium.
pub fn sort_entities(
    entities: &mut Vec<EntitySummary>,
    mode: OrderMode,
    graph: Option<&GraphData>,
) {
    let dependency_or_file = |entities: &mut Vec<EntitySummary>| match graph {
        Some(g) => topo_sort_entities(entities, g),
        None => file_line_sort(entities),
    };
    match mode {
        OrderMode::File => file_line_sort(entities),
        OrderMode::Dependency => dependency_or_file(entities),
        OrderMode::Risk => {
            dependency_or_file(entities);
            // Stable sort: within a tier the dependency (or file) order
            // from the pass above is preserved.
            entities.sort_by_key(|e| e.risk.as_ref().map_or(RiskTier::Medium, |r| r.tier).rank());
        }
    }
}

fn file_line_sort(entities: &mut [EntitySummary]) {
    entities.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.line_range.0.cmp(&b.line_range.0))
    });
}

/// Sort `entities` in dependency-first (callees-before-callers) order using
/// the call graph. Entities not connected to any other entity in the list are
/// appended in file+line order. When `graph` is empty or has no edges among
/// the changed entities, the order is unchanged.
pub fn topo_sort_entities(entities: &mut Vec<EntitySummary>, graph: &GraphData) {
    let n = entities.len();
    if n <= 1 {
        return;
    }

    let id_to_pos = crate::semantic::entity::index_by_id(entities);

    // Build the reversed subgraph: only edges where both from and to are
    // changed entities. In the original graph A→B means "A calls B". In the
    // reversed graph B→A means "B is called by A". Kahn's on the reversed
    // graph yields callees (B) before callers (A).
    let mut rev_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    // in_deg here = in-degree in the reversed graph = out-degree in original
    // = how many changed entities this entity calls.
    let mut in_deg: Vec<usize> = vec![0; n];

    for edge in &graph.edges {
        let Some(&f) = id_to_pos.get(&edge.from) else {
            continue;
        };
        let Some(&t) = id_to_pos.get(&edge.to) else {
            continue;
        };
        // Original: f calls t. Reversed: t → f.
        rev_adj[t].push(f);
        in_deg[f] += 1;
    }

    // Nodes that appear in no edge (no in or out edges in the subgraph) are
    // "disconnected" from the changed-entity graph. Sorting them with the
    // connected nodes would interleave them arbitrarily; instead collect them
    // separately and append in file+line order at the end.
    let has_edge: Vec<bool> = {
        let mut h = vec![false; n];
        for edge in &graph.edges {
            if let (Some(&f), Some(&t)) = (id_to_pos.get(&edge.from), id_to_pos.get(&edge.to)) {
                h[f] = true;
                h[t] = true;
            }
        }
        h
    };

    // If no edges among changed entities, there is nothing to sort.
    if has_edge.iter().all(|&b| !b) {
        return;
    }

    let order = kahn_order(n, &rev_adj, &mut in_deg, &has_edge, entities);

    // Permute entities in place.
    let old = std::mem::take(entities);
    *entities = order.into_iter().map(|i| old[i].clone()).collect();
}

/// Kahn's BFS on the reversed graph, returning entity indices in callee-first
/// order. `has_edge[i]` marks connected nodes (participate in at least one
/// edge). Disconnected nodes are appended at the end in file+line order so
/// connected entities form a coherent group. Cyclic nodes that Kahn's can't
/// order are also appended in file+line order.
fn kahn_order(
    n: usize,
    rev_adj: &[Vec<usize>],
    in_deg: &mut [usize],
    has_edge: &[bool],
    entities: &[EntitySummary],
) -> Vec<usize> {
    // Seed: connected nodes with in-degree 0 in the reversed graph call
    // nothing in the changed set → pure callees / dependencies → go first.
    let mut seeds: Vec<usize> = (0..n).filter(|&i| has_edge[i] && in_deg[i] == 0).collect();
    seeds.sort_by_key(|&i| (&entities[i].file_path, entities[i].line_range.0));

    let mut queue: VecDeque<usize> = seeds.into();
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut visited = vec![false; n];

    while let Some(pos) = queue.pop_front() {
        order.push(pos);
        visited[pos] = true;

        let mut newly_zero: Vec<usize> = Vec::new();
        for &nbr in &rev_adj[pos] {
            in_deg[nbr] -= 1;
            if in_deg[nbr] == 0 {
                newly_zero.push(nbr);
            }
        }
        // Tie-break within each BFS layer by file+line.
        newly_zero.sort_by_key(|&i| (&entities[i].file_path, entities[i].line_range.0));
        for next in newly_zero {
            queue.push_back(next);
        }
    }

    // Unvisited: cyclic connected nodes + disconnected nodes — append in
    // file+line order. Disconnected nodes come after cyclic ones to keep
    // the connected subgraph together.
    if order.len() < n {
        let mut unvisited: Vec<usize> = (0..n).filter(|&i| !visited[i]).collect();
        unvisited.sort_by_key(|&i| (&entities[i].file_path, entities[i].line_range.0));
        order.extend(unvisited);
    }

    order
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::semantic::cache::GraphEdge;
    use crate::semantic::entity::{ChangeAnnotation, ChangeType, EntityKind, EntitySummary};
    use crate::semantic::entity_id::EntityId;

    fn eid(file: &str, name: &str) -> EntityId {
        EntityId::new(PathBuf::from(file), vec![name.to_owned()], None, 0)
    }

    fn summary(file: &str, name: &str, line: u32) -> EntitySummary {
        EntitySummary {
            id: eid(file, name),
            display_name: name.to_owned(),
            kind: EntityKind::Function,
            change: ChangeType::Modified,
            annotation: ChangeAnnotation::BodyOnly,
            file_path: PathBuf::from(file),
            source_file: None,
            target_line: None,
            line_range: (line, line + 5),
            structural_change: true,
            content_hash: 0,
            refactor: None,
            comment_count: 0,
            reviewed: false,
            risk: None,
            fallback: false,
        }
    }

    fn with_tier(mut e: EntitySummary, t: RiskTier) -> EntitySummary {
        e.risk = Some(crate::semantic::risk::RiskAssessment {
            tier: t,
            clause: String::new(),
        });
        e
    }

    fn graph(edges: &[(&str, &str, &str, &str)]) -> GraphData {
        GraphData {
            nodes: Vec::new(),
            edges: edges
                .iter()
                .map(|(ff, fn_, tf, tn)| GraphEdge {
                    from: eid(ff, fn_),
                    to: eid(tf, tn),
                    call_sites: Vec::new(),
                })
                .collect(),
            unresolved: Vec::new(),
        }
    }

    #[test]
    fn callee_sorts_before_caller() {
        let mut entities = vec![summary("a.rs", "caller", 10), summary("b.rs", "callee", 1)];
        // caller → callee
        let g = graph(&[("a.rs", "caller", "b.rs", "callee")]);
        topo_sort_entities(&mut entities, &g);
        assert_eq!(entities[0].id.name(), "callee");
        assert_eq!(entities[1].id.name(), "caller");
    }

    #[test]
    fn chain_sorted_deepest_first() {
        let mut entities = vec![
            summary("a.rs", "top", 30),
            summary("a.rs", "mid", 20),
            summary("a.rs", "bot", 10),
        ];
        // top → mid → bot
        let g = graph(&[
            ("a.rs", "top", "a.rs", "mid"),
            ("a.rs", "mid", "a.rs", "bot"),
        ]);
        topo_sort_entities(&mut entities, &g);
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["bot", "mid", "top"]);
    }

    #[test]
    fn no_edges_among_changed_leaves_order_unchanged() {
        let mut entities = vec![summary("a.rs", "foo", 1), summary("b.rs", "bar", 1)];
        // Edge exists in graph but neither endpoint is in entities.
        let g = graph(&[("x.rs", "other", "y.rs", "external")]);
        let before: Vec<String> = entities.iter().map(|e| e.id.name().to_owned()).collect();
        topo_sort_entities(&mut entities, &g);
        let after: Vec<String> = entities.iter().map(|e| e.id.name().to_owned()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn cyclic_entities_appended_without_panic() {
        let mut entities = vec![summary("a.rs", "ping", 1), summary("a.rs", "pong", 10)];
        // Mutual recursion: ping → pong → ping
        let g = graph(&[
            ("a.rs", "ping", "a.rs", "pong"),
            ("a.rs", "pong", "a.rs", "ping"),
        ]);
        topo_sort_entities(&mut entities, &g);
        // Both entities present, no panic.
        assert_eq!(entities.len(), 2);
    }

    #[test]
    fn order_mode_cycles_risk_dependency_file() {
        assert_eq!(OrderMode::Risk.next(), OrderMode::Dependency);
        assert_eq!(OrderMode::Dependency.next(), OrderMode::File);
        assert_eq!(OrderMode::File.next(), OrderMode::Risk);
    }

    #[test]
    fn risk_mode_sorts_high_before_medium_before_low() {
        let mut entities = vec![
            with_tier(summary("a.rs", "low_e", 1), RiskTier::Low),
            with_tier(summary("a.rs", "med_e", 10), RiskTier::Medium),
            with_tier(summary("a.rs", "high_e", 20), RiskTier::High),
        ];
        sort_entities(&mut entities, OrderMode::Risk, None);
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["high_e", "med_e", "low_e"]);
    }

    #[test]
    fn risk_mode_keeps_dependency_order_within_a_tier() {
        // Both High; caller → callee, listed caller-first so only the topo
        // pass can produce callee-first order within the tier.
        let mut entities = vec![
            with_tier(summary("a.rs", "caller", 10), RiskTier::High),
            with_tier(summary("b.rs", "callee", 1), RiskTier::High),
            with_tier(summary("a.rs", "cosmetic_e", 1), RiskTier::Low),
        ];
        let g = graph(&[("a.rs", "caller", "b.rs", "callee")]);
        sort_entities(&mut entities, OrderMode::Risk, Some(&g));
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["callee", "caller", "cosmetic_e"]);
    }

    #[test]
    fn risk_mode_without_tiers_treats_entities_as_medium() {
        let mut entities = vec![
            summary("b.rs", "untiered", 1),
            with_tier(summary("a.rs", "high_e", 1), RiskTier::High),
            with_tier(summary("c.rs", "low_e", 1), RiskTier::Low),
        ];
        sort_entities(&mut entities, OrderMode::Risk, None);
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["high_e", "untiered", "low_e"]);
    }

    #[test]
    fn dependency_mode_without_graph_falls_back_to_file_order() {
        let mut entities = vec![summary("z.rs", "zeta", 5), summary("a.rs", "alpha", 1)];
        sort_entities(&mut entities, OrderMode::Dependency, None);
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn file_mode_sorts_by_path_then_line() {
        let mut entities = vec![
            summary("b.rs", "late", 30),
            summary("b.rs", "early", 2),
            summary("a.rs", "first", 99),
        ];
        sort_entities(&mut entities, OrderMode::File, None);
        let names: Vec<_> = entities.iter().map(|e| e.id.name()).collect();
        assert_eq!(names, ["first", "early", "late"]);
    }

    #[test]
    fn disconnected_entity_appended_in_file_line_order() {
        let mut entities = vec![
            summary("a.rs", "caller", 20),
            summary("a.rs", "callee", 1),
            summary("z.rs", "island", 5), // no edges
        ];
        let g = graph(&[("a.rs", "caller", "a.rs", "callee")]);
        topo_sort_entities(&mut entities, &g);
        // callee first, caller second, island last (file+line order for disconnected)
        assert_eq!(entities[0].id.name(), "callee");
        assert_eq!(entities[1].id.name(), "caller");
        assert_eq!(entities[2].id.name(), "island");
    }
}
