//! Cross-file dependency graph builder.
//!
//! Given a set of source files and the workspace's extractor registry,
//! `build_graph` returns a `GraphData` with one node per extracted entity
//! and one edge per resolved `caller → callee` relationship. Resolution is
//! by **leaf name only** for v1: every entity whose `EntityId::name()`
//! matches the call-site callee text becomes a candidate target. This is
//! intentionally permissive — same-name entities in different modules all
//! get edges. For the "deps / dependents" Claude bundle that consumes the
//! graph, a slight over-shoot is preferable to dropping real dependents,
//! and the bundle's token budget caps the blast radius.
//!
//! The builder is best-effort. Files that fail to parse, files outside the
//! registered language set, or files unreadable on disk are silently
//! skipped — a missing file degrades the graph rather than blocking it. If
//! the input set produces zero edges (e.g. a repo of only YAML), the
//! returned `GraphData` is `{ nodes, edges: [] }` and the bundle simply
//! omits the deps / dependents sections.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::semantic::cache::{GraphData, GraphEdge, GraphNode};
use crate::semantic::entity::{EntityId, EntityKind};
use crate::semantic::registry::ExtractorRegistry;

/// Build the call graph over the given source files. `repo_root` is joined
/// with each `files` entry to locate the on-disk source. Paths in the
/// returned `EntityId`s are the same `files` paths (relative or whatever
/// the caller passed in) — consistent with the rest of the extraction
/// pipeline, which stores repo-relative paths.
pub fn build_graph(registry: &ExtractorRegistry, repo_root: &Path, files: &[PathBuf]) -> GraphData {
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut by_name: HashMap<String, Vec<EntityId>> = HashMap::new();
    // Vec rather than HashMap because we scan linearly per call site and
    // per-file sizes are small enough that the constant factor wins.
    let mut entities_by_file: HashMap<PathBuf, Vec<(EntityId, EntityKind, u32, u32)>> =
        HashMap::new();
    let mut calls_by_file: HashMap<PathBuf, Vec<(u32, String)>> = HashMap::new();

    for rel in files {
        let abs = repo_root.join(rel);
        let Ok(content) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        if let Ok(raw_entities) = registry.extract(&content, &rel_str) {
            for raw in &raw_entities {
                nodes.push(GraphNode {
                    id: raw.id.clone(),
                    kind: raw.kind,
                });
                by_name
                    .entry(raw.id.name().to_owned())
                    .or_default()
                    .push(raw.id.clone());
                entities_by_file.entry(rel.clone()).or_default().push((
                    raw.id.clone(),
                    raw.kind,
                    raw.start_line,
                    raw.end_line,
                ));
            }
        }
        let calls = registry.extract_calls(&content, &rel_str);
        if !calls.is_empty() {
            calls_by_file.insert(
                rel.clone(),
                calls.into_iter().map(|c| (c.line, c.callee_name)).collect(),
            );
        }
    }

    // Ambiguous matches (multiple entities with the same leaf name) all get
    // edges — the bundle's budget handles pruning. Unresolved names (no
    // matching entity in the repo) are silently dropped.
    let mut seen: std::collections::HashSet<(EntityId, EntityId)> =
        std::collections::HashSet::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    for (path, calls) in &calls_by_file {
        let Some(file_entities) = entities_by_file.get(path) else {
            continue;
        };
        for (line, callee) in calls {
            let Some(caller) = containing_entity(file_entities, *line) else {
                // Top-level call; skip — there's no enclosing entity to
                // attribute the edge to and top-level glue rarely helps.
                continue;
            };
            let Some(targets) = by_name.get(callee) else {
                continue;
            };
            for target in targets {
                // Don't emit self-edges; recursive functions aren't useful
                // context in the bundle and bloat the edge count.
                if target == caller {
                    continue;
                }
                // Deduplicate: a function that calls bar() three times only
                // needs one edge from caller → bar in the bundle.
                if seen.insert((caller.clone(), target.clone())) {
                    edges.push(GraphEdge {
                        from: caller.clone(),
                        to: target.clone(),
                    });
                }
            }
        }
    }

    GraphData { nodes, edges }
}

/// Return the `EntityId` of the smallest entity in `file_entities` whose
/// line range contains `line`. When entities nest (an `impl` block
/// containing a method), the method's range is narrower so it wins, which
/// matches what the reviewer expects: a call inside `Foo::bar` is
/// attributed to `bar`, not to the surrounding `impl Foo`.
fn containing_entity(
    file_entities: &[(EntityId, EntityKind, u32, u32)],
    line: u32,
) -> Option<&EntityId> {
    let mut best: Option<&(EntityId, EntityKind, u32, u32)> = None;
    for entry in file_entities {
        let (_, _, start, end) = entry;
        if line < *start || line > *end {
            continue;
        }
        match best {
            None => best = Some(entry),
            Some((_, _, bs, be)) if (end - start) < (be - bs) => best = Some(entry),
            Some(_) => {}
        }
    }
    best.map(|(id, _, _, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::create_default_registry;
    use std::fs;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).expect("write fixture");
        PathBuf::from(name)
    }

    #[test]
    fn rust_inter_function_call_yields_edge() {
        // `caller` invokes `callee`; the graph must have one edge from
        // caller's EntityId to callee's EntityId.
        let dir = tempfile::tempdir().unwrap();
        let rel = write_file(
            dir.path(),
            "lib.rs",
            "fn callee() -> i32 { 1 }\nfn caller() -> i32 { callee() + 1 }\n",
        );
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[rel]);
        assert_eq!(graph.nodes.len(), 2, "two functions => two nodes");
        assert_eq!(
            graph.edges.len(),
            1,
            "one call => one edge; got {:?}",
            graph.edges
        );
        let edge = &graph.edges[0];
        assert_eq!(edge.from.name(), "caller");
        assert_eq!(edge.to.name(), "callee");
    }

    #[test]
    fn rust_call_outside_any_entity_is_skipped() {
        // A bare call at module top level has no containing entity. We
        // intentionally drop it rather than synthesize a module-level
        // pseudo-entity; the reviewer's bundle is about entity-scoped
        // dependencies.
        let dir = tempfile::tempdir().unwrap();
        let rel = write_file(dir.path(), "lib.rs", "fn target() {}\ntarget();\n");
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[rel]);
        assert!(
            graph.edges.is_empty(),
            "top-level call must not emit an edge; got {:?}",
            graph.edges
        );
    }

    #[test]
    fn rust_method_call_resolves_to_leaf_name() {
        // `obj.method()` resolves to a target named `method`. With no
        // type-aware resolution, every entity called `method` becomes a
        // candidate. Here only one exists, so we get exactly one edge.
        let dir = tempfile::tempdir().unwrap();
        let rel = write_file(
            dir.path(),
            "lib.rs",
            "struct Foo;\nimpl Foo {\n    fn method(&self) -> i32 { 1 }\n}\nfn caller(f: &Foo) -> i32 { f.method() }\n",
        );
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[rel]);
        let method_edges: Vec<&GraphEdge> = graph
            .edges
            .iter()
            .filter(|e| e.to.name() == "method")
            .collect();
        assert!(
            !method_edges.is_empty(),
            "expected an edge into `method`; got {:?}",
            graph.edges
        );
    }

    #[test]
    fn rust_self_call_does_not_emit_self_edge() {
        // Recursive functions shouldn't bloat the edge list with a noisy
        // self-loop that the bundle would then have to filter out.
        let dir = tempfile::tempdir().unwrap();
        let rel = write_file(
            dir.path(),
            "lib.rs",
            "fn factorial(n: u64) -> u64 { if n <= 1 { 1 } else { n * factorial(n - 1) } }\n",
        );
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[rel]);
        assert!(
            graph.edges.iter().all(|e| e.from != e.to),
            "self-edge present in {:?}",
            graph.edges
        );
    }

    #[test]
    fn unresolvable_callee_is_silently_dropped() {
        // `external_fn` has no matching entity in the repo. The edge is
        // skipped — there's no graph node to point at.
        let dir = tempfile::tempdir().unwrap();
        let rel = write_file(dir.path(), "lib.rs", "fn caller() { external_fn(); }\n");
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[rel]);
        assert_eq!(graph.nodes.len(), 1);
        assert!(
            graph.edges.is_empty(),
            "unresolved callee must not emit edge"
        );
    }

    #[test]
    fn ambiguous_callee_emits_edge_to_every_candidate() {
        // Two entities named `parse` in different files; one caller in a
        // third file. Both become candidates — the bundle decides which to
        // include. Better to over-include than under-resolve when the v1
        // resolver is leaf-name-only.
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.rs", "pub fn parse() -> i32 { 1 }\n");
        let b = write_file(dir.path(), "b.rs", "pub fn parse() -> i32 { 2 }\n");
        let c = write_file(
            dir.path(),
            "c.rs",
            "use crate::a;\nfn caller() -> i32 { a::parse() }\n",
        );
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[a, b, c]);
        let parse_edges: Vec<&GraphEdge> = graph
            .edges
            .iter()
            .filter(|e| e.to.name() == "parse")
            .collect();
        assert_eq!(
            parse_edges.len(),
            2,
            "ambiguous leaf name `parse` must emit one edge per candidate; got {:?}",
            graph.edges
        );
    }

    #[test]
    fn missing_file_on_disk_is_skipped_not_errored() {
        // The graph builder is best-effort: a path that doesn't exist on
        // disk (deleted between extraction and graph build, say) silently
        // contributes nothing rather than failing the whole graph build.
        let dir = tempfile::tempdir().unwrap();
        let registry = create_default_registry();
        let graph = build_graph(&registry, dir.path(), &[PathBuf::from("ghost.rs")]);
        assert_eq!(graph.nodes.len(), 0);
        assert_eq!(graph.edges.len(), 0);
    }
}
