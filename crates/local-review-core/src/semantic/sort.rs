//! Topological sort for entity lists.
//!
//! `topo_sort_entities` reorders a `Vec<EntitySummary>` so that callees
//! (dependencies) appear before their callers. The sort uses Kahn's algorithm
//! on the reversed call graph — entities with no outgoing edges into the
//! changed set (pure callees) go first; pure callers go last.
//!
//! When the graph has cycles or disconnected nodes, those entities are appended
//! in the existing file-then-line order so none are dropped.

use std::collections::{HashMap, VecDeque};

use crate::semantic::cache::GraphData;
use crate::semantic::entity::{EntityId, EntitySummary};

/// Sort `entities` in dependency-first (callees-before-callers) order using
/// the call graph. Entities not connected to any other entity in the list are
/// appended in file+line order. When `graph` is empty or has no edges among
/// the changed entities, the order is unchanged.
pub fn topo_sort_entities(entities: &mut Vec<EntitySummary>, graph: &GraphData) {
    let n = entities.len();
    if n <= 1 {
        return;
    }

    // Map EntityId → index in `entities`.
    let id_to_pos: HashMap<&EntityId, usize> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| (&e.id, i))
        .collect();

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
            comment_count: 0,
            reviewed: false,
        }
    }

    fn graph(edges: &[(&str, &str, &str, &str)]) -> GraphData {
        GraphData {
            nodes: Vec::new(),
            edges: edges
                .iter()
                .map(|(ff, fn_, tf, tn)| GraphEdge {
                    from: eid(ff, fn_),
                    to: eid(tf, tn),
                })
                .collect(),
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
