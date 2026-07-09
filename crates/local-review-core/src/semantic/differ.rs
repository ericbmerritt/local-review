//! Entity diff computation: given before and after `RawEntity` lists, produce
//! the `EntityCoreData` list that reflects what changed.
//!
//! Implements the Container Rule: a container entity appears in the output only
//! when its own declaration changed. If only its children changed, the
//! children appear and the container is suppressed.

use std::path::PathBuf;

use crate::semantic::entity::{
    ChangeAnnotation, ChangeType, EntityCoreData, EntityId, EntityKind, RawEntity, RefactorKind,
};
use crate::semantic::identity::{
    annotation, find_extraction_source, is_structural_change, match_entities,
};

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
        refactor: None,
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
        refactor: None,
        line_range: (e.start_line, e.end_line),
        source_file: None,
        target_line: Some(e.start_line),
        structural_change: true,
        content_hash: e.content_hash,
    }
}

fn make_modified(be: &RawEntity, ae: &RawEntity) -> EntityCoreData {
    // A matched same-file pair whose scope-chain tail differs is a rename.
    // Purity is decided by whole-content substitution: only when swapping the
    // new name for the old across the entire before-state reproduces the
    // after-state exactly did nothing but the name change. Any params /
    // return-type / visibility / body edit fails the equality, and substring
    // over-replacement also fails — conservatively toward not demoting.
    let refactor = match (be.id.scope_chain.last(), ae.id.scope_chain.last()) {
        (Some(old), Some(new)) if old != new => {
            let pure = be.content.replace(old.as_str(), new) == ae.content;
            Some(RefactorKind::Renamed {
                from: old.clone(),
                pure,
            })
        }
        _ => None,
    };
    EntityCoreData {
        id: ae.id.clone(),
        kind: ae.kind,
        change: ChangeType::Modified,
        annotation: annotation(be, ae),
        refactor,
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
        refactor: Some(RefactorKind::Moved {
            identical: be.content_hash == ae.content_hash,
        }),
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
        let mut core = make_added(e);
        // Extract-method detection: an added entity whose tokens are largely
        // contained in the removed span of a shrunken same-file sibling.
        core.refactor =
            find_extraction_source(e, &mr.matched).map(|from| RefactorKind::Extracted { from });
        result.push(core);
    }
    for e in &mr.deleted {
        result.push(make_deleted(e));
    }

    apply_container_rule(&mut result, &mr.matched);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        file: &str,
        name: &str,
        content: &str,
        hashes: (u64, u64, u64), // (content, sig, body)
    ) -> RawEntity {
        RawEntity {
            id: EntityId::new(PathBuf::from(file), vec![name.to_owned()], None, 0),
            scope: name.to_owned(),
            kind: EntityKind::Function,
            start_line: 1,
            end_line: 10,
            content: content.to_owned(),
            content_hash: hashes.0,
            sig_hash: hashes.1,
            body_hash: hashes.2,
            file_path: file.to_owned(),
        }
    }

    #[test]
    fn rename_only_is_behavior_preserving() {
        // Same file, identical content hash (hash_match pairs them), name
        // differs, body hash unchanged: sig-only rename.
        let before = vec![raw(
            "a.rs",
            "old_name",
            "fn old_name() { work() }",
            (7, 1, 9),
        )];
        let after = vec![raw(
            "a.rs",
            "new_name",
            "fn new_name() { work() }",
            (7, 2, 9),
        )];
        let out = diff_entities(&before, &after);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].refactor,
            Some(RefactorKind::Renamed {
                from: "old_name".to_owned(),
                pure: true
            })
        );
        assert!(
            out[0].is_behavior_preserving(),
            "sig-only rename must be behavior-preserving"
        );
    }

    #[test]
    fn rename_with_body_change_is_not_demoted() {
        // Fuzzy-matched (shared tokens ≥ threshold), name AND body differ.
        let body = "let session = fetch_session(token); let expiry = session.expiry; \
                    if expiry < now { return Err(Expired) } audit_log(session); \
                    refresh_budget(session); Ok(session)";
        let before = vec![raw(
            "a.rs",
            "old_name",
            &format!("fn old_name() {{ {body} }}"),
            (7, 1, 9),
        )];
        let after = vec![raw(
            "a.rs",
            "new_name",
            &format!("fn new_name() {{ {body} retry_once(); }}"),
            (8, 2, 10),
        )];
        let out = diff_entities(&before, &after);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].refactor, Some(RefactorKind::Renamed { ref from, .. }) if from == "old_name"),
            "must classify as rename; got {:?}",
            out[0].refactor
        );
        assert!(
            !out[0].is_behavior_preserving(),
            "rename + body change must NOT be behavior-preserving"
        );
    }

    #[test]
    fn rename_with_param_change_is_not_demoted() {
        // Name AND parameter list change; body identical. Whole-content
        // substitution fails on the extra param, so the rename is impure —
        // a meaningful API edit must not be demoted as behavior-preserving.
        // Body is long enough that the signature delta stays under the fuzzy
        // Jaccard threshold and the pair still matches.
        let body = "let session = fetch_session(token); let expiry = session.expiry; \
                    if expiry < now { return Err(Expired) } audit_log(session); \
                    metrics.observe(latency); tracing::debug(request_id); \
                    cache.invalidate(token); budget.charge(actor); \
                    guard.release(); span.exit(); pool.recycle(conn); \
                    stats.flush(elapsed); ledger.commit(entry); \
                    refresh_budget(session); Ok(session)";
        let before = vec![raw(
            "a.rs",
            "old_name",
            &format!("fn old_name(a: u32) {{ {body} }}"),
            (7, 1, 9),
        )];
        let after = vec![raw(
            "a.rs",
            "new_name",
            &format!("fn new_name(a: u32, b: bool) {{ {body} }}"),
            (8, 2, 9),
        )];
        let out = diff_entities(&before, &after);
        assert_eq!(out.len(), 1);
        assert!(
            matches!(
                out[0].refactor,
                Some(RefactorKind::Renamed { pure: false, .. })
            ),
            "rename + param change must classify as impure rename; got {:?}",
            out[0].refactor
        );
        assert!(
            !out[0].is_behavior_preserving(),
            "param change is a meaningful signature edit — never demoted"
        );
    }

    #[test]
    fn extract_method_true_positive() {
        // `validate` shrinks; the removed tokens reappear as `is_expired`.
        let extracted_body = "let expiry = session.expiry_timestamp; \
                              let margin = config.expiry_margin_secs; \
                              expiry.saturating_add(margin) < clock.now_epoch_secs()";
        let before = vec![raw(
            "a.rs",
            "validate",
            &format!("fn validate() {{ check_sig(); {extracted_body} }}"),
            (1, 1, 1),
        )];
        let after = vec![
            raw(
                "a.rs",
                "validate",
                "fn validate() { check_sig(); is_expired() }",
                (2, 1, 2),
            ),
            raw(
                "a.rs",
                "is_expired",
                &format!("fn is_expired() {{ {extracted_body} }}"),
                (3, 3, 3),
            ),
        ];
        let out = diff_entities(&before, &after);
        let added = out
            .iter()
            .find(|e| e.change == ChangeType::Added)
            .expect("added entity present");
        assert!(
            matches!(
                &added.refactor,
                Some(RefactorKind::Extracted { from }) if from.name() == "validate"
            ),
            "added entity must be tagged extracted ← validate; got {:?}",
            added.refactor
        );
        assert!(added.is_behavior_preserving());
    }

    #[test]
    fn extract_false_positive_coincidental_similarity() {
        // Sibling shrinks, but the added entity's tokens are unrelated new
        // logic — containment stays below threshold, no Extracted tag.
        let before = vec![raw(
            "a.rs",
            "validate",
            "fn validate() { check_sig(); let expiry = session.expiry_timestamp; \
             let margin = config.expiry_margin_secs; expiry < now() }",
            (1, 1, 1),
        )];
        let after = vec![
            raw(
                "a.rs",
                "validate",
                "fn validate() { check_sig() }",
                (2, 1, 2),
            ),
            raw(
                "a.rs",
                "audit_request",
                "fn audit_request() { let entry = AuditEntry::new(actor, verb, target); \
                 entry.stamp(request_id); sink.write_entry(entry); metrics.incr_audit_total() }",
                (3, 3, 3),
            ),
        ];
        let out = diff_entities(&before, &after);
        let added = out
            .iter()
            .find(|e| e.change == ChangeType::Added)
            .expect("added entity present");
        assert_eq!(
            added.refactor, None,
            "coincidental similarity must not tag an extraction"
        );
    }

    #[test]
    fn identical_cross_file_move_is_behavior_preserving() {
        let before = vec![raw(
            "a.rs",
            "helper",
            "fn helper() { do_the_thing() }",
            (7, 1, 9),
        )];
        let after = vec![raw(
            "b.rs",
            "helper",
            "fn helper() { do_the_thing() }",
            (7, 1, 9),
        )];
        let out = diff_entities(&before, &after);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].change, ChangeType::Moved);
        assert_eq!(
            out[0].refactor,
            Some(RefactorKind::Moved { identical: true })
        );
        assert!(out[0].is_behavior_preserving());
    }
}
