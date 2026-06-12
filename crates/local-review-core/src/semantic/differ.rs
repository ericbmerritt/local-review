//! Entity diff computation: given before and after `RawEntity` lists, produce
//! the `EntityCoreData` list that reflects what changed.
//!
//! Implements the Container Rule: a container entity appears in the output only
//! when its own declaration changed. If only its children changed, the
//! children appear and the container is suppressed.

use std::path::PathBuf;

use crate::semantic::entity::{
    ChangeAnnotation, ChangeType, EntityCoreData, EntityId, EntityKind, RawEntity,
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
            | EntityKind::Section   // markdown heading section
            | EntityKind::TestSuite // describe() block
    )
}

// ── EntityCoreData construction ──────────────────────────────────────────────

fn make_added(e: &RawEntity) -> EntityCoreData {
    EntityCoreData {
        id: e.id.clone(),
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
        id: e.id.clone(),
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
        id: ae.id.clone(),
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
        id: ae.id.clone(),
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

/// True when `child` is a direct or transitive descendant of `container`
/// by scope-chain prefix match within the same file.
fn entity_is_child_of(child: &EntityCoreData, container: &EntityCoreData) -> bool {
    child.id.file_path == container.id.file_path
        && child
            .id
            .scope_chain
            .starts_with(container.id.scope_chain.as_slice())
        && child.id.scope_chain.len() > container.id.scope_chain.len()
}

/// Variant used in the Modified-container path where the container identity
/// comes from the before-state `RawEntity`.
fn entity_is_child(child: &EntityCoreData, container: &RawEntity) -> bool {
    child.id.file_path == container.id.file_path
        && child
            .id
            .scope_chain
            .starts_with(container.id.scope_chain.as_slice())
        && child.id.scope_chain.len() > container.id.scope_chain.len()
}

/// Remove redundant entities from `result` so the entity list shows the
/// most-useful unit of review for each change type.
///
/// Two cases, opposite directions:
///
/// 1. **Modified containers** with body-only changes are suppressed in favor
///    of their changed children. If only `Foo::bar()`'s body changed, the
///    reviewer wants `bar` in the list, not `Foo`. The most-specific changed
///    entity is the useful one.
///
/// 2. **Children of Added/Deleted containers** are suppressed in favor of
///    the parent. Adding `impl Deref for BuildDate { type Target = str; fn
///    deref(...) }` is one logical change; the reviewer wants the impl
///    block in the list, not separate rows for `Target` and `deref`. The
///    broadest added/deleted entity is the useful one.
///
/// Called after all entities are in `result` so added/deleted/modified
/// children are already present.
fn apply_container_rule(
    result: &mut Vec<EntityCoreData>,
    raw_modified: &[(&RawEntity, &RawEntity)],
) {
    // Case 1: Modified containers whose only change is in their children.
    let modified_suppress: Vec<EntityId> = raw_modified
        .iter()
        .filter(|(be, ae)| {
            is_container(ae.kind) && annotation(be, ae) == ChangeAnnotation::BodyOnly
        })
        .filter(|(be, _)| result.iter().any(|e| entity_is_child(e, be)))
        .map(|(be, _)| be.id.clone())
        .collect();

    // Case 2: Added/Deleted children inside an Added/Deleted container of the
    // same change type. Suppress the children — the parent already conveys
    // "this entire block is new/gone."
    let added_deleted_child_suppress: Vec<EntityId> = result
        .iter()
        .filter(|child| matches!(child.change, ChangeType::Added | ChangeType::Deleted))
        .filter(|child| {
            result.iter().any(|parent| {
                parent.id != child.id
                    && parent.change == child.change
                    && is_container(parent.kind)
                    && entity_is_child_of(child, parent)
            })
        })
        .map(|c| c.id.clone())
        .collect();

    result.retain(|e| {
        !modified_suppress.contains(&e.id) && !added_deleted_child_suppress.contains(&e.id)
    });
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
        if be.content_hash != ae.content_hash || be.id != ae.id {
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
