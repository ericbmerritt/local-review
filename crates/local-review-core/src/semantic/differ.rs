//! Entity diff computation: given before and after `RawEntity` lists, produce
//! the `EntityCoreData` list that reflects what changed.
//!
//! Implements the Container Rule: a container entity appears in the output only
//! when its own declaration changed. If only its children changed, the
//! children appear and the container is suppressed.

use std::path::PathBuf;

use crate::semantic::entity::{
    ChangeAnnotation, ChangeType, EntityCoreData, EntityKind, PlaceholderEntityId, RawEntity,
};
use crate::semantic::identity::{annotation, is_structural_change, match_entities};

// ── Container kind check ─────────────────────────────────────────────────────

fn is_container(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Class
            | EntityKind::Struct
            | EntityKind::Enum
            | EntityKind::Trait
            | EntityKind::Interface
            | EntityKind::Module
    )
}

// ── EntityCoreData construction ──────────────────────────────────────────────

fn make_added(e: &RawEntity) -> EntityCoreData {
    EntityCoreData {
        id: PlaceholderEntityId(e.id_str.clone()),
        kind: e.kind,
        change: ChangeType::Added,
        annotation: ChangeAnnotation::None,
        line_range: (e.start_line, e.end_line),
        source_file: None,
        target_line: Some(e.start_line),
        structural_change: true,
        content_hash: e.content_hash,
    }
}

fn make_deleted(e: &RawEntity) -> EntityCoreData {
    EntityCoreData {
        id: PlaceholderEntityId(e.id_str.clone()),
        kind: e.kind,
        change: ChangeType::Deleted,
        annotation: ChangeAnnotation::None,
        line_range: (e.start_line, e.end_line),
        source_file: None,
        target_line: Some(e.start_line),
        structural_change: true,
        content_hash: e.content_hash,
    }
}

fn make_modified(be: &RawEntity, ae: &RawEntity) -> EntityCoreData {
    EntityCoreData {
        id: PlaceholderEntityId(ae.id_str.clone()),
        kind: ae.kind,
        change: ChangeType::Modified,
        annotation: annotation(be, ae),
        line_range: (ae.start_line, ae.end_line),
        source_file: None,
        target_line: Some(ae.start_line),
        structural_change: is_structural_change(be, ae),
        content_hash: ae.content_hash,
    }
}

fn make_moved(be: &RawEntity, ae: &RawEntity) -> EntityCoreData {
    EntityCoreData {
        id: PlaceholderEntityId(ae.id_str.clone()),
        kind: ae.kind,
        change: ChangeType::Moved,
        annotation: ChangeAnnotation::None,
        line_range: (ae.start_line, ae.end_line),
        source_file: Some(PathBuf::from(&be.file_path)),
        target_line: Some(ae.start_line),
        structural_change: be.file_path != ae.file_path,
        content_hash: ae.content_hash,
    }
}

// ── Container Rule suppression ────────────────────────────────────────────────

/// True when an entity whose result id-string uses `child_scope` within
/// `container_file` is a direct or transitive child of the given container.
///
/// The scope comparison uses `RawEntity::scope` (just the `::` -separated
/// name chain, e.g., `Session::refresh`) rather than the full `id_str`.
/// This correctly handles containers and children having different `<kind>`
/// segments in their `id_str`.
fn scope_is_child(child_scope: &str, child_file: &str, container: &RawEntity) -> bool {
    child_file == container.file_path
        && child_scope.starts_with(&container.scope)
        && child_scope.len() > container.scope.len()
        && child_scope.as_bytes().get(container.scope.len()).copied() == Some(b':')
}

/// Extract the scope and file from a result entity's `PlaceholderEntityId`.
///
/// Format is `<file>::<kind>::<scope>`. Splits on the first two `::` pairs.
fn id_parts(id_str: &str) -> (&str, &str) {
    let mut parts = id_str.splitn(3, "::");
    let file = parts.next().unwrap_or("");
    let _ = parts.next(); // kind — not needed
    let scope = parts.next().unwrap_or("");
    (file, scope)
}

/// Remove container entities from `result` that have a body-only annotation
/// and at least one changed child (added, deleted, or modified) in the result.
/// Called after all entities are in `result` so added/deleted children are
/// visible — not just modified ones.
fn apply_container_rule(
    result: &mut Vec<EntityCoreData>,
    raw_modified: &[(&RawEntity, &RawEntity)],
) {
    let to_suppress: Vec<String> = raw_modified
        .iter()
        .filter(|(be, ae)| {
            is_container(ae.kind) && annotation(be, ae) == ChangeAnnotation::BodyOnly
        })
        .filter(|(be, _)| {
            result.iter().any(|e| {
                let (file, scope) = id_parts(&e.id.0);
                scope_is_child(scope, file, be)
            })
        })
        .map(|(be, _)| be.id_str.clone())
        .collect();

    result.retain(|e| !to_suppress.contains(&e.id.0));
}

// ── Public diff entry point ───────────────────────────────────────────────────

/// Compute the list of entity-level changes from before and after entity lists.
///
/// Returns `EntityCoreData` entries only for entities that actually changed.
/// Unchanged entities (same id, same content hash) are not included.
pub fn diff_entities(before: &[RawEntity], after: &[RawEntity]) -> Vec<EntityCoreData> {
    let mr = match_entities(before, after);
    let mut result = Vec::new();

    for (be, ae) in &mr.matched {
        if be.content_hash != ae.content_hash || be.id_str != ae.id_str {
            if be.file_path == ae.file_path {
                result.push(make_modified(be, ae));
            } else {
                result.push(make_moved(be, ae));
            }
        }
    }
    for e in &mr.added {
        result.push(make_added(e));
    }
    for e in &mr.deleted {
        result.push(make_deleted(e));
    }

    apply_container_rule(&mut result, &mr.matched);
    result
}
