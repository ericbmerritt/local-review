//! Concern clustering: connected components over the changed-entity set.
//!
//! Two changed entities belong to the same concern when the call graph
//! links them (either direction) or when one is the `Extracted` source of
//! the other. Entities with no edges to any other changed entity resolve
//! by file affinity, then nearest member by line distance, then stand
//! alone as singletons (spec: specs/review-comprehension.md, "Concern
//! clusters").
//!
//! The module is pure: it takes the *already ordered* entity slice (the
//! active order mode has been applied) and returns a permutation plus
//! group spans. Member order inside a group and singleton order both
//! preserve the input order, so clustering composes with whatever order
//! mode is active. Without a graph every entity is edgeless → all
//! singletons → callers render flat automatically (visible degradation,
//! never an error).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::semantic::cache::GraphData;
use crate::semantic::entity::{EntitySummary, RefactorKind};
use crate::semantic::risk::RiskTier;

/// One labeled concern group, as a span into the permuted entity order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSpan {
    /// Heuristic label: shared scope-chain prefix, else the
    /// highest-fanout member's name, else the dominant file stem.
    pub label: String,
    /// Highest member tier — drives group ordering and the header badge.
    pub max_tier: RiskTier,
    /// First index of the group in the permuted order.
    pub start: usize,
    /// Number of members.
    pub len: usize,
}

/// Result of [`cluster_entities`]: a permutation of the input indices
/// (cluster members contiguous, singletons trailing) plus the group spans.
/// `groups` is empty when clustering adds nothing (fewer than two labeled
/// clusters) — callers render flat and skip the permutation.
///
/// The concern verdict is reported even when the render stays flat: "this
/// change is one connected concern" is orientation signal in its own
/// right (evidence of a well-scoped change), and silence reads as "the
/// feature did nothing".
#[derive(Debug, Default)]
pub struct Clustering {
    pub order: Vec<usize>,
    pub groups: Vec<GroupSpan>,
    /// Labeled-cluster count after edgeless resolution — the Σ-header
    /// "N concerns" figure. `0` when no call-connected structure exists
    /// (no graph, or every entity is a singleton).
    pub concern_count: usize,
    /// The single concern's label when `concern_count == 1`.
    pub single_concern: Option<String>,
}

/// Minimal union-find (path halving, min-root union for determinism).
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// Cluster `entities` (already sorted by the active order mode) using
/// `graph`. Returns an empty [`Clustering::groups`] when grouping adds
/// nothing: no graph, all singletons, or a single cluster.
pub fn cluster_entities(entities: &[EntitySummary], graph: Option<&GraphData>) -> Clustering {
    let n = entities.len();
    let Some(graph) = graph else {
        return Clustering::default();
    };
    if n < 2 {
        return Clustering::default();
    }

    let mut clusters = initial_components(entities, graph);

    // Snapshot each cluster's file set before resolving edgeless entities,
    // so resolution is a single deterministic pass (joining a cluster does
    // not extend its affinity for later entities).
    let cluster_files: Vec<Vec<&PathBuf>> = clusters
        .iter()
        .map(|m| m.iter().map(|&i| &entities[i].file_path).collect())
        .collect();
    let mut clustered = vec![false; n];
    for &i in clusters.iter().flatten() {
        clustered[i] = true;
    }

    let mut singletons: Vec<usize> = Vec::new();
    for (i, _) in clustered.iter().enumerate().filter(|(_, c)| !**c) {
        match resolve_edgeless(i, entities, &clusters, &cluster_files) {
            Some(c) => clusters[c].push(i),
            None => singletons.push(i),
        }
    }
    // Late joiners keep member order = input order.
    for members in &mut clusters {
        members.sort_unstable();
    }

    if clusters.len() < 2 {
        // A single concern (or none): grouping adds nothing — flat. Still
        // name the verdict for the orientation header.
        return Clustering {
            concern_count: clusters.len(),
            single_concern: clusters.first().map(|m| label_for(m, entities, graph)),
            ..Clustering::default()
        };
    }

    // Clusters sort by max member tier (High first); ties keep the
    // earlier-in-input cluster first. Members and singletons already
    // preserve input order, i.e. the active order mode.
    let mut labeled: Vec<(RiskTier, Vec<usize>)> = clusters
        .into_iter()
        .map(|m| (max_tier(&m, entities), m))
        .collect();
    labeled.sort_by_key(|(tier, m)| (tier.rank(), m[0]));

    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut groups: Vec<GroupSpan> = Vec::with_capacity(labeled.len());
    for (tier, members) in labeled {
        groups.push(GroupSpan {
            label: label_for(&members, entities, graph),
            max_tier: tier,
            start: order.len(),
            len: members.len(),
        });
        order.extend(members);
    }
    order.extend(singletons);
    Clustering {
        concern_count: groups.len(),
        single_concern: None,
        order,
        groups,
    }
}

/// Connected components of size >= 2 over call-graph links (either
/// direction) and Extracted links, in deterministic first-member order.
fn initial_components(entities: &[EntitySummary], graph: &GraphData) -> Vec<Vec<usize>> {
    let n = entities.len();
    let mut uf = UnionFind::new(n);
    let id_to_pos = crate::semantic::entity::index_by_id(entities);

    for edge in &graph.edges {
        if let (Some(&a), Some(&b)) = (id_to_pos.get(&edge.from), id_to_pos.get(&edge.to)) {
            uf.union(a, b);
        }
    }
    // Extract-method links: the new entity and its source are one concern.
    for (i, e) in entities.iter().enumerate() {
        if let Some(RefactorKind::Extracted { from }) = &e.refactor {
            if let Some(&src) = id_to_pos.get(from) {
                uf.union(i, src);
            }
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        components.entry(root).or_default().push(i);
    }
    let mut clusters: Vec<Vec<usize>> = components
        .into_values()
        .filter(|members| members.len() >= 2)
        .collect();
    for members in &mut clusters {
        members.sort_unstable();
    }
    clusters.sort_by_key(|m| m[0]);
    clusters
}

/// File affinity, then nearest member by line distance (spec resolution
/// order). Returns the cluster index to join, or `None` for singleton.
fn resolve_edgeless(
    i: usize,
    entities: &[EntitySummary],
    clusters: &[Vec<usize>],
    cluster_files: &[Vec<&PathBuf>],
) -> Option<usize> {
    let file = &entities[i].file_path;
    let sharing: Vec<usize> = cluster_files
        .iter()
        .enumerate()
        .filter(|(_, files)| files.contains(&file))
        .map(|(c, _)| c)
        .collect();
    match sharing.as_slice() {
        [] => None,
        [only] => Some(*only),
        two_or_more => {
            // Nearest member by line distance within the shared file;
            // deterministic tiebreak: the earlier cluster in render order.
            let line = i64::from(entities[i].line_range.0);
            two_or_more
                .iter()
                .map(|&c| {
                    let nearest = clusters[c]
                        .iter()
                        .filter(|&&m| entities[m].file_path == *file)
                        .map(|&m| (i64::from(entities[m].line_range.0) - line).abs())
                        .min()
                        .unwrap_or(i64::MAX);
                    (nearest, c)
                })
                .min()
                .map(|(_, c)| c)
        }
    }
}

fn max_tier(members: &[usize], entities: &[EntitySummary]) -> RiskTier {
    members
        .iter()
        .map(|&i| {
            entities[i]
                .risk
                .as_ref()
                .map_or(RiskTier::Medium, |r| r.tier)
        })
        .min_by_key(|t| t.rank())
        .unwrap_or(RiskTier::Medium)
}

/// Heuristic group label, in fallback order: shared scope-chain prefix →
/// highest-fanout member's name → dominant file stem. Output passes
/// through `strip_controls` — labels render into the terminal.
fn label_for(members: &[usize], entities: &[EntitySummary], graph: &GraphData) -> String {
    let raw = common_scope_prefix(members, entities)
        .or_else(|| highest_fanout_name(members, entities, graph))
        .unwrap_or_else(|| file_stem(members, entities));
    crate::util::strip_controls(&raw)
}

/// Longest common scope-chain prefix across all members, excluding each
/// member's own leaf name (a prefix equal to an entire chain would just
/// repeat that entity's name).
fn common_scope_prefix(members: &[usize], entities: &[EntitySummary]) -> Option<String> {
    let chains: Vec<&[String]> = members
        .iter()
        .map(|&i| {
            let chain = entities[i].id.scope_chain.as_slice();
            // Exclude the leaf name.
            &chain[..chain.len().saturating_sub(1)]
        })
        .collect();
    let first = chains.first()?;
    let mut len = first.len();
    for chain in &chains[1..] {
        // Cap the scan at the shorter of (current prefix, this chain) BEFORE
        // indexing: `chain[k]` inside the predicate would panic if `k` ran to
        // the previous `len` on a shorter chain that matches all the way.
        let bound = len.min(chain.len());
        len = (0..bound).take_while(|&k| first[k] == chain[k]).count();
    }
    if len == 0 {
        return None;
    }
    Some(first[..len].join("."))
}

/// Name of the member with the most graph edges touching it.
fn highest_fanout_name(
    members: &[usize],
    entities: &[EntitySummary],
    graph: &GraphData,
) -> Option<String> {
    members
        .iter()
        .map(|&i| {
            let id = &entities[i].id;
            let fanout = graph
                .edges
                .iter()
                .filter(|e| &e.from == id || &e.to == id)
                .count();
            (fanout, i)
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .filter(|(fanout, _)| *fanout > 0)
        .map(|(_, i)| entities[i].display_name.clone())
}

/// Stem of the first member's file — the fallback of last resort.
fn file_stem(members: &[usize], entities: &[EntitySummary]) -> String {
    members
        .first()
        .and_then(|&i| entities[i].file_path.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "group".to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::semantic::cache::GraphEdge;
    use crate::semantic::entity::{ChangeAnnotation, ChangeType, EntityKind};
    use crate::semantic::risk::RiskAssessment;
    use crate::semantic::EntityId;

    fn entity(file: &str, chain: &[&str], line: u32, tier: RiskTier) -> EntitySummary {
        let id = EntityId::new(
            PathBuf::from(file),
            chain.iter().map(|s| (*s).to_owned()).collect(),
            None,
            0,
        );
        EntitySummary {
            display_name: id.display_name(),
            id,
            kind: EntityKind::Function,
            change: ChangeType::Modified,
            annotation: ChangeAnnotation::BodyOnly,
            file_path: PathBuf::from(file),
            source_file: None,
            target_line: None,
            line_range: (line, line + 5),
            structural_change: true,
            content_hash: 1,
            refactor: None,
            comment_count: 0,
            reviewed: false,
            risk: Some(RiskAssessment {
                tier,
                clause: String::new(),
            }),
            fallback: false,
        }
    }

    fn edge(entities: &[EntitySummary], from: usize, to: usize) -> GraphEdge {
        GraphEdge {
            from: entities[from].id.clone(),
            to: entities[to].id.clone(),
            call_sites: vec![1],
        }
    }

    fn graph(edges: Vec<GraphEdge>) -> GraphData {
        GraphData {
            nodes: Vec::new(),
            edges,
            unresolved: Vec::new(),
        }
    }

    #[test]
    fn two_concerns_yield_two_groups_ordered_by_max_tier() {
        // Concern A (Medium): a0 → a1. Concern B (High): b0 → b1.
        // A comes first in input; B must still sort first (High).
        let entities = vec![
            entity("a.rs", &["a0"], 1, RiskTier::Medium),
            entity("a.rs", &["a1"], 10, RiskTier::Low),
            entity("b.rs", &["b0"], 1, RiskTier::High),
            entity("b.rs", &["b1"], 10, RiskTier::Medium),
        ];
        let g = graph(vec![edge(&entities, 0, 1), edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        assert_eq!(c.groups.len(), 2);
        assert_eq!(c.groups[0].max_tier, RiskTier::High);
        assert_eq!(c.groups[0].start, 0);
        assert_eq!(c.groups[0].len, 2);
        assert_eq!(c.groups[1].max_tier, RiskTier::Medium);
        // Permutation: B's members first, then A's.
        let names: Vec<&str> = c.order.iter().map(|&i| entities[i].id.name()).collect();
        assert_eq!(names, ["b0", "b1", "a0", "a1"]);
    }

    #[test]
    fn extracted_link_joins_new_entity_to_its_source() {
        let mut entities = vec![
            entity("a.rs", &["source_fn"], 1, RiskTier::Medium),
            entity("a.rs", &["helper"], 40, RiskTier::Low),
            entity("b.rs", &["other0"], 1, RiskTier::Medium),
            entity("b.rs", &["other1"], 9, RiskTier::Medium),
        ];
        entities[1].refactor = Some(RefactorKind::Extracted {
            from: entities[0].id.clone(),
        });
        // No call edge between source_fn and helper — only the Extracted
        // link can join them. other0/other1 form the second cluster.
        let g = graph(vec![edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        assert_eq!(c.groups.len(), 2);
        let first_group: Vec<&str> = c.order
            [c.groups[0].start..c.groups[0].start + c.groups[0].len]
            .iter()
            .map(|&i| entities[i].id.name())
            .collect();
        assert!(
            first_group.contains(&"source_fn") && first_group.contains(&"helper"),
            "Extracted pair must cluster together: {first_group:?}"
        );
    }

    #[test]
    fn edgeless_entity_joins_unique_file_affinity_cluster() {
        let entities = vec![
            entity("a.rs", &["a0"], 1, RiskTier::Medium),
            entity("a.rs", &["a1"], 10, RiskTier::Medium),
            entity("b.rs", &["b0"], 1, RiskTier::Medium),
            entity("b.rs", &["b1"], 10, RiskTier::Medium),
            entity("a.rs", &["lonely"], 90, RiskTier::Low),
        ];
        let g = graph(vec![edge(&entities, 0, 1), edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        let group_of_lonely = c
            .groups
            .iter()
            .find(|gr| {
                c.order[gr.start..gr.start + gr.len]
                    .iter()
                    .any(|&i| entities[i].id.name() == "lonely")
            })
            .expect("lonely must join a cluster");
        let members: Vec<&str> = c.order[group_of_lonely.start..]
            .iter()
            .take(group_of_lonely.len)
            .map(|&i| entities[i].id.name())
            .collect();
        assert!(members.contains(&"a0"), "must join the a.rs cluster");
    }

    #[test]
    fn edgeless_entity_with_two_sharing_clusters_joins_nearest_by_line() {
        // Both clusters have members in shared.rs; the edgeless entity at
        // line 100 is nearest to cluster B's member at line 90.
        let entities = vec![
            entity("shared.rs", &["a_member"], 1, RiskTier::Medium),
            entity("x.rs", &["a_other"], 1, RiskTier::Medium),
            entity("shared.rs", &["b_member"], 90, RiskTier::Medium),
            entity("y.rs", &["b_other"], 1, RiskTier::Medium),
            entity("shared.rs", &["lonely"], 100, RiskTier::Low),
        ];
        let g = graph(vec![edge(&entities, 0, 1), edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        let group_of_lonely = c
            .groups
            .iter()
            .find(|gr| {
                c.order[gr.start..gr.start + gr.len]
                    .iter()
                    .any(|&i| entities[i].id.name() == "lonely")
            })
            .expect("lonely must join a cluster");
        let members: Vec<&str> = c.order[group_of_lonely.start..]
            .iter()
            .take(group_of_lonely.len)
            .map(|&i| entities[i].id.name())
            .collect();
        assert!(
            members.contains(&"b_member"),
            "line 100 is nearest to b_member at 90: {members:?}"
        );
    }

    #[test]
    fn no_graph_or_single_cluster_renders_flat() {
        let entities = vec![
            entity("a.rs", &["a0"], 1, RiskTier::Medium),
            entity("a.rs", &["a1"], 10, RiskTier::Medium),
        ];
        // No graph → flat.
        let c = cluster_entities(&entities, None);
        assert!(c.groups.is_empty());
        // One cluster → flat (grouping adds nothing), but the verdict is
        // still named for the orientation header.
        let g = graph(vec![edge(&entities, 0, 1)]);
        let c = cluster_entities(&entities, Some(&g));
        assert!(c.groups.is_empty());
        assert_eq!(c.concern_count, 1);
        assert!(c.single_concern.is_some(), "single concern must be labeled");
    }

    #[test]
    fn concern_count_matches_group_count_when_grouped() {
        let entities = vec![
            entity("a.rs", &["a0"], 1, RiskTier::Medium),
            entity("a.rs", &["a1"], 10, RiskTier::Medium),
            entity("b.rs", &["b0"], 1, RiskTier::Medium),
            entity("b.rs", &["b1"], 10, RiskTier::Medium),
        ];
        let g = graph(vec![edge(&entities, 0, 1), edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        assert_eq!(c.concern_count, 2);
        assert_eq!(c.single_concern, None);
        // No graph → no verdict.
        let c = cluster_entities(&entities, None);
        assert_eq!(c.concern_count, 0);
    }

    #[test]
    fn labels_prefer_scope_prefix_then_fanout_then_file_stem() {
        // Shared scope prefix.
        let entities = vec![
            entity("a.rs", &["Session", "refresh"], 1, RiskTier::Medium),
            entity("a.rs", &["Session", "validate"], 10, RiskTier::Medium),
            entity("b.rs", &["free_a"], 1, RiskTier::Medium),
            entity("b.rs", &["free_b"], 10, RiskTier::Medium),
        ];
        let g = graph(vec![edge(&entities, 0, 1), edge(&entities, 2, 3)]);
        let c = cluster_entities(&entities, Some(&g));
        let labels: Vec<&str> = c.groups.iter().map(|gr| gr.label.as_str()).collect();
        assert!(
            labels.contains(&"Session"),
            "scope prefix label: {labels:?}"
        );
        // The free-function cluster has no shared prefix; highest fanout
        // is free_b (callee of the only edge counts 1, caller counts 1 —
        // tie broken toward the earlier member, free_a).
        assert!(
            labels.contains(&"free_a") || labels.contains(&"free_b"),
            "fanout label: {labels:?}"
        );
    }

    /// Regression (PR #76 review): a later member whose (leaf-excluded)
    /// scope chain is shorter than the running prefix AND matches it to
    /// its full length must shrink the prefix — not index past its end.
    /// Previously `(0..len)` used the pre-min `len`, so `chain[k]`
    /// panicked on exactly this shape.
    #[test]
    fn common_scope_prefix_shorter_matching_chain_does_not_panic() {
        let entities = vec![
            entity("a.rs", &["Session", "Auth", "refresh"], 1, RiskTier::Medium),
            entity("a.rs", &["Session", "validate"], 20, RiskTier::Medium),
        ];
        let prefix = common_scope_prefix(&[0, 1], &entities);
        assert_eq!(prefix.as_deref(), Some("Session"));
    }

    #[test]
    fn singletons_render_after_labeled_clusters_in_input_order() {
        let entities = vec![
            entity("z.rs", &["island"], 5, RiskTier::High),
            entity("a.rs", &["a0"], 1, RiskTier::Medium),
            entity("a.rs", &["a1"], 10, RiskTier::Medium),
            entity("b.rs", &["b0"], 1, RiskTier::Medium),
            entity("b.rs", &["b1"], 10, RiskTier::Medium),
        ];
        let g = graph(vec![edge(&entities, 1, 2), edge(&entities, 3, 4)]);
        let c = cluster_entities(&entities, Some(&g));
        assert_eq!(c.groups.len(), 2);
        // island shares no file with any cluster → singleton at the end,
        // even though it is High tier.
        assert_eq!(
            entities[*c.order.last().expect("nonempty")].id.name(),
            "island"
        );
    }
}
